//! Deserializable argument structs for the three MCP tools, plus the leniency
//! shim `job`'s `action` needs. Split out of `mod.rs` to keep the dispatcher
//! (the tool bodies + presentation) focused on behavior, not schema shape.

// ---- bash ----

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct BashArgs {
    /// Shell command to run.
    pub cmd: String,
    /// Working directory (optional).
    pub cwd: Option<String>,
    /// Seconds to wait inline before backgrounding (default 2).
    pub timeout: Option<u64>,
    /// Background immediately and return a job id without waiting.
    pub bg: Option<bool>,
    /// Run in an interactive bash that sources `~/.bashrc` so aliases and
    /// version managers (mise/nvm/rbenv) resolve. Default false (faster bare
    /// `sh -c`); set true when the command needs the user's shell setup.
    pub interactive: Option<bool>,
    /// Short label for this job, e.g. "build-api" or "deploy check". It becomes
    /// the job id prefix (`<title>-HH:MM:SS`) so you can tell your own jobs apart
    /// in `job(action="list")`. Optional; omit and the id is `job-HH:MM:SS`.
    pub title: Option<String>,
}

// ---- job ----

// `#[schemars(inline)]`: emit this enum inline on the `action` property
// (`{"type":"string","enum":[...]}`) instead of a `$ref` into `$defs`. Clients
// (Claude Desktop, codex, n8n) routinely drop `$defs`, so a `$ref` enum resolves
// to nothing — the model can't see it's a string and sends a garbage placeholder
// (`null`, then `true`), which fails deserialization before dispatch. Inlining is
// the documented fix (MCP python-sdk #1373: Literal inlines, Enum uses `$ref`).
//
// Variants also carry NO `///` doc comments on purpose: a doc'd unit enum renders
// as `oneOf` of `{const, description}`, which the same clients mishandle. Bare
// variants render a flat `enum`. Per-action docs live in the `job` tool
// description and the `action` field below.
#[derive(serde::Deserialize, schemars::JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum JobAction {
    Poll,
    // Read-only and id-free, so it's the safe fallback when `action` arrives as a
    // non-string placeholder or is absent — see `lenient_action`.
    #[default]
    List,
    Kill,
}

/// Tolerate the malformed `action` clients still send despite the inlined schema:
/// a literal `null`, a bare `true`, a number — anything non-string. Accept a valid
/// string variant; coerce everything else (and an omitted key, via `#[serde(default)]`)
/// to the read-only `list` default instead of dead-ending the whole call. `list`
/// is safe to guess: it's id-free and has no side effects, so the model just gets
/// the job list back and retries. Destructive `file` actions get no such fallback.
pub fn lenient_action<'de, D>(de: D) -> Result<JobAction, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    Ok(match Option::<serde_json::Value>::deserialize(de)? {
        Some(serde_json::Value::String(s)) => match s.as_str() {
            "poll" => JobAction::Poll,
            "kill" => JobAction::Kill,
            // "list" or any unknown string falls through to the safe default.
            _ => JobAction::List,
        },
        // null, bool, number, object, or array — the client bug. Default safely.
        _ => JobAction::List,
    })
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct JobArgs {
    /// What to do: poll, list, or kill. Defaults to list.
    #[serde(default, deserialize_with = "lenient_action")]
    pub action: JobAction,
    /// [poll, kill] job id.
    pub id: Option<String>,
    /// [poll] how many of the newest lines to skip. 0 (default) returns the most
    /// recent page; pass the previous response's next_cursor to page further back.
    pub cursor: Option<usize>,
    /// [poll] max lines to return (default 200).
    pub limit: Option<usize>,
}

// ---- file ----

// `#[schemars(inline)]` for the same reason as `JobAction`: keep the enum on the
// `action` property instead of a `$ref` clients drop. No lenient fallback here —
// `file` actions are destructive (delete/write/move) with no safe default, so a
// malformed `action` must error rather than be guessed.
#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum FileAction {
    Read,
    Write,
    Append,
    Delete,
    List,
    Grep,
    Move,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct FileArgs {
    /// What to do.
    pub action: FileAction,
    /// [read, write, append, delete, list, grep] target path.
    pub path: Option<String>,
    /// [write, append] file content.
    pub content: Option<String>,
    /// [grep] pattern to search for.
    pub pattern: Option<String>,
    /// [list, grep] recurse into subdirectories.
    pub recursive: Option<bool>,
    /// [move] source path.
    pub src: Option<String>,
    /// [move] destination path.
    pub dest: Option<String>,
    /// [read] line offset to start from (default 0).
    pub cursor: Option<usize>,
    /// [read] max lines to return (default 200).
    pub limit: Option<usize>,
}
