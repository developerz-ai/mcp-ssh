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
mod tests {
    use super::*;
    use crate::jobs::KillOutcome;
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
    /// to the read-only `list` default instead of erroring before dispatch. A
    /// *string* that names no action is the model's real intent and must not be
    /// guessed: it is kept as `Unknown` (F9).
    #[test]
    fn job_action_garbage_or_missing_defaults_to_list() {
        for bad in [
            serde_json::json!({ "action": null }),
            serde_json::json!({ "action": true }),
            serde_json::json!({ "action": 1 }),
            serde_json::json!({}),
        ] {
            let args: JobArgs = serde_json::from_value(bad.clone()).unwrap();
            assert!(
                matches!(args.action, JobAction::List),
                "expected list default for {bad}"
            );
        }
        for unknown in ["bogus", "stop", "KILL"] {
            let args: JobArgs =
                serde_json::from_value(serde_json::json!({ "action": unknown })).unwrap();
            assert!(
                matches!(&args.action, JobAction::Unknown(s) if s == unknown),
                "{unknown} must not become list"
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

        // An empty string is as missing as an absent key — it never reaches `files`.
        let empty = || Some(String::new());
        for (args, want) in [
            (
                FileArgs {
                    path: empty(),
                    ..file_args(FileAction::Read)
                },
                "read requires `path`",
            ),
            (
                FileArgs {
                    pattern: empty(),
                    path: Some("/tmp".into()),
                    ..file_args(FileAction::Grep)
                },
                "grep requires `pattern` and `path`",
            ),
            (
                FileArgs {
                    src: Some("/tmp/a".into()),
                    dest: empty(),
                    ..file_args(FileAction::Move)
                },
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

    /// F2: deleting a populated directory without the opt-in is refused, names
    /// the fix, and leaves the tree whole.
    #[tokio::test]
    async fn file_delete_of_populated_dir_requires_recursive() {
        let tools = tools();
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("keep.txt"), "k").unwrap();
        let delete = |recursive| FileArgs {
            path: Some(tree.to_string_lossy().into_owned()),
            recursive,
            ..file_args(FileAction::Delete)
        };

        let r = tools
            .file(
                Parameters(delete(None)),
                RequestId(NumberOrString::Number(1)),
            )
            .await
            .unwrap();
        let (out, is_err) = result_text(&r);
        assert!(is_err && out.contains("recursive=true"), "{out}");
        assert!(tree.join("keep.txt").exists());

        let r = tools
            .file(
                Parameters(delete(Some(true))),
                RequestId(NumberOrString::Number(2)),
            )
            .await
            .unwrap();
        assert!(!result_text(&r).1);
        assert!(!tree.exists());
    }

    /// F10: a misspelt param must fail deserialization, not be silently dropped —
    /// `recurse=true` on a delete would otherwise change what gets removed.
    #[test]
    fn file_args_reject_unknown_fields() {
        let bad = serde_json::json!({ "action": "delete", "path": "/x", "recurse": true });
        assert!(serde_json::from_value::<FileArgs>(bad).is_err());
        let good = serde_json::json!({ "action": "delete", "path": "/x", "recursive": true });
        assert!(serde_json::from_value::<FileArgs>(good).is_ok());
    }

    /// F14: the `file` span names the action (enum name only) and closes with an
    /// outcome; the path and content never reach the log.
    #[tokio::test]
    async fn file_dispatch_logs_action_and_outcome_not_path_or_content() {
        use tracing::instrument::WithSubscriber;

        let buf = BufWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret-name.txt");

        tools()
            .file(
                Parameters(FileArgs {
                    path: Some(path.to_string_lossy().into_owned()),
                    content: Some("s3cret-content".into()),
                    ..file_args(FileAction::Write)
                }),
                RequestId(NumberOrString::Number(9)),
            )
            .with_subscriber(subscriber)
            .await
            .unwrap();

        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(out.contains("tool=\"file\""), "{out}");
        assert!(out.contains("action=Write"), "{out}");
        assert!(out.contains("outcome=\"ok\""), "{out}");
        assert!(out.contains("request_id=9"), "{out}");
        assert!(
            !out.contains("secret-name") && !out.contains("s3cret"),
            "{out}"
        );
    }

    fn bash_args(cmd: &str) -> BashArgs {
        BashArgs {
            cmd: cmd.into(),
            cwd: None,
            timeout: None,
            bg: None,
            interactive: None,
            title: None,
        }
    }

    fn job_args(action: JobAction, id: Option<&str>) -> JobArgs {
        JobArgs {
            action,
            id: id.map(str::to_string),
            cursor: None,
            limit: None,
        }
    }

    async fn call_bash(tools: &Tools, args: BashArgs) -> (String, bool) {
        let r = tools
            .bash(Parameters(args), RequestId(NumberOrString::Number(1)))
            .await
            .unwrap();
        result_text(&r)
    }

    async fn call_job(tools: &Tools, args: JobArgs) -> (String, bool) {
        let r = tools
            .job(Parameters(args), RequestId(NumberOrString::Number(1)))
            .await
            .unwrap();
        result_text(&r)
    }

    /// F16 + K14: an empty `cmd` is refused before anything runs; a bad `cwd` is
    /// named; `bg` reads as started, not as "still running after the window".
    #[tokio::test]
    async fn bash_validates_its_input_and_words_bg_as_started() {
        let tools = tools();
        assert_eq!(
            call_bash(&tools, bash_args("  ")).await,
            ("bash requires a non-empty `cmd`".to_string(), true)
        );
        let (out, is_err) = call_bash(
            &tools,
            BashArgs {
                cwd: Some("/no/such/dir".into()),
                ..bash_args("true")
            },
        )
        .await;
        assert!(
            is_err && out.starts_with("cwd /no/such/dir is not usable"),
            "{out}"
        );
        let (out, is_err) = call_bash(
            &tools,
            BashArgs {
                bg: Some(true),
                ..bash_args("true")
            },
        )
        .await;
        assert!(
            !is_err && out.contains("started in the background"),
            "{out}"
        );
    }

    /// F10: a misspelt param fails deserialization instead of being dropped.
    #[test]
    fn bash_and_job_args_reject_unknown_fields() {
        let bad = serde_json::json!({ "cmd": "ls", "timout": 5 });
        assert!(serde_json::from_value::<BashArgs>(bad).is_err());
        let bad = serde_json::json!({ "action": "poll", "id": "x", "offset": 200 });
        assert!(serde_json::from_value::<JobArgs>(bad).is_err());
    }

    /// F9: an unknown action string is refused by name, with the valid ones.
    #[tokio::test]
    async fn job_unknown_action_is_an_error_naming_the_valid_ones() {
        let (out, is_err) = call_job(
            &tools(),
            job_args(JobAction::Unknown("stop".into()), Some("x")),
        )
        .await;
        assert!(
            is_err && out.contains("\"stop\"") && out.contains("poll, list, kill"),
            "{out}"
        );
    }

    /// K8: each kill outcome reads as what happened — never "no such job" for a
    /// job that exists.
    #[tokio::test]
    async fn job_kill_wording_per_outcome() {
        let tools = tools();
        let (out, is_err) = call_job(&tools, job_args(JobAction::Kill, Some("nope"))).await;
        assert_eq!((out.as_str(), is_err), ("no such job: nope", true));

        call_bash(&tools, bash_args("echo done")).await;
        let listed = tools.jobs.list(0, None).await.unwrap();
        let finished = listed.jobs[0].id.as_ref().to_string();
        let (out, is_err) = call_job(&tools, job_args(JobAction::Kill, Some(&finished))).await;
        assert!(
            is_err && out.contains("is not running (status: exited)"),
            "{out}"
        );

        let (out, _) = call_bash(
            &tools,
            BashArgs {
                bg: Some(true),
                title: Some("sleeper".into()),
                ..bash_args("sleep 300")
            },
        )
        .await;
        let running = out.split_whitespace().nth(1).unwrap().to_string();
        assert_eq!(
            call_job(&tools, job_args(JobAction::Kill, Some(&running))).await,
            (format!("killed {running}"), false)
        );

        let rendered = |o| result_text(&job_render::kill(&JobId::from("j"), Ok(o)));
        assert!(
            rendered(KillOutcome::KilledLeftovers)
                .0
                .contains("left running")
        );
        assert!(
            rendered(KillOutcome::NoProcessGroup)
                .0
                .contains("no recorded process group")
        );
        assert!(
            rendered(KillOutcome::CorruptProcessGroup { pgid: 0 })
                .0
                .contains("refusing to signal")
        );
        let (out, is_err) = rendered(KillOutcome::NotSignalled {
            error: std::io::Error::from_raw_os_error(libc::EINVAL),
        });
        assert!(is_err && out.contains("may still be running"), "{out}");
        let (out, is_err) = result_text(&job_render::kill(
            &JobId::from("j"),
            Err(crate::jobs::JobError::Store(rusqlite::Error::InvalidQuery)),
        ));
        assert!(
            is_err && out == "job store unavailable — try again shortly",
            "{out}"
        );
    }

    /// B6: `list` is a bounded page, newest first, with the cursor to the next.
    #[tokio::test]
    async fn job_list_is_a_bounded_page_with_a_cursor() {
        let db = crate::db::Db::memory();
        let store = JobStore::new(
            tempfile::tempdir().unwrap().keep(),
            std::time::Duration::from_secs(2),
            crate::jobs::Shell::sh(),
            db.clone(),
        )
        .unwrap();
        let tools = Tools::new(store);
        db.call(|c| {
            let tx = c.unchecked_transaction()?;
            for i in 0..5000 {
                tx.execute(
                    "INSERT INTO jobs (id, status, code, started_unix) VALUES (?1, 'exited', 0, ?2)",
                    rusqlite::params![format!("job-{i:05}"), i],
                )?;
            }
            tx.commit()
        })
        .await
        .unwrap();
        let (out, is_err) = call_job(&tools, job_args(JobAction::List, None)).await;
        assert!(!is_err);
        assert!(out.len() <= 64 * 1024, "{} bytes", out.len());
        assert!(
            out.find("job-04999").unwrap() < out.find("job-04998").unwrap(),
            "newest first"
        );
        assert!(
            out.ends_with("[older jobs: job(action=\"list\", cursor=200)]"),
            "{}",
            &out[out.len() - 80..]
        );
    }

    /// F14: the `job` span names the action and closes with an outcome.
    #[tokio::test]
    async fn job_dispatch_logs_action_and_outcome() {
        use tracing::instrument::WithSubscriber;
        let buf = BufWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        tools()
            .job(
                Parameters(job_args(JobAction::Poll, Some("nope"))),
                RequestId(NumberOrString::Number(5)),
            )
            .with_subscriber(subscriber)
            .await
            .unwrap();
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(
            out.contains("action=\"poll\"") && out.contains("outcome=\"error\""),
            "{out}"
        );
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
