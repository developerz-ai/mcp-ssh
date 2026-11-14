//! Speaks MCP Streamable HTTP to a running server the way a client does: an
//! initialized session, `tools/call`, and helpers for reading tool replies.
use std::time::{Duration, Instant};

use reqwest::{Client, Response, StatusCode, header, redirect};
use serde_json::{Value, json};

/// HTTP client that never follows redirects: the OAuth code arrives in a 302.
pub fn http() -> Client {
    Client::builder()
        .redirect(redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .expect("reqwest client")
}

/// Result of one `tools/call`: the joined text content and the `isError` flag.
#[derive(Debug)]
pub struct ToolResult {
    pub text: String,
    pub is_error: bool,
}

/// An initialized MCP Streamable HTTP session.
pub struct Session {
    base: String,
    token: String,
    sid: String,
    next_id: u64,
    http: Client,
}

impl Session {
    pub async fn open(base: &str, token: &str) -> Self {
        let http = http();
        let init = json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "e2e", "version": "0" }
            }
        });
        let resp = post_mcp(&http, base, token, None, &init).await;
        assert_eq!(resp.status(), StatusCode::OK, "initialize");
        let sid = resp.headers()["mcp-session-id"]
            .to_str()
            .expect("session id")
            .to_string();
        read_rpc(resp, 0).await;
        let mut session = Self {
            base: base.to_string(),
            token: token.to_string(),
            sid,
            next_id: 1,
            http,
        };
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        let resp = session.post(&note).await;
        assert!(resp.status().is_success(), "initialized: {}", resp.status());
        session
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// Open the standalone SSE stream (`GET /mcp`) for this session.
    pub async fn stream(&self) -> Response {
        self.http
            .get(format!("{}/mcp", self.base))
            .bearer_auth(&self.token)
            .header("mcp-session-id", &self.sid)
            .header(header::ACCEPT, "text/event-stream")
            .send()
            .await
            .expect("GET /mcp")
    }

    async fn post(&mut self, body: &Value) -> Response {
        post_mcp(&self.http, &self.base, &self.token, Some(&self.sid), body).await
    }

    pub async fn call(&mut self, tool: &str, args: Value) -> ToolResult {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": tool, "arguments": args }
        });
        let resp = self.post(&req).await;
        assert_eq!(resp.status(), StatusCode::OK, "tools/call {tool}");
        let msg = read_rpc(resp, id).await;
        if let Some(err) = msg.get("error") {
            // Protocol-level rejection (bad args): surface as a tool error.
            return ToolResult {
                text: err.to_string(),
                is_error: true,
            };
        }
        let result = &msg["result"];
        let text = result["content"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|c| c["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        ToolResult {
            text,
            is_error: result["isError"].as_bool().unwrap_or(false),
        }
    }
}

async fn post_mcp(
    http: &Client,
    base: &str,
    token: &str,
    sid: Option<&str>,
    body: &Value,
) -> Response {
    let mut req = http
        .post(format!("{base}/mcp"))
        .bearer_auth(token)
        .header(header::ACCEPT, "application/json, text/event-stream")
        .json(body);
    if let Some(sid) = sid {
        req = req.header("mcp-session-id", sid);
    }
    req.send().await.expect("POST /mcp")
}

/// Read a JSON or SSE response until the JSON-RPC message with `id` arrives. An
/// SSE stream may stay open after it, so stop at the message, not at EOF.
async fn read_rpc(mut resp: Response, id: u64) -> Value {
    let mut buf = String::new();
    while let Some(chunk) = resp.chunk().await.expect("read body") {
        buf.push_str(&String::from_utf8_lossy(&chunk));
        if let Some(msg) = find_message(&buf, id) {
            return msg;
        }
    }
    find_message(&buf, id).unwrap_or_else(|| panic!("no JSON-RPC reply {id} in: {buf}"))
}

fn find_message(buf: &str, id: u64) -> Option<Value> {
    let candidates = buf
        .lines()
        .map(|l| l.strip_prefix("data:").unwrap_or(l).trim());
    candidates
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["id"].as_u64() == Some(id))
}

/// First job id `<title>-HH-MM-SS[...]` in a tool reply.
pub fn job_id(text: &str, title: &str) -> String {
    let start = text
        .find(&format!("{title}-"))
        .unwrap_or_else(|| panic!("no {title} job id in: {text}"));
    text[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect()
}

/// Poll `job` until its output contains `needle` (bounded wait).
pub async fn poll_until(s: &mut Session, id: &str, needle: &str) -> String {
    let start = Instant::now();
    loop {
        let r = s.call("job", json!({ "action": "poll", "id": id })).await;
        if r.text.contains(needle) {
            return r.text;
        }
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "{id} never showed {needle:?}: {}",
            r.text
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The cursor a poll reply names for the next (older) page: `cursor=<n>`.
pub fn next_cursor(text: &str) -> u64 {
    let start = text
        .find("cursor=")
        .unwrap_or_else(|| panic!("no next cursor in: {text}"))
        + "cursor=".len();
    let digits: String = text[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().expect("numeric cursor")
}
