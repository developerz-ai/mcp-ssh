//! The MCP tool surface. Per the resource principle (Claude Code bible): expose
//! a tiny, constant set of tools grouped by *resource* and push everything else
//! into *parameters*. Three tools — `bash`, `job`, `file`. Real work lives in
//! `crate::jobs` and `files`.
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{common::RequestId, router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
// `RequestId` above is rmcp's extractor for the per-call JSON-RPC request id.
use tracing::Instrument;

use crate::jobs::{JobId, JobState, JobStore, Page, RunResult};

mod args;
mod files;

use args::{BashArgs, FileAction, FileArgs, JobAction, JobArgs};
use files::{FileError, FileOutcome};

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
                    // Output overflowed this first (top-of-output) page: the job is
                    // still in the store, so hand back its id. `job poll` is
                    // newest-first, so point at the latest rather than a forward
                    // cursor this page's reading order wouldn't match.
                    if page.has_more {
                        out.push_str(&format!(
                            "\noutput continues — see the latest with job(action=\"poll\", id=\"{id}\"), then page back with cursor."
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
        description = "Manage background jobs created by `bash`. action=poll returns job `id`'s output + status newest-first: call it with no cursor to see the most recent output of a long-running job, then page *back* through history with cursor=next_cursor (each page is itself in chronological order). action=list lists all jobs; action=kill kills job `id`."
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
                        Ok(Some((state, page))) => {
                            let mut out = render(&state, &page);
                            if page.has_more {
                                out.push_str(&format!(
                                    "\nolder output remains — page back with job(action=\"poll\", id=\"{id}\", cursor={}).",
                                    page.next_cursor
                                ));
                            }
                            Ok(ok(out))
                        }
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
            // A missing argument bails right here: it's this adapter's own
            // validation, not a file-op failure, so it never becomes a `FileError`.
            let result = match args.action {
                FileAction::Read => {
                    let Some(p) = args.path else {
                        return Ok(err("read requires `path`"));
                    };
                    files::read(&p, args.cursor.unwrap_or(0), args.limit.unwrap_or(200)).await
                }
                FileAction::Write => {
                    let (Some(p), Some(c)) = (args.path, args.content) else {
                        return Ok(err("write requires `path` and `content`"));
                    };
                    files::write(&p, &c).await
                }
                FileAction::Append => {
                    let (Some(p), Some(c)) = (args.path, args.content) else {
                        return Ok(err("append requires `path` and `content`"));
                    };
                    files::append(&p, &c).await
                }
                FileAction::Delete => {
                    let Some(p) = args.path else {
                        return Ok(err("delete requires `path`"));
                    };
                    files::delete(&p).await
                }
                FileAction::List => {
                    let Some(p) = args.path else {
                        return Ok(err("list requires `path`"));
                    };
                    files::list(&p, recursive).await
                }
                FileAction::Grep => {
                    let (Some(pat), Some(p)) = (args.pattern, args.path) else {
                        return Ok(err("grep requires `pattern` and `path`"));
                    };
                    files::grep(&pat, &p, recursive).await
                }
                FileAction::Move => {
                    let (Some(s), Some(d)) = (args.src, args.dest) else {
                        return Ok(err("move requires `src` and `dest`"));
                    };
                    files::rename(&s, &d).await
                }
            };
            Ok(match result {
                Ok(outcome) => ok(render_file(outcome)),
                Err(e) => err(render_file_error(&e)),
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

/// The sentence a finished file op reads as. `files` returns facts; the wording
/// belongs here with the rest of the presentation.
fn render_file(outcome: FileOutcome) -> String {
    match outcome {
        FileOutcome::Output(text) => text,
        // A marker, not a blank page: zero matches must stay distinguishable from
        // grep matching an empty line.
        FileOutcome::NoMatches => "[grep: no matches]".to_string(),
        FileOutcome::Wrote { path, bytes } => format!("wrote {bytes} bytes to {path}"),
        FileOutcome::Appended { path, bytes } => format!("appended {bytes} bytes to {path}"),
        FileOutcome::Deleted { path } => format!("deleted {path}"),
        FileOutcome::Moved { src, dest } => format!("moved {src} -> {dest}"),
    }
}

/// The message a failed file op reads as, plus the next step only this layer can
/// phrase — the hints name `file`'s own actions, which the domain doesn't know.
fn render_file_error(e: &FileError) -> String {
    match e {
        FileError::IsDirectory { path } => {
            format!("{path} is a directory — use file(action=\"list\", path=\"{path}\") instead")
        }
        FileError::DestinationExists { dest } => format!(
            "destination exists: {dest} — delete it or choose another path (move won't overwrite)"
        ),
        // Nothing to add: an errno or a failed `ls`/`find`/`grep` already says it.
        FileError::Io(_) | FileError::Shell(_) => e.to_string(),
    }
}

fn render(state: &JobState, page: &Page) -> String {
    let mut s = serde_json::to_string(state).unwrap_or_default();
    s.push('\n');
    s.push_str(&page.lines.join("\n"));
    if page.has_more {
        // Navigation hint (which cursor to pass next) is appended by the caller,
        // since `bash` reads forward from the top and `job poll` reads newest-first.
        s.push_str(&format!(
            "\n[{} of {} lines shown]",
            page.lines.len(),
            page.total_lines
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

    /// The `action` enum must be inlined onto the property — `{type:string, enum}` —
    /// not a `$ref` into `$defs`. Clients (Claude Desktop, codex, n8n) drop `$defs`,
    /// so a `$ref` enum resolves to nothing and the model sends a garbage placeholder.
    /// Lock both arg structs: no `$ref`/`$defs` anywhere, `action` is a string enum.
    #[test]
    fn action_enum_is_inlined_not_a_ref() {
        for schema in [
            serde_json::to_value(schemars::schema_for!(JobArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(FileArgs)).unwrap(),
        ] {
            let s = schema.to_string();
            assert!(!s.contains("$ref"), "schema must not use $ref: {schema}");
            assert!(!s.contains("$defs"), "schema must not use $defs: {schema}");
            let action = &schema["properties"]["action"];
            assert!(
                action.get("enum").and_then(|e| e.as_array()).is_some(),
                "`action` must be an inline string enum: {schema}"
            );
        }
    }

    /// Clients mishandle the enum's schema and send a non-string placeholder for
    /// `action` — `null`, a bare `true`, or omit it entirely. Each must deserialize
    /// to the read-only `list` default instead of erroring before dispatch; an
    /// explicit valid value still wins.
    #[test]
    fn job_action_garbage_or_missing_defaults_to_list() {
        for bad in [
            serde_json::json!({ "action": null }),
            serde_json::json!({ "action": true }),
            serde_json::json!({ "action": 1 }),
            serde_json::json!({ "action": "bogus" }),
            serde_json::json!({}),
        ] {
            let args: JobArgs = serde_json::from_value(bad.clone()).unwrap();
            assert!(
                matches!(args.action, JobAction::List),
                "expected list default for {bad}"
            );
        }

        let explicit: JobArgs =
            serde_json::from_value(serde_json::json!({ "action": "poll" })).unwrap();
        assert!(matches!(explicit.action, JobAction::Poll));
        let kill: JobArgs =
            serde_json::from_value(serde_json::json!({ "action": "kill" })).unwrap();
        assert!(matches!(kill.action, JobAction::Kill));

        // MCP spec: the advertised inputSchema must match behavior. `action` now
        // has a default, so it must NOT be listed `required` — clients (and the
        // model) are then free to omit it.
        let schema = serde_json::to_value(schemars::schema_for!(JobArgs)).unwrap();
        let required = schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|r| r.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();
        assert!(
            !required.contains(&"action"),
            "`action` must not be required once it has a default: {schema}"
        );
    }

    /// `FileArgs` with everything unset but `action` — each test fills only the
    /// fields its own action needs.
    fn file_args(action: FileAction) -> FileArgs {
        FileArgs {
            action,
            path: None,
            content: None,
            pattern: None,
            recursive: None,
            src: None,
            dest: None,
            cursor: None,
            limit: None,
        }
    }

    /// The text a tool call handed back, plus whether it was flagged an error.
    fn result_text(r: &CallToolResult) -> (String, bool) {
        let text = r
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap_or_default();
        (text, r.is_error.unwrap_or(false))
    }

    /// The agent-facing wording lives here in the adapter, not in `files` — the ops
    /// return facts. Drive the real dispatch over both sides of that boundary: a
    /// mutating op reads as its confirmation sentence, and a typed failure gains the
    /// next-step hint only this layer can phrase (it names `file`'s own actions).
    #[tokio::test]
    async fn file_dispatch_renders_outcomes_and_next_step_hints() {
        let tools = tools();
        let req = || RequestId(NumberOrString::Number(7));
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| dir.path().join(name).to_string_lossy().into_owned();
        let call = async |args: FileArgs| {
            let r = tools.file(Parameters(args), req()).await.unwrap();
            result_text(&r)
        };

        let note = path("note.txt");
        let (out, is_err) = call(FileArgs {
            path: Some(note.clone()),
            content: Some("hello".into()),
            ..file_args(FileAction::Write)
        })
        .await;
        assert_eq!(out, format!("wrote 5 bytes to {note}"));
        assert!(!is_err, "a completed write is not an error result");

        let moved = path("moved.txt");
        let (out, _) = call(FileArgs {
            src: Some(note.clone()),
            dest: Some(moved.clone()),
            ..file_args(FileAction::Move)
        })
        .await;
        assert_eq!(out, format!("moved {note} -> {moved}"));

        // Move onto an occupied path: the refusal keeps its "what to do instead".
        call(FileArgs {
            path: Some(note.clone()),
            content: Some("again".into()),
            ..file_args(FileAction::Write)
        })
        .await;
        let (out, is_err) = call(FileArgs {
            src: Some(note.clone()),
            dest: Some(moved),
            ..file_args(FileAction::Move)
        })
        .await;
        assert!(
            is_err && out.contains("destination exists") && out.contains("won't overwrite"),
            "a clobbering move must explain the alternative: {out}"
        );

        // Reading a directory redirects to `list` — the hint names the tool surface,
        // which is exactly why it belongs to the adapter and not to `files`.
        let (out, is_err) = call(FileArgs {
            path: Some(dir.path().to_string_lossy().into_owned()),
            ..file_args(FileAction::Read)
        })
        .await;
        assert!(
            is_err && out.contains("is a directory") && out.contains("file(action=\"list\""),
            "a directory read must redirect to list: {out}"
        );

        let (out, is_err) = call(FileArgs {
            pattern: Some("nothing-matches-this".into()),
            path: Some(note),
            ..file_args(FileAction::Grep)
        })
        .await;
        assert_eq!(out, "[grep: no matches]");
        assert!(!is_err, "zero matches is a marked result, not an error");
    }

    /// A missing argument is this adapter's own validation, not a file-op failure:
    /// it must come back as an error result naming the field, never reach `files`.
    #[tokio::test]
    async fn file_dispatch_rejects_missing_arguments() {
        let tools = tools();
        for (args, want) in [
            (file_args(FileAction::Read), "read requires `path`"),
            (
                file_args(FileAction::Write),
                "write requires `path` and `content`",
            ),
            (
                file_args(FileAction::Append),
                "append requires `path` and `content`",
            ),
            (file_args(FileAction::Delete), "delete requires `path`"),
            (file_args(FileAction::List), "list requires `path`"),
            (
                file_args(FileAction::Grep),
                "grep requires `pattern` and `path`",
            ),
            (
                file_args(FileAction::Move),
                "move requires `src` and `dest`",
            ),
        ] {
            let r = tools
                .file(Parameters(args), RequestId(NumberOrString::Number(1)))
                .await
                .unwrap();
            assert_eq!(result_text(&r), (want.to_string(), true));
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
