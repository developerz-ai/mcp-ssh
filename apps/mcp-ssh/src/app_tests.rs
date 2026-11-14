use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt; // oneshot

/// Collect every `data: <payload>` line from an SSE body and parse as JSON.
fn sse_json_lines(body: &str) -> Vec<serde_json::Value> {
    body.lines()
        .filter(|l| l.starts_with("data:"))
        .filter_map(|l| serde_json::from_str(l.trim_start_matches("data:").trim()).ok())
        .collect()
}

/// Find the first JSON-RPC message (has a `"jsonrpc"` key) in an SSE body.
fn first_jsonrpc(body: &str) -> serde_json::Value {
    sse_json_lines(body)
        .into_iter()
        .find(|v| v.get("jsonrpc").is_some())
        .expect("no JSON-RPC message found in SSE response body")
}

fn test_app() -> Router {
    test_app_with(None)
}

fn test_app_with(public_url: Option<&str>) -> Router {
    let dir = tempfile::tempdir().unwrap().keep();
    // One in-memory DB shared by the job store and the OAuth store, mirroring prod
    // where both back onto the single SQLite database.
    let database = crate::db::Db::memory();
    let jobs = JobStore::new(
        dir,
        std::time::Duration::from_secs(2),
        crate::jobs::Shell::sh(),
        database.clone(),
    )
    .unwrap();
    let state = oauth::AuthState {
        creds: auth::Credentials {
            user: "admin".into(),
            pass: crate::config::Password::new("secret"),
        },
        store: Arc::new(oauth::Store::new(database)),
        public_url: public_url.map(Into::into),
        revalidate_every: crate::auth::STREAM_REVALIDATE,
    };
    build(
        state,
        jobs,
        vec!["localhost".into(), "127.0.0.1".into()],
        tokio_util::sync::CancellationToken::new(),
    )
}

/// Build a test app with a pre-minted bearer token ready for session tests.
async fn test_app_with_token() -> (Router, String) {
    let dir = tempfile::tempdir().unwrap().keep();
    let database = crate::db::Db::memory();
    let jobs = JobStore::new(
        dir,
        std::time::Duration::from_secs(2),
        crate::jobs::Shell::sh(),
        database.clone(),
    )
    .unwrap();
    let oauth_store = Arc::new(oauth::Store::new(database));
    let token = "test-bearer-token".to_string();
    oauth_store.insert_token(&token).await;
    let state = oauth::AuthState {
        creds: auth::Credentials {
            user: "admin".into(),
            pass: crate::config::Password::new("secret"),
        },
        store: oauth_store,
        public_url: None,
        revalidate_every: crate::auth::STREAM_REVALIDATE,
    };
    let app = build(
        state,
        jobs,
        vec!["localhost".into(), "127.0.0.1".into()],
        tokio_util::sync::CancellationToken::new(),
    );
    (app, token)
}

/// `/healthz` is unauthenticated and returns a bare 200 — used by liveness probes.
#[tokio::test]
async fn healthz_returns_200_without_auth() {
    let res = test_app()
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"ok");
}

/// `/healthz` being public must not weaken `/mcp`'s auth guard.
#[tokio::test]
async fn healthz_does_not_bypass_mcp_auth() {
    let res = test_app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header(header::HOST, "localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

/// `/healthz` answers whatever the Host: local probes use `127.0.0.1` even when
/// only the public name is allow-listed.
#[tokio::test]
async fn healthz_is_outside_the_host_guard() {
    let res = test_app()
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .header(header::HOST, "elsewhere.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn discovery_metadata_is_public() {
    let res = test_app()
        .oneshot(
            Request::builder()
                .uri("/.well-known/oauth-authorization-server")
                .header(header::HOST, "localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

/// Without a `public_url` the metadata is built from `Host`, so a Host off the
/// allowlist must never reach it — nor any other OAuth route.
#[tokio::test]
async fn oauth_routes_refuse_a_disallowed_host() {
    for (method, uri) in [
        ("GET", "/.well-known/oauth-authorization-server"),
        ("GET", "/.well-known/oauth-protected-resource"),
        ("GET", "/authorize"),
        ("POST", "/token"),
        ("POST", "/register"),
    ] {
        let res = test_app()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::HOST, "evil.example.com")
                    .header("x-forwarded-proto", "javascript")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "{method} {uri}");
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(!body.contains("evil.example.com") && !body.contains("javascript"));
    }
}

#[tokio::test]
async fn an_allowed_host_matches_case_insensitively_and_with_any_port() {
    for host in ["LOCALHOST", "localhost:1337", "127.0.0.1:9"] {
        let res = test_app()
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-authorization-server")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{host}");
    }
}

/// Any 401 from `/mcp` carries the RFC 9728 challenge, built from the
/// (allow-listed) Host when no `public_url` is configured.
#[tokio::test]
async fn mcp_without_auth_and_no_public_url_challenges_with_the_allowed_host() {
    let res = test_app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header(header::HOST, "localhost:1337")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        res.headers()[header::WWW_AUTHENTICATE],
        "Bearer resource_metadata=\"http://localhost:1337/.well-known/oauth-protected-resource\""
    );
}

#[tokio::test]
async fn mcp_with_a_disallowed_host_is_forbidden_before_auth() {
    let res = test_app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header(header::HOST, "evil.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert!(!res.headers().contains_key(header::WWW_AUTHENTICATE));
}

#[tokio::test]
async fn mcp_without_auth_with_public_url_advertises_configured_challenge() {
    // A configured public_url is the challenge's base; the request's Host is
    // never consulted.
    let res = test_app_with(Some("https://mcp.example.com"))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header(header::HOST, "127.0.0.1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        res.headers()[header::WWW_AUTHENTICATE],
        "Bearer resource_metadata=\"https://mcp.example.com/.well-known/oauth-protected-resource\""
    );
}

/// A token store that can't answer is a 503, never a 401 that would send every
/// client back through the login flow.
#[tokio::test]
async fn mcp_with_an_unavailable_token_store_is_503() {
    let dir = tempfile::tempdir().unwrap().keep();
    let database = crate::db::Db::memory();
    let jobs = JobStore::new(
        dir,
        std::time::Duration::from_secs(2),
        crate::jobs::Shell::sh(),
        database.clone(),
    )
    .unwrap();
    let state = oauth::AuthState {
        creds: auth::Credentials {
            user: "admin".into(),
            pass: crate::config::Password::new("secret"),
        },
        store: Arc::new(oauth::Store::new(database.clone())),
        public_url: None,
        revalidate_every: crate::auth::STREAM_REVALIDATE,
    };
    let app = build(
        state,
        jobs,
        vec!["localhost".into()],
        tokio_util::sync::CancellationToken::new(),
    );
    database
        .call(|c| c.execute_batch("DROP TABLE access_tokens"))
        .await
        .unwrap();
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header(header::HOST, "localhost")
                .header(header::AUTHORIZATION, "Bearer some-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
}

/// A router whose open streams re-check their token every 20ms, a live bearer,
/// and the database behind both.
async fn revalidating_app() -> (Router, String, crate::db::Db) {
    let dir = tempfile::tempdir().unwrap().keep();
    let database = crate::db::Db::memory();
    let jobs = JobStore::new(
        dir,
        std::time::Duration::from_secs(2),
        crate::jobs::Shell::sh(),
        database.clone(),
    )
    .unwrap();
    let oauth_store = Arc::new(oauth::Store::new(database.clone()));
    let token = "stream-bearer-token".to_string();
    oauth_store.insert_token(&token).await;
    let state = oauth::AuthState {
        creds: auth::Credentials {
            user: "admin".into(),
            pass: crate::config::Password::new("secret"),
        },
        store: oauth_store,
        public_url: None,
        revalidate_every: std::time::Duration::from_millis(20),
    };
    let app = build(
        state,
        jobs,
        vec!["localhost".into()],
        tokio_util::sync::CancellationToken::new(),
    );
    (app, token, database)
}

/// Open the session's standalone SSE stream (GET `/mcp`) and return its body.
async fn open_stream(app: Router, token: &str, session_id: &str) -> Body {
    let res = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/mcp")
                .header(header::HOST, "localhost")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::ACCEPT, "text/event-stream")
                .header("mcp-session-id", session_id)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "no SSE stream opened");
    res.into_body()
}

/// True if `body` ends within `wait`; false if it is still open then.
async fn ends_within(body: &mut Body, wait: std::time::Duration) -> bool {
    use axum::body::HttpBody;
    let drained = async {
        while std::future::poll_fn(|cx| std::pin::Pin::new(&mut *body).poll_frame(cx))
            .await
            .is_some()
        {}
    };
    tokio::time::timeout(wait, drained).await.is_ok()
}

/// A16: the bearer used to be checked only when the request started, so a stream
/// opened before `sessions --revoke` stayed open. It now ends within one interval
/// of the token going away — and not before.
#[tokio::test]
async fn an_open_stream_ends_once_its_token_is_revoked() {
    let (app, token, database) = revalidating_app().await;
    let session_id = open_session(app.clone(), &token).await;
    let mut stream = open_stream(app, &token, &session_id).await;
    assert!(
        !ends_within(&mut stream, std::time::Duration::from_millis(100)).await,
        "a stream with a live token must stay open across re-checks"
    );

    oauth::revoke_all(&database).await.unwrap();

    assert!(
        ends_within(&mut stream, std::time::Duration::from_secs(5)).await,
        "stream outlived its revoked token"
    );
}

/// A store that can't answer is not a revocation: the stream stays open and the
/// next tick asks again (the stream-side twin of "outage is a 503, not a 401").
#[tokio::test]
async fn an_open_stream_survives_a_token_store_outage() {
    let (app, token, database) = revalidating_app().await;
    let session_id = open_session(app.clone(), &token).await;
    let mut stream = open_stream(app, &token, &session_id).await;

    database
        .call(|c| c.execute_batch("DROP TABLE access_tokens"))
        .await
        .unwrap();

    assert!(
        !ends_within(&mut stream, std::time::Duration::from_millis(200)).await,
        "a store outage must not end the stream"
    );
}

/// A valid OAuth bearer token passes the `/mcp` guard — but only under an
/// allowed Host: a disallowed one is refused outright (DNS-rebinding guard).
#[tokio::test]
async fn mcp_with_bearer_auth_passes_the_guard() {
    let (app, token) = test_app_with_token().await;
    for (host, expected) in [
        ("localhost", StatusCode::OK),
        ("mcp.example.com", StatusCode::FORBIDDEN),
    ] {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(header::HOST, host)
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ACCEPT, "application/json, text/event-stream")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), expected, "Host {host}");
    }
}

/// HTTP Basic is no longer accepted on `/mcp` — bearer-only per OAuth 2.1.
#[tokio::test]
async fn basic_on_mcp_now_401s() {
    use base64::Engine;
    let creds = base64::engine::general_purpose::STANDARD.encode("admin:secret");
    let res = test_app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header(header::HOST, "localhost")
                .header(header::AUTHORIZATION, format!("Basic {creds}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream")
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

/// Boot a session (initialize + notifications/initialized) and return the session id.
async fn open_session(app: axum::Router, token: &str) -> String {
    let init_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header(header::HOST, "localhost")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream")
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(init_res.status(), StatusCode::OK);
    let session_id = init_res
        .headers()
        .get("mcp-session-id")
        .expect("mcp-session-id header must be present")
        .to_str()
        .unwrap()
        .to_owned();
    // consume body so the connection is released before the next oneshot
    axum::body::to_bytes(init_res.into_body(), 1 << 20)
        .await
        .unwrap();

    let notif_res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header(header::HOST, "localhost")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream")
                .header("mcp-session-id", &session_id)
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(notif_res.status(), StatusCode::ACCEPTED);
    session_id
}

/// Send a `tools/call` request and return the full JSON-RPC response message.
async fn call_tool(
    app: axum::Router,
    token: &str,
    session_id: &str,
    id: u32,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    });
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header(header::HOST, "localhost")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream")
                .header("mcp-session-id", session_id)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "tools/call HTTP status");
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    first_jsonrpc(std::str::from_utf8(&bytes).unwrap())
}

/// Exercise each tool end-to-end via tools/call:
///   bash `echo hello`  → inline output contains "hello"
///   file write→read    → roundtrip content survives the cycle
///   job list           → returns parseable JSON
#[tokio::test]
async fn tools_call_exercises_bash_file_and_job() {
    let (app, token) = test_app_with_token().await;
    let session_id = open_session(app.clone(), &token).await;

    // ── bash: echo hello → inline result contains "hello" ────────────────
    let bash_msg = call_tool(
        app.clone(),
        &token,
        &session_id,
        3,
        "bash",
        serde_json::json!({"cmd": "echo hello"}),
    )
    .await;
    let bash_text = bash_msg["result"]["content"][0]["text"]
        .as_str()
        .expect("bash result must have text content");
    assert!(
        bash_text.contains("hello"),
        "bash echo hello output: {bash_text}"
    );

    // ── file: write then read roundtrip ───────────────────────────────────
    // Keep the tempdir alive for the duration of the test so the path is valid.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("roundtrip.txt");
    let path_str = path.to_str().unwrap();

    let write_msg = call_tool(
        app.clone(),
        &token,
        &session_id,
        4,
        "file",
        serde_json::json!({"action": "write", "path": path_str, "content": "roundtrip"}),
    )
    .await;
    let write_text = write_msg["result"]["content"][0]["text"]
        .as_str()
        .expect("file write result must have text content");
    assert!(
        write_text.contains("wrote"),
        "file write confirmation: {write_text}"
    );

    let read_msg = call_tool(
        app.clone(),
        &token,
        &session_id,
        5,
        "file",
        serde_json::json!({"action": "read", "path": path_str}),
    )
    .await;
    let read_text = read_msg["result"]["content"][0]["text"]
        .as_str()
        .expect("file read result must have text content");
    assert!(
        read_text.contains("roundtrip"),
        "file read content: {read_text}"
    );

    // ── job: list → the presence line (the caller, as `you`: the bearer's
    // client reached the tool through rmcp), then a JSON array ─
    let job_msg = call_tool(
        app.clone(),
        &token,
        &session_id,
        6,
        "job",
        serde_json::json!({"action": "list"}),
    )
    .await;
    let job_text = job_msg["result"]["content"][0]["text"]
        .as_str()
        .expect("job list result must have text content");
    let (presence, json) = job_text.split_once('\n').expect("presence line, then JSON");
    assert!(
        presence.starts_with("1 agent active (last 15 min): you (client#"),
        "{presence}"
    );
    let jobs = serde_json::from_str::<serde_json::Value>(json).expect("job list must be JSON");
    let jobs = jobs.as_array().expect("an array");
    assert!(!jobs.is_empty(), "the inline bash call is listed");
    assert!(
        jobs.iter().all(|job| job["by"] == "you"),
        "the caller's own jobs read `by: you`: {jobs:?}"
    );
}

/// A call rmcp refuses before dispatch (here a misspelt param) used to leave the
/// journal silent. The client's reply is rmcp's, untouched; the operator gets one
/// line naming the tool and the reason — and none of the arguments.
#[tokio::test]
async fn a_call_rejected_before_dispatch_is_logged_without_its_arguments() {
    let logs = crate::log_capture_tests::capture();
    let (app, token) = test_app_with_token().await;
    let session_id = open_session(app.clone(), &token).await;

    let msg = call_tool(
        app,
        &token,
        &session_id,
        5,
        "bash",
        serde_json::json!({ "cmd": "echo s3cret-arg", "timout": 5 }),
    )
    .await;
    assert_eq!(msg["result"]["isError"], true, "{msg}");
    let text = msg["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.starts_with("failed to deserialize parameters: unknown field `timout`"),
        "the agent still gets rmcp's own explanation: {text}"
    );

    let out = logs.text();
    let line = out
        .lines()
        .find(|l| l.contains("rejected"))
        .unwrap_or_else(|| panic!("no rejection logged: {out}"));
    assert!(line.contains("WARN"), "{line}");
    assert!(line.contains("tool=\"bash\""), "{line}");
    assert!(line.contains("reason=\"unknown_field\""), "{line}");
    assert!(line.contains("request_id=5"), "{line}");
    // rmcp's own DEBUG lines (below prod's default filter) print the request;
    // ours must not.
    for ours in out.lines().filter(|l| l.contains(" mcp_ssh::")) {
        assert!(
            !ours.contains("s3cret-arg") && !ours.contains("timout"),
            "{ours}"
        );
    }
}

/// tools/list must return exactly the three tools: bash, job, file — never more.
/// This test is the canary; if someone adds a 4th tool it fails here first.
#[tokio::test]
async fn tool_surface_is_exactly_bash_job_file() {
    let (app, token) = test_app_with_token().await;
    let session_id = open_session(app.clone(), &token).await;

    let list_res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header(header::HOST, "localhost")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream")
                .header("mcp-session-id", &session_id)
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_res.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(list_res.into_body(), 1 << 20)
        .await
        .unwrap();
    let msg = first_jsonrpc(std::str::from_utf8(&bytes).unwrap());
    let tools = msg["result"]["tools"]
        .as_array()
        .expect("tools/list result must contain a tools array");
    let mut names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["bash", "file", "job"],
        "MCP surface must be exactly 3 tools: bash, job, file"
    );
}

/// Drive a real MCP JSON-RPC session through the built router:
///   initialize → capture mcp-session-id → notifications/initialized → tools/list
///
/// Router clones share the same Arc<LocalSessionManager>, so the session
/// created in step 1 is visible to subsequent oneshot calls.
#[tokio::test]
async fn mcp_json_rpc_initialize_and_tools_list() {
    let (app, token) = test_app_with_token().await;

    // ── 1. initialize ────────────────────────────────────────────────────
    let init_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header(header::HOST, "localhost")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream")
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(init_res.status(), StatusCode::OK);
    let session_id = init_res
        .headers()
        .get("mcp-session-id")
        .expect("mcp-session-id header must be present on initialize response")
        .to_str()
        .unwrap()
        .to_owned();
    let init_bytes = axum::body::to_bytes(init_res.into_body(), 1 << 20)
        .await
        .unwrap();
    let init_msg = first_jsonrpc(std::str::from_utf8(&init_bytes).unwrap());
    assert_eq!(init_msg["id"], 1);
    assert!(
        init_msg["result"].is_object(),
        "initialize must return a result"
    );

    // ── 2. notifications/initialized → 202 ──────────────────────────────
    let notif_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header(header::HOST, "localhost")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream")
                .header("mcp-session-id", &session_id)
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(notif_res.status(), StatusCode::ACCEPTED);

    // ── 3. tools/list → exactly bash + job + file ───────────────────────
    let list_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header(header::HOST, "localhost")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream")
                .header("mcp-session-id", &session_id)
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_res.status(), StatusCode::OK);
    let list_bytes = axum::body::to_bytes(list_res.into_body(), 1 << 20)
        .await
        .unwrap();
    let list_msg = first_jsonrpc(std::str::from_utf8(&list_bytes).unwrap());
    assert_eq!(list_msg["id"], 2);
    let tools = list_msg["result"]["tools"]
        .as_array()
        .expect("tools/list result must contain a tools array");
    let mut names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["bash", "file", "job"], "exactly the 3 MCP tools");
}
