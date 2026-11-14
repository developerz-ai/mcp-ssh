//! `/mcp` auth matrix, Host guard, consent-only code issuance, credential checks.
use std::io::Read;
use std::process::Stdio;
use std::time::Duration;

use reqwest::{StatusCode, header};
use serde_json::json;

use crate::client::http;
use crate::harness::{self, PASS, Server, USER};
use crate::oauth::{REDIRECT, login};

#[tokio::test]
async fn mcp_rejects_every_non_bearer_request() {
    let server = Server::start().await;
    let http = http();

    let health = http
        .get(format!("{}/healthz", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    assert_eq!(health.text().await.unwrap(), "ok");

    let url = format!("{}/mcp", server.base);
    let none = http.post(&url).send().await.unwrap();
    let basic = http
        .post(&url)
        .basic_auth(USER, Some(PASS))
        .send()
        .await
        .unwrap();
    let bogus = http
        .post(&url)
        .bearer_auth("not-a-token")
        .send()
        .await
        .unwrap();
    for (what, resp) in [("no auth", none), ("basic", basic), ("bad bearer", bogus)] {
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{what}");
        assert!(
            resp.headers().contains_key(header::WWW_AUTHENTICATE),
            "{what}: 401 must carry the resource-metadata challenge"
        );
    }
}

#[tokio::test]
async fn bad_host_with_a_good_bearer_is_forbidden() {
    let server = Server::start().await;
    let token = login(&server.base).await;
    let resp = http()
        .post(format!("{}/mcp", server.base))
        .bearer_auth(&token)
        .header(header::HOST, "evil.example")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .json(&json!({ "jsonrpc": "2.0", "id": 0, "method": "ping" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn get_authorize_with_basic_creds_never_issues_a_code() {
    let server = Server::start().await;
    let http = http();
    let reg: serde_json::Value = http
        .post(format!("{}/register", server.base))
        .json(&json!({ "redirect_uris": [REDIRECT] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let resp = http
        .get(format!("{}/authorize", server.base))
        .basic_auth(USER, Some(PASS))
        .query(&[
            ("response_type", "code"),
            ("client_id", reg["client_id"].as_str().unwrap()),
            ("redirect_uri", REDIRECT),
            (
                "code_challenge",
                "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
            ),
            ("code_challenge_method", "S256"),
            ("state", "e2e-state"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "consent page expected");
    assert!(!resp.text().await.unwrap().contains("code="));
}

#[tokio::test]
async fn empty_password_refuses_to_start() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = harness::command(dir.path(), harness::free_port(), &[("MCP_SSH_PASS", "")])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let exited = harness::wait_exit(&mut child, Duration::from_secs(10));
    if !exited {
        let _ = child.kill();
        let _ = child.wait();
    }
    assert!(exited, "server booted with an empty password");
    assert!(!child.wait().unwrap().success());
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        stderr.contains("no auth credentials"),
        "startup error must name the missing credentials: {stderr}"
    );
}
