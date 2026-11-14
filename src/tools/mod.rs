//! The MCP tool surface. Per the resource principle (Claude Code bible): expose
//! a tiny, constant set of tools grouped by *resource* and push everything else
//! into *parameters*. Three tools — `bash`, `job`, `file`. Real work lives in
//! `crate::jobs` and `files`.
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{common::RequestId, router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
// `RequestId` above is rmcp's extractor for the per-call JSON-RPC request id.
use tracing::Instrument;

use crate::jobs::{JobId, JobStore};

mod args;
mod files;
mod job_render;
mod render;

use args::{BashArgs, FileAction, FileArgs, JobAction, JobArgs};
use render::{render_file, render_file_error};

#[derive(Clone)]
pub struct Tools {
    jobs: JobStore,
    // Read by the rmcp `#[tool_handler]` macro; dead-code analysis can't see it.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
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
        description = "Run a shell command on the host, locally as the service user. Fast commands return output inline (the newest page); anything past the inline window (default 2s) returns a job id to monitor with `job`. bg=true backgrounds at once. timeout overrides the inline window (at most 600s). cwd must be an existing directory. interactive=true sources ~/.bashrc (aliases, mise/nvm/rbenv); default is the faster bare sh -c. title labels the job id (`<title>-HH-MM-SS`) so you can tell your jobs apart. Output is byte- and line-capped per page so it never floods context; a job whose output passes 256 MiB is killed. Finished jobs are kept 24h after they finish; running jobs are never reaped."
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
            let result = if cmd.trim().is_empty() {
                err("bash requires a non-empty `cmd`")
            } else {
                let background = bg.unwrap_or(false);
                let timeout = timeout.map(|t| t.min(args::MAX_BASH_TIMEOUT_SECS));
                let run = self
                    .jobs
                    .run(
                        cmd,
                        cwd,
                        timeout,
                        background,
                        interactive.unwrap_or(false),
                        title,
                    )
                    .await;
                job_render::run(run, background)
            };
            log_outcome(&result);
            Ok(result)
        }
        .instrument(tracing::info_span!("tool", tool = "bash", %request_id))
        .await
    }

    #[tool(
        description = "Manage background jobs created by `bash`. action=poll returns job `id`'s status + output newest-first: call it with no cursor to see the most recent output, then page *back* through history with the cursor the reply names (each page is itself in chronological order). action=list lists jobs newest first, paged by cursor/limit. action=kill kills job `id`'s whole process group (and processes a finished job left running)."
    )]
    async fn job(
        &self,
        Parameters(args): Parameters<JobArgs>,
        RequestId(request_id): RequestId,
    ) -> Result<CallToolResult, McpError> {
        // `action` is a fixed word only — never the raw string an unknown one carried.
        let span =
            tracing::info_span!("tool", tool = "job", action = args.action.name(), %request_id);
        async move {
            tracing::info!("dispatch");
            let id = non_empty(args.id).map(JobId::from);
            let result = match args.action {
                JobAction::Poll => match id {
                    Some(id) => {
                        job_render::poll(&id, self.jobs.poll(&id, args.cursor, args.limit).await)
                    }
                    None => err("poll requires `id`"),
                },
                JobAction::List => {
                    let cursor = args
                        .cursor
                        .map_or(0, |c| usize::try_from(c).unwrap_or(usize::MAX));
                    job_render::list(self.jobs.list(cursor, args.limit).await)
                }
                JobAction::Kill => match id {
                    Some(id) => job_render::kill(&id, self.jobs.kill(&id).await),
                    None => err("kill requires `id`"),
                },
                JobAction::Unknown(action) => job_render::unknown_action(&action),
            };
            log_outcome(&result);
            Ok(result)
        }
        .instrument(span)
        .await
    }

    #[tool(
        description = "File operations on the host, run locally as the service user. action: read (`path`, regular files only, paginated by line via cursor/limit), write (replace `path` with `content`, atomically; keeps an existing file's mode and writes through a symlink), append (`content` to `path`), delete (`path`; a symlink is unlinked, never followed; a non-empty directory needs recursive=true), list (`path`; recursive=true for the whole tree), grep (basic-regex `pattern` in `path`; recursive=true under a directory), move (`src` -> `dest`, the full new path; never overwrites)."
    )]
    async fn file(
        &self,
        Parameters(args): Parameters<FileArgs>,
        RequestId(request_id): RequestId,
    ) -> Result<CallToolResult, McpError> {
        // `action` is the enum name only — never a path, pattern or content.
        let span = tracing::info_span!("tool", tool = "file", action = ?args.action, %request_id);
        async move {
            tracing::info!("dispatch");
            let recursive = args.recursive.unwrap_or(false);
            // An empty string is as missing as an absent key (`path=""` used to
            // surface as a bare errno). `content` is exempt: writing "" is valid.
            let (path, pattern) = (non_empty(args.path), non_empty(args.pattern));
            let (src, dest) = (non_empty(args.src), non_empty(args.dest));
            // A missing argument bails right here: it's this adapter's own
            // validation, not a file-op failure, so it never becomes a `FileError`.
            let result = match args.action {
                FileAction::Read => {
                    let Some(p) = path else {
                        return Ok(err("read requires `path`"));
                    };
                    files::read(&p, args.cursor.unwrap_or(0), args.limit.unwrap_or(200)).await
                }
                FileAction::Write => {
                    let (Some(p), Some(c)) = (path, args.content) else {
                        return Ok(err("write requires `path` and `content`"));
                    };
                    files::write(&p, &c).await
                }
                FileAction::Append => {
                    let (Some(p), Some(c)) = (path, args.content) else {
                        return Ok(err("append requires `path` and `content`"));
                    };
                    files::append(&p, &c).await
                }
                FileAction::Delete => {
                    let Some(p) = path else {
                        return Ok(err("delete requires `path`"));
                    };
                    files::delete(&p, recursive).await
                }
                FileAction::List => {
                    let Some(p) = path else {
                        return Ok(err("list requires `path`"));
                    };
                    files::list(&p, recursive).await
                }
                FileAction::Grep => {
                    let (Some(pat), Some(p)) = (pattern, path) else {
                        return Ok(err("grep requires `pattern` and `path`"));
                    };
                    files::grep(&pat, &p, recursive).await
                }
                FileAction::Move => {
                    let (Some(s), Some(d)) = (src, dest) else {
                        return Ok(err("move requires `src` and `dest`"));
                    };
                    files::rename(&s, &d).await
                }
            };
            Ok(match result {
                Ok(outcome) => {
                    tracing::info!(outcome = "ok", "done");
                    ok(render_file(outcome))
                }
                Err(e) => {
                    tracing::info!(outcome = "error", "done");
                    err(render_file_error(&e))
                }
            })
        }
        .instrument(span)
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
    CallToolResult::success(vec![ContentBlock::text(text.into())])
}

fn err(text: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(text.into())])
}

/// A string argument that is absent *or empty* is missing.
fn non_empty(arg: Option<String>) -> Option<String> {
    arg.filter(|s| !s.is_empty())
}

/// The one outcome event a `bash`/`job` dispatch closes with.
fn log_outcome(result: &CallToolResult) {
    let outcome = if result.is_error == Some(true) {
        "error"
    } else {
        "ok"
    };
    tracing::info!(outcome, "done");
}

#[cfg(test)]
mod tests;
