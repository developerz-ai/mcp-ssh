//! The MCP tool surface. Per the resource principle (Claude Code bible): expose
//! a tiny, constant set of tools grouped by *resource* and push everything else
//! into *parameters*. Three tools — `bash`, `job`, `file`. Real work lives in
//! `crate::jobs` and `files`.
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{common::RequestId, router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
// `RequestId` above is rmcp's extractor for the per-call JSON-RPC request id.
use tracing::Instrument;

use crate::jobs::{JobId, JobState, JobStore, Page, RunResult};

mod files;

#[derive(Clone)]
pub struct Tools {
    jobs: JobStore,
    // Read by the rmcp `#[tool_handler]` macro; dead-code analysis can't see it.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

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

// Variants carry NO `///` doc comments on purpose: schemars renders a doc'd
// unit enum as `oneOf` of `{const, description}`, which some MCP clients
// (Claude Desktop) mishandle — they send an `action` value serde can't parse,
// so the call fails before dispatch. Bare variants render a flat `enum`, which
// every client handles (same as `FileAction`). Per-action docs live in the
// `bash`/`job` tool descriptions and the `action` field below.
#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobAction {
    Poll,
    List,
    Kill,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct JobArgs {
    /// What to do: poll, list, or kill.
    pub action: JobAction,
    /// [poll, kill] job id.
    pub id: Option<String>,
    /// [poll] line offset to start from (default 0).
    pub cursor: Option<usize>,
    /// [poll] max lines to return (default 200).
    pub limit: Option<usize>,
}

// ---- file ----

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
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

#[tool_router]
impl Tools {
    pub fn new(jobs: JobStore) -> Self {
        Self {
            jobs,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Run a shell command on the host, locally as the service user. Fast commands return output inline; anything past the inline window (default 2s) returns a job id to monitor with `job`. bg=true backgrounds at once. timeout overrides the inline window. interactive=true sources ~/.bashrc (aliases, mise/nvm/rbenv); default is the faster bare sh -c. title labels the job id (`<title>-HH:MM:SS`) so you can tell your jobs apart. Output is byte- and line-capped per page so it never floods context."
    )]
    async fn bash(
        &self,
        Parameters(BashArgs {
            cmd,
            cwd,
            timeout,
            bg,
            interactive,
            title,
        }): Parameters<BashArgs>,
        RequestId(request_id): RequestId,
    ) -> Result<CallToolResult, McpError> {
        async move {
            // Emit inside the span so the prod subscriber (FmtSpan::NONE) logs the
            // dispatch with the span's `tool`/`request_id`; a bare span logs nothing.
            tracing::info!("dispatch");
            match self
                .jobs
                .run(
                    cmd,
                    cwd,
                    timeout,
                    bg.unwrap_or(false),
                    interactive.unwrap_or(false),
                    title,
                )
                .await
            {
                Ok(RunResult::Inline { id, state, page }) => {
                    let mut out = render(&state, &page);
                    // Output overflowed the first page: the job is still in the
                    // store, so hand back its id to fetch the rest instead of
                    // stranding the agent with a dead cursor.
                    if page.has_more {
                        out.push_str(&format!(
                            "\nfull output continues — fetch the rest with job(action=\"poll\", id=\"{id}\", cursor={}).",
                            page.next_cursor
                        ));
                    }
                    Ok(ok(out))
                }
                Ok(RunResult::Backgrounded { id }) => Ok(ok(format!(
                    "job {id} still running after the inline window. Monitor it with job(action=\"poll\", id=\"{id}\")."
                ))),
                Err(e) => Ok(err(e.to_string())),
            }
        }
        .instrument(tracing::info_span!("tool", tool = "bash", %request_id))
        .await
    }

    #[tool(
        description = "Manage background jobs created by `bash`. action=poll returns a paginated page (cursor/limit) of job `id`'s output + status — page through long logs without flooding context; action=list lists all jobs; action=kill kills job `id`."
    )]
    async fn job(
        &self,
        Parameters(args): Parameters<JobArgs>,
        RequestId(request_id): RequestId,
    ) -> Result<CallToolResult, McpError> {
        async move {
            tracing::info!("dispatch");
            match args.action {
                JobAction::Poll => {
                    let Some(id) = args.id else {
                        return Ok(err("poll requires `id`"));
                    };
                    let id = JobId::from(id);
                    match self
                        .jobs
                        .poll(&id, args.cursor.unwrap_or(0), args.limit)
                        .await
                    {
                        Ok(Some((state, page))) => Ok(ok(render(&state, &page))),
                        Ok(None) => Ok(err(format!("no such job: {id}"))),
                        Err(e) => Ok(err(e.to_string())),
                    }
                }
                JobAction::List => {
                    let jobs = self.jobs.list().await;
                    Ok(ok(serde_json::to_string_pretty(&jobs).unwrap_or_default()))
                }
                JobAction::Kill => {
                    let Some(id) = args.id else {
                        return Ok(err("kill requires `id`"));
                    };
                    let id = JobId::from(id);
                    if self.jobs.kill(&id).await {
                        Ok(ok(format!("killed {id}")))
                    } else {
                        Ok(err(format!("no such job: {id}")))
                    }
                }
            }
        }
        .instrument(tracing::info_span!("tool", tool = "job", %request_id))
        .await
    }

    #[tool(
        description = "File operations on the host, run locally as the service user. action: read (paginated by line via cursor/limit), write (create/truncate `path` with `content`), append (`content` to `path`), delete (`path`, file or dir), list (`path`; recursive=true for the whole tree), grep (`pattern` in `path`; recursive=true under a dir), move (`src` -> `dest`)."
    )]
    async fn file(
        &self,
        Parameters(args): Parameters<FileArgs>,
        RequestId(request_id): RequestId,
    ) -> Result<CallToolResult, McpError> {
        async move {
            tracing::info!("dispatch");
            let recursive = args.recursive.unwrap_or(false);
            let result = match args.action {
                FileAction::Read => match args.path {
                    Some(p) => {
                        files::read(&p, args.cursor.unwrap_or(0), args.limit.unwrap_or(200)).await
                    }
                    None => Err("read requires `path`".into()),
                },
                FileAction::Write => match (args.path, args.content) {
                    (Some(p), Some(c)) => files::write(&p, &c).await,
                    _ => Err("write requires `path` and `content`".into()),
                },
                FileAction::Append => match (args.path, args.content) {
                    (Some(p), Some(c)) => files::append(&p, &c).await,
                    _ => Err("append requires `path` and `content`".into()),
                },
                FileAction::Delete => match args.path {
                    Some(p) => files::delete(&p).await,
                    None => Err("delete requires `path`".into()),
                },
                FileAction::List => match args.path {
                    Some(p) => files::list(&p, recursive).await,
                    None => Err("list requires `path`".into()),
                },
                FileAction::Grep => match (args.pattern, args.path) {
                    (Some(pat), Some(p)) => files::grep(&pat, &p, recursive).await,
                    _ => Err("grep requires `pattern` and `path`".into()),
                },
                FileAction::Move => match (args.src, args.dest) {
                    (Some(s), Some(d)) => files::rename(&s, &d).await,
                    _ => Err("move requires `src` and `dest`".into()),
                },
            };
            Ok(match result {
                Ok(s) => ok(s),
                Err(e) => err(e),
            })
        }
        .instrument(tracing::info_span!("tool", tool = "file", %request_id))
        .await
    }
}

#[tool_handler]
impl ServerHandler for Tools {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Remote shell + file access for one host. Three tools: `bash` runs a command \
             (auto-backgrounds slow ones, returning a job id); `job` (action=poll/list/kill) \
             monitors jobs with paginated output; `file` (action=read/write/append/delete/list/grep/move) \
             operates locally as the service user.",
        )
    }
}

// ---- helpers ----

fn ok(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![Content::text(text.into())])
}

fn err(text: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(text.into())])
}

fn render(state: &JobState, page: &Page) -> String {
    let mut s = serde_json::to_string(state).unwrap_or_default();
    s.push('\n');
    s.push_str(&page.lines.join("\n"));
    if page.has_more {
        s.push_str(&format!(
            "\n[lines ..{} of {}; next_cursor={}]",
            page.next_cursor, page.total_lines, page.next_cursor
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::NumberOrString;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    /// A `MakeWriter` that appends everything the subscriber emits into a shared
    /// buffer, so a test can assert on the formatted span/event output.
    #[derive(Clone, Default)]
    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Ok(mut guard) = self.0.lock() {
                guard.extend_from_slice(buf);
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufWriter {
        type Writer = BufWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// A doc'd unit enum makes schemars emit `oneOf` of `{const, description}`,
    /// which Claude Desktop mishandles — the `job` tool's `action` then fails
    /// deserialization before dispatch. Both action enums must render as a flat
    /// `enum` (string list). Lock it so a stray `///` can't silently regress.
    #[test]
    fn action_enums_render_as_flat_enum_not_oneof() {
        for schema in [
            serde_json::to_value(schemars::schema_for!(JobAction)).unwrap(),
            serde_json::to_value(schemars::schema_for!(FileAction)).unwrap(),
        ] {
            assert!(
                schema.get("enum").and_then(|e| e.as_array()).is_some(),
                "action enum must be a flat string `enum`: {schema}"
            );
            assert!(
                schema.get("oneOf").is_none(),
                "action enum must NOT be `oneOf` (Claude Desktop mishandles it): {schema}"
            );
        }
    }

    fn tools() -> Tools {
        let dir = tempfile::tempdir().unwrap().keep();
        let store = JobStore::new(
            dir,
            std::time::Duration::from_secs(2),
            crate::jobs::Shell::sh(),
            crate::db::Db::memory(),
        )
        .unwrap();
        Tools::new(store)
    }

    /// Every tool dispatch emits an event inside a span carrying `tool` +
    /// `request_id` (CLAUDE.md). The subscriber here mirrors prod (no
    /// `FmtSpan` span events), so a green assertion proves the *event* — not
    /// span lifecycle logging prod disables — carries the fields. `bash` is
    /// representative; `job`/`file` wrap identically.
    #[tokio::test]
    async fn bash_dispatch_emits_event_with_tool_and_request_id() {
        use tracing::instrument::WithSubscriber;

        let buf = BufWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();

        tools()
            .bash(
                Parameters(BashArgs {
                    cmd: "true".into(),
                    cwd: None,
                    timeout: None,
                    bg: None,
                    interactive: None,
                    title: None,
                }),
                RequestId(NumberOrString::Number(42)),
            )
            .with_subscriber(subscriber)
            .await
            .unwrap();

        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(
            out.contains("tool=\"bash\""),
            "span must tag tool=bash: {out}"
        );
        assert!(
            out.contains("request_id=42"),
            "span must carry the request id: {out}"
        );
    }
}
