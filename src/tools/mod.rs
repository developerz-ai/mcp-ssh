//! The MCP tool surface: a small set of heavily-parametrized tools. Every method
//! is a thin adapter — real work lives in `crate::jobs` and `files`.
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};

use crate::jobs::{JobState, JobStore, Page, RunResult};

mod files;

#[derive(Clone)]
pub struct Tools {
    jobs: JobStore,
    // Read by the rmcp `#[tool_handler]` macro; dead-code analysis can't see it.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

// ---- tool parameter schemas ----

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct Bash {
    #[schemars(description = "shell command to run")]
    pub cmd: String,
    #[schemars(description = "working directory (optional)")]
    pub cwd: Option<String>,
    #[schemars(description = "seconds to wait inline before backgrounding (default 2)")]
    pub timeout: Option<u64>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct JobPoll {
    #[schemars(description = "job id returned by bash")]
    pub id: String,
    #[schemars(description = "line offset to start from (default 0)")]
    pub cursor: Option<usize>,
    #[schemars(description = "max lines to return (default 200)")]
    pub limit: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct JobId {
    #[schemars(description = "job id")]
    pub id: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ReadFile {
    pub path: String,
    #[schemars(description = "line offset to start from (default 0)")]
    pub cursor: Option<usize>,
    #[schemars(description = "max lines to return (default 200)")]
    pub limit: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct WriteFile {
    pub path: String,
    pub content: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct Path {
    pub path: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ListDir {
    pub path: String,
    #[schemars(description = "recurse into subdirectories")]
    pub recursive: Option<bool>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct Grep {
    pub pattern: String,
    pub path: String,
    #[schemars(description = "recurse into subdirectories")]
    pub recursive: Option<bool>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct Move {
    pub src: String,
    pub dest: String,
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
        description = "Run a shell command locally as the service user. Returns output inline if it finishes within the inline window (default 2s); otherwise returns a job id to poll with job_poll. Use it to launch long tasks (builds, deploys, `claude -p ...`) and monitor them without blocking."
    )]
    async fn bash(
        &self,
        Parameters(Bash { cmd, cwd, timeout }): Parameters<Bash>,
    ) -> Result<CallToolResult, McpError> {
        match self.jobs.run(cmd, cwd, timeout).await {
            Ok(RunResult::Inline { state, page }) => Ok(ok(render(&state, &page))),
            Ok(RunResult::Backgrounded { id }) => Ok(ok(format!(
                "job {id} still running after the inline window. Poll it with job_poll(id=\"{id}\")."
            ))),
            Err(e) => Ok(err(e.to_string())),
        }
    }

    #[tool(
        description = "Fetch a page of a job's output and its status. Paginated by line via cursor/limit so long logs don't flood context."
    )]
    async fn job_poll(
        &self,
        Parameters(JobPoll { id, cursor, limit }): Parameters<JobPoll>,
    ) -> Result<CallToolResult, McpError> {
        match self.jobs.poll(&id, cursor.unwrap_or(0), limit).await {
            Some((state, page)) => Ok(ok(render(&state, &page))),
            None => Ok(err(format!("no such job: {id}"))),
        }
    }

    #[tool(description = "List all jobs with their status.")]
    async fn job_list(&self) -> Result<CallToolResult, McpError> {
        let jobs = self.jobs.list().await;
        Ok(ok(serde_json::to_string_pretty(&jobs).unwrap_or_default()))
    }

    #[tool(description = "Kill a running job by id.")]
    async fn job_kill(
        &self,
        Parameters(JobId { id }): Parameters<JobId>,
    ) -> Result<CallToolResult, McpError> {
        if self.jobs.kill(&id).await {
            Ok(ok(format!("killed {id}")))
        } else {
            Ok(err(format!("no such job: {id}")))
        }
    }

    #[tool(description = "Read a file, paginated by line (cursor/limit) to bound output size.")]
    async fn file_read(
        &self,
        Parameters(ReadFile {
            path,
            cursor,
            limit,
        }): Parameters<ReadFile>,
    ) -> Result<CallToolResult, McpError> {
        wrap(files::read(&path, cursor.unwrap_or(0), limit.unwrap_or(200)).await)
    }

    #[tool(description = "Write content to a file, creating or truncating it.")]
    async fn file_write(
        &self,
        Parameters(WriteFile { path, content }): Parameters<WriteFile>,
    ) -> Result<CallToolResult, McpError> {
        wrap(files::write(&path, &content).await)
    }

    #[tool(description = "Append content to a file, creating it if absent.")]
    async fn file_append(
        &self,
        Parameters(WriteFile { path, content }): Parameters<WriteFile>,
    ) -> Result<CallToolResult, McpError> {
        wrap(files::append(&path, &content).await)
    }

    #[tool(description = "Delete a file or directory.")]
    async fn file_delete(
        &self,
        Parameters(Path { path }): Parameters<Path>,
    ) -> Result<CallToolResult, McpError> {
        wrap(files::delete(&path).await)
    }

    #[tool(description = "List a directory (ls), or the whole tree when recursive=true (find).")]
    async fn file_list(
        &self,
        Parameters(ListDir { path, recursive }): Parameters<ListDir>,
    ) -> Result<CallToolResult, McpError> {
        wrap(files::list(&path, recursive.unwrap_or(false)).await)
    }

    #[tool(
        description = "Grep a pattern in a file, or recursively under a directory when recursive=true."
    )]
    async fn file_grep(
        &self,
        Parameters(Grep {
            pattern,
            path,
            recursive,
        }): Parameters<Grep>,
    ) -> Result<CallToolResult, McpError> {
        wrap(files::grep(&pattern, &path, recursive.unwrap_or(false)).await)
    }

    #[tool(description = "Move or rename a file or directory.")]
    async fn file_move(
        &self,
        Parameters(Move { src, dest }): Parameters<Move>,
    ) -> Result<CallToolResult, McpError> {
        wrap(files::rename(&src, &dest).await)
    }
}

#[tool_handler]
impl ServerHandler for Tools {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Remote shell + file access for one host. `bash` auto-backgrounds slow commands and \
             returns a job id; poll it with `job_poll` (paginated). File tools read/write/list/grep \
             locally as the service user.",
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

fn wrap(r: Result<String, String>) -> Result<CallToolResult, McpError> {
    Ok(match r {
        Ok(s) => ok(s),
        Err(e) => err(e),
    })
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
