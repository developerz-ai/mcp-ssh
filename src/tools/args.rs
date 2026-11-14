//! Deserializable argument structs for the three MCP tools, plus the leniency
//! shim `job`'s `action` needs. Split out of `mod.rs` to keep the dispatcher
//! (the tool bodies + presentation) focused on behavior, not schema shape.

// ---- bash ----

/// Longest `timeout` honoured, in seconds: past it a command is backgrounded
/// anyway, so a request can't sit open for hours.
pub const MAX_BASH_TIMEOUT_SECS: u64 = 600;

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BashArgs {
    /// Shell command to run (must not be empty).
    pub cmd: String,
    /// Working directory (optional; must be an existing directory).
    pub cwd: Option<String>,
    /// Seconds to wait inline before backgrounding (default 2, at most 600).
    pub timeout: Option<u64>,
    /// Background immediately and return a job id without waiting.
    pub bg: Option<bool>,
    /// Run in an interactive bash that sources `~/.bashrc` so aliases and
    /// version managers (mise/nvm/rbenv) resolve. Default false (faster bare
    /// `sh -c`); set true when the command needs the user's shell setup.
    pub interactive: Option<bool>,
    /// Short label for this job, e.g. "build-api" or "deploy check". It becomes
    /// the job id prefix (`<title>-HH-MM-SS`) so you can tell your own jobs apart
    /// in `job(action="list")`. Optional; omit and the id is `job-HH-MM-SS`.
    pub title: Option<String>,
}

// ---- job ----

/// The `job` tool's `action`. Deserialized only through [`lenient_action`], and
/// its schema is written by hand (below) because it must advertise exactly the
/// three real actions while `Unknown` carries a string no derive would leave out
/// cleanly.
#[derive(Debug, Default)]
pub enum JobAction {
    Poll,
    // Read-only and id-free, so it's the safe fallback when `action` arrives as a
    // non-string placeholder or is absent — see `lenient_action`.
    #[default]
    List,
    Kill,
    // A string naming no action (`"stop"`, `"KILL"`): an error naming the valid
    // ones, never a guess. Not advertised in the schema.
    Unknown(String),
}

/// A flat, inlined `{"type":"string","enum":[...]}` — never a `$ref` into `$defs`
/// and never a `oneOf`. Clients (Claude Desktop, codex, n8n) drop `$defs` and
/// mishandle `oneOf`, then send a garbage placeholder (`null`, `true`) that fails
/// before dispatch (MCP python-sdk #1373 has the same fix).
impl schemars::JsonSchema for JobAction {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        "JobAction".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "enum": ["poll", "list", "kill"]
        })
    }
}

impl JobAction {
    /// The action's name for a log line — fixed words only, never the raw string
    /// an unknown action carried.
    pub fn name(&self) -> &'static str {
        match self {
            JobAction::Poll => "poll",
            JobAction::List => "list",
            JobAction::Kill => "kill",
            JobAction::Unknown(_) => "unknown",
        }
    }
}

/// Tolerate the malformed `action` clients send despite the inlined schema: a
/// literal `null`, a bare `true`, a number — anything non-string — and an omitted
/// key (via `#[serde(default)]`) become the read-only `list`, instead of
/// dead-ending the whole call; it's id-free and side-effect-free, so the model
/// just gets the job list back. A *string* is the model's real intent, though: one
/// that names no action is kept as [`JobAction::Unknown`] and refused, never
/// silently turned into a listing.
pub fn lenient_action<'de, D>(de: D) -> Result<JobAction, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    Ok(match Option::<serde_json::Value>::deserialize(de)? {
        Some(serde_json::Value::String(s)) => match s.as_str() {
            "poll" => JobAction::Poll,
            "list" => JobAction::List,
            "kill" => JobAction::Kill,
            _ => JobAction::Unknown(s),
        },
        // null, bool, number, object, or array — the client bug. Default safely.
        _ => JobAction::List,
    })
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobArgs {
    /// What to do: poll, list, or kill. Defaults to list.
    #[serde(default, deserialize_with = "lenient_action")]
    pub action: JobAction,
    /// [poll, kill] job id.
    pub id: Option<String>,
    /// [poll, list] where the next page starts: pass the previous response's
    /// next_cursor. Omit for the first page (poll: the latest output; list: the
    /// newest jobs).
    pub cursor: Option<u64>,
    /// [poll] max lines to return (default 200); [list] max jobs (default and
    /// most 200).
    pub limit: Option<usize>,
}

// ---- file ----

// `#[schemars(inline)]` for the same reason as `JobAction`: keep the enum on the
// `action` property instead of a `$ref` clients drop. No lenient fallback here —
// `file` actions are destructive (delete/write/move) with no safe default, so a
// malformed `action` must error rather than be guessed.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
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

// `deny_unknown_fields`: a misspelt param (`recurse=true`, `offset=200`) must
// error, not be silently ignored — for `delete` that would change what's removed.
#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileArgs {
    /// What to do.
    pub action: FileAction,
    /// [read, write, append, delete, list, grep] target path.
    pub path: Option<String>,
    /// [write, append] file content.
    pub content: Option<String>,
    /// [grep] basic regular expression (grep's default syntax) to search for.
    pub pattern: Option<String>,
    /// [list, grep, delete] recurse into subdirectories; delete requires it to
    /// remove a non-empty directory.
    pub recursive: Option<bool>,
    /// [move] source path.
    pub src: Option<String>,
    /// [move] destination path — the full new path, not a directory to move into.
    pub dest: Option<String>,
    /// [read] line offset to start from (default 0).
    pub cursor: Option<usize>,
    /// [read] max lines to return (default 200, at most 1000; a page is also
    /// capped at 64 KiB).
    pub limit: Option<usize>,
}
