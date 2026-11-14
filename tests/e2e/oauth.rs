//! The OAuth 2.1 login a headless client runs: register → consent page (GET
//! /authorize) → consent POST → /token with PKCE, as `bin/mcp-token` does.
use std::collections::HashMap;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::{StatusCode, header};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::client::http;
use crate::harness::{PASS, USER};

pub const REDIRECT: &str = "http://localhost/cb";

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
