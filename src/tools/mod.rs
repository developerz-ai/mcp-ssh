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

use crate::jobs::{JobState, JobStore, Page, RunResult};

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
}

// ---- job ----

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobAction {
    /// Fetch a page of a job's output + status.
    Poll,
    /// List all jobs.
    List,
    /// Kill a running job.
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
        description = "Run a shell command on the host (locally, as the service user). Returns output inline if it finishes within the inline window (default 2s); otherwise returns a job id — monitor it with the `job` tool. Pass bg=true to background immediately and get the id without waiting. Use it to launch long tasks (builds, deploys, `claude -p ...`) without blocking."
    )]
    async fn bash(
        &self,
        Parameters(BashArgs {
            cmd,
            cwd,
            timeout,
            bg,
        }): Parameters<BashArgs>,
        RequestId(request_id): RequestId,
    ) -> Result<CallToolResult, McpError> {
        async move {
            match self.jobs.run(cmd, cwd, timeout, bg.unwrap_or(false)).await {
                Ok(RunResult::Inline { state, page }) => Ok(ok(render(&state, &page))),
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
            match args.action {
                JobAction::Poll => {
                    let Some(id) = args.id else {
                        return Ok(err("poll requires `id`"));
                    };
                    match self
                        .jobs
                        .poll(&id, args.cursor.unwrap_or(0), args.limit)
                        .await
                    {
                        Some((state, page)) => Ok(ok(render(&state, &page))),
                        None => Ok(err(format!("no such job: {id}"))),
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
    use tracing_subscriber::fmt::format::FmtSpan;

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

    fn tools() -> Tools {
        let dir = tempfile::tempdir().unwrap().keep();
        let store = JobStore::new(dir, std::time::Duration::from_secs(2)).unwrap();
        Tools::new(store)
    }

    /// Every tool dispatch runs inside a span carrying `tool` + `request_id`
    /// (CLAUDE.md). `bash` is representative; `job`/`file` wrap identically.
    #[tokio::test]
    async fn bash_dispatch_runs_in_a_span_with_tool_and_request_id() {
        use tracing::instrument::WithSubscriber;

        let buf = BufWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .with_span_events(FmtSpan::NEW)
            .with_max_level(tracing::Level::INFO)
            .finish();

        tools()
            .bash(
                Parameters(BashArgs {
                    cmd: "true".into(),
                    cwd: None,
                    timeout: None,
                    bg: None,
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
