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
    let jobs = JobStore::new(dir, std::time::Duration::from_secs(2)).unwrap();
    let state = oauth::AuthState {
        creds: auth::Credentials {
            user: "admin".into(),
            pass: "secret".into(),
        },
        store: Arc::new(oauth::Store::default()),
        public_url: public_url.map(Into::into),
    };
    build(state, jobs, vec!["localhost".into(), "127.0.0.1".into()])
}

/// Build a test app with a pre-minted bearer token ready for session tests.
async fn test_app_with_token() -> (Router, String) {
    let dir = tempfile::tempdir().unwrap().keep();
    let jobs = JobStore::new(dir, std::time::Duration::from_secs(2)).unwrap();
    let oauth_store = Arc::new(oauth::Store::default());
    let token = "test-bearer-token".to_string();
    oauth_store.insert_token(&token).await;
    let state = oauth::AuthState {
        creds: auth::Credentials {
            user: "admin".into(),
            pass: "secret".into(),
        },
        store: oauth_store,
        public_url: None,
    };
    let app = build(state, jobs, vec!["localhost".into(), "127.0.0.1".into()]);
    (app, token)
}

#[tokio::test]
async fn discovery_metadata_is_public() {
    let res = test_app()
        .oneshot(
            Request::builder()
                .uri("/.well-known/oauth-authorization-server")
                .header(header::HOST, "mcp.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn mcp_without_auth_and_no_public_url_returns_bare_401() {
    // `/mcp` auth runs before the allowed-hosts gate, so the Host here is
    // attacker-controlled. With no configured public_url it must NOT be
    // reflected into a challenge: a bare 401, no WWW-Authenticate.
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
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert!(!res.headers().contains_key(header::WWW_AUTHENTICATE));
}

#[tokio::test]
async fn mcp_without_auth_with_public_url_advertises_configured_challenge() {
    // A configured public_url is trusted, so the challenge is advertised — and
    // it uses that URL, never the (attacker-controlled) Host header.
    let res = test_app_with(Some("https://mcp.example.com"))
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
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let challenge = res
        .headers()
        .get(header::WWW_AUTHENTICATE)
        .expect("challenge present when public_url is configured")
        .to_str()
        .unwrap();
    assert!(challenge.contains("Bearer"));
    assert!(challenge.contains("https://mcp.example.com/.well-known/oauth-protected-resource"));
    assert!(!challenge.contains("evil.example.com"));
}

/// A valid OAuth bearer token passes the `/mcp` guard.
#[tokio::test]
async fn mcp_with_bearer_auth_passes_the_guard() {
    let (app, token) = test_app_with_token().await;
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header(header::HOST, "mcp.example.com")
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
    assert_ne!(res.status(), StatusCode::UNAUTHORIZED);
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
                .header(header::HOST, "mcp.example.com")
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

    // ── job: list → valid JSON (array, possibly empty since bash ran inline) ─
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
    serde_json::from_str::<serde_json::Value>(job_text).expect("job list must return valid JSON");
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
