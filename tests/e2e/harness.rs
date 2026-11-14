//! Boots the real `mcp-ssh serve` binary on an ephemeral port with all state in a
//! tempdir, and speaks OAuth + MCP Streamable HTTP to it the way a client does.
//! The child is killed when the `Server` drops, so a failing test leaks nothing.
use std::collections::HashMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::{Client, Response, StatusCode, header, redirect};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const USER: &str = "e2e";
pub const PASS: &str = "e2e-password";
const REDIRECT: &str = "http://localhost/cb";
const BOOT_DEADLINE: Duration = Duration::from_secs(20);

/// A running server plus the tempdir holding its DB, job logs, config and HOME.
pub struct Server {
    child: Option<Child>,
    pub base: String,
    pub dir: tempfile::TempDir,
}

impl Server {
    /// Boot with the default test env; panics if it isn't healthy in time.
    pub async fn start() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        Self::start_in(dir, &[]).await
    }

    /// Boot on an existing state dir (restart scenarios) with extra env.
    pub async fn start_in(dir: tempfile::TempDir, extra_env: &[(&str, &str)]) -> Self {
        let port = free_port();
        let child = spawn(dir.path(), port, extra_env).expect("spawn mcp-ssh");
        let server = Self {
            child: Some(child),
            base: format!("http://127.0.0.1:{port}"),
            dir,
        };
        server.wait_healthy().await;
        server
    }

    pub fn pid(&self) -> u32 {
        self.child.as_ref().expect("running").id()
    }

    pub fn job_dir(&self) -> PathBuf {
        self.dir.path().join("jobs")
    }

    pub fn db_path(&self) -> PathBuf {
        self.dir.path().join("state").join("mcp-ssh.db")
    }

    /// SIGTERM the server and wait up to `deadline` for it to exit. Returns the
    /// state dir (for a restart) and whether it exited in time.
    pub fn terminate(mut self, deadline: Duration) -> (tempfile::TempDir, bool) {
        let mut child = self.child.take().expect("running");
        signal(&child, libc::SIGTERM);
        let exited = wait_exit(&mut child, deadline);
        if !exited {
            let _ = child.kill();
            let _ = child.wait();
        }
        let dir = std::mem::replace(&mut self.dir, tempfile::tempdir().expect("tempdir"));
        (dir, exited)
    }

    async fn wait_healthy(&self) {
        let start = Instant::now();
        let http = Client::new();
        while start.elapsed() < BOOT_DEADLINE {
            let url = format!("{}/healthz", self.base);
            if let Ok(r) = http.get(url).send().await
                && r.status().is_success()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("mcp-ssh did not answer /healthz within {BOOT_DEADLINE:?}");
    }

    /// Run the full OAuth login and open an initialized MCP session.
    pub async fn session(&self) -> Session {
        let token = login(&self.base).await;
        Session::open(&self.base, &token).await
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Spawn `mcp-ssh serve` with a scrubbed env: nothing from the developer's shell
/// (a stray `MCP_SSH_*`) leaks in, and nothing touches /etc or /var/lib.
pub fn spawn(dir: &Path, port: u16, extra_env: &[(&str, &str)]) -> std::io::Result<Child> {
    let home = dir.join("home");
    std::fs::create_dir_all(&home)?;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mcp-ssh"));
    cmd.arg("serve")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("MCP_SSH_CONFIG", dir.join("absent-config.toml"))
        .env("MCP_SSH_BIND", format!("127.0.0.1:{port}"))
        .env("MCP_SSH_USER", USER)
        .env("MCP_SSH_PASS", PASS)
        .env("MCP_SSH_DB", dir.join("state").join("mcp-ssh.db"))
        .env("MCP_SSH_JOB_DIR", dir.join("jobs"))
        .env("MCP_SSH_INLINE_TIMEOUT_SECS", "1")
        .env("RUST_LOG", "warn")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.spawn()
}

/// An OS-assigned free port (never 1337). The listener is dropped before the
/// server binds; the window for a collision is negligible in a test run.
pub fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind :0");
    listener.local_addr().expect("local addr").port()
}

pub fn signal(child: &Child, sig: libc::c_int) {
    let pid = libc::pid_t::try_from(child.id()).expect("pid fits");
    // SAFETY: plain kill(2) on our own child's pid.
    let rc = unsafe { libc::kill(pid, sig) };
    assert_eq!(rc, 0, "kill({pid}, {sig}) failed");
}

pub fn wait_exit(child: &mut Child, deadline: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// HTTP client that never follows redirects: the OAuth code arrives in a 302.
pub fn http() -> Client {
    Client::builder()
        .redirect(redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .expect("reqwest client")
}

/// register → GET /authorize (consent page) → POST /authorize → /token, as the
/// headless `bin/mcp-token` does. Returns the access token.
pub async fn login(base: &str) -> String {
    let http = http();
    let reg: Value = http
        .post(format!("{base}/register"))
        .json(&json!({ "redirect_uris": [REDIRECT] }))
        .send()
        .await
        .expect("register")
        .json()
        .await
        .expect("register json");
    let client_id = reg["client_id"].as_str().expect("client_id").to_string();

    let verifier = format!(
        "e2e-verifier-{}-padding-to-forty-three-chars",
        std::process::id()
    );
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let params = [
        ("response_type", "code"),
        ("client_id", client_id.as_str()),
        ("redirect_uri", REDIRECT),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", "e2e-state"),
    ];
    let page = http
        .get(format!("{base}/authorize"))
        .query(&params)
        .send()
        .await
        .expect("GET /authorize");
    assert_eq!(page.status(), StatusCode::OK, "consent page expected");
    assert!(
        page.headers().get(header::LOCATION).is_none(),
        "GET /authorize must never redirect with a code"
    );
    let form = consent_form(&page.text().await.expect("consent body"));

    let resp = http
        .post(format!("{base}/authorize"))
        .form(&form)
        .send()
        .await
        .expect("POST /authorize");
    assert!(
        resp.status().is_redirection(),
        "consent POST: {}",
        resp.status()
    );
    let location = resp.headers()[header::LOCATION].to_str().expect("location");
    let code = query_param(location, "code").expect("code in redirect");

    let tok: Value = http
        .post(format!("{base}/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("code_verifier", verifier.as_str()),
            ("redirect_uri", REDIRECT),
            ("client_id", client_id.as_str()),
        ])
        .send()
        .await
        .expect("token")
        .json()
        .await
        .expect("token json");
    tok["access_token"]
        .as_str()
        .expect("access_token")
        .to_string()
}

/// Fill the consent form: every named `<input>` keeps its value (hidden CSRF and
/// echoed OAuth params), the password field gets `PASS`, the other visible text
/// field gets `USER`. Field names are read from the page, not hardcoded.
fn consent_form(html: &str) -> HashMap<String, String> {
    let mut form = HashMap::new();
    for tag in html.split("<input").skip(1) {
        let tag = tag.split('>').next().unwrap_or_default();
        let Some(name) = attr(tag, "name") else {
            continue;
        };
        let value = match attr(tag, "type").as_deref() {
            Some("password") => PASS.to_string(),
            Some("text") | Some("email") | None => USER.to_string(),
            _ => attr(tag, "value").unwrap_or_default(),
        };
        form.insert(name, value);
    }
    assert!(!form.is_empty(), "no form inputs in consent page: {html}");
    form
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let start = tag.find(&format!("{name}=\""))? + name.len() + 2;
    let len = tag[start..].find('"')?;
    Some(html_unescape(&tag[start..start + len]))
}

fn html_unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        if k != key {
            return None;
        }
        urlencoding::decode(v).ok().map(|v| v.into_owned())
    })
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

pub async fn post_mcp(
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

/// Resident set size of `pid` in KiB, from /proc.
pub fn rss_kib(pid: u32) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).expect("proc status");
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
        .expect("VmRSS")
}
