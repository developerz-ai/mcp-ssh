//! Scrubber tests for `src/sentry.rs`.
use super::*;

#[test]
fn scrubs_password_assignment() {
    // `key=value` / `key: value` for the secret key set: the value is dropped,
    // never the key. Covers both separators and the short `pwd`/`pass` forms.
    for (input, secret) in [
        ("MCP_SSH_PASS=hunter2", "hunter2"),
        ("password: hunter2", "hunter2"),
        ("pwd=x", "x"),
    ] {
        let out = scrub_str(input);
        assert!(!out.contains(secret), "{input:?} leaked {secret:?}: {out}");
        assert!(out.contains(REDACTED), "{input:?} not redacted: {out}");
    }
}

#[test]
fn redacts_mcp_ssh_pass_env_value() {
    // MCP_SSH_PASS is the must-never-leak env; its value can never survive scrubbing.
    let out = scrub_str("MCP_SSH_PASS=hunter2");
    assert!(!out.contains("hunter2"));
    assert_eq!(out, "MCP_SSH_PASS=***REDACTED***");
}

#[test]
fn scrubs_user_pass_url() {
    let out = scrub_str("https://alice:s3cr3t@host.example/p");
    assert!(!out.contains("s3cr3t"), "password leaked: {out}");
    assert!(out.contains(REDACTED));
    // Userinfo user + host/path survive; only the password is blanked.
    assert!(out.contains("alice"));
    assert!(out.contains("host.example/p"));
}

#[test]
fn scrubs_url_passwords_containing_slash_or_at() {
    // Base64-ish secrets routinely contain `/`; the old pattern skipped these
    // entirely and the whole password leaked.
    let out = scrub_str("https://user:abc/def@host.example");
    assert!(!out.contains("abc/def"), "slash password leaked: {out}");
    assert!(out.contains(REDACTED));
    assert!(out.contains("host.example"), "host survives: {out}");

    // A password containing `@` previously leaked its tail (`ss` here).
    let out = scrub_str("https://u:p@ss@host.example");
    assert!(!out.contains("p@ss"), "at-sign password leaked: {out}");
    assert!(
        !out.contains("ss@host"),
        "password tail must not leak: {out}"
    );
    assert!(out.contains("host.example"), "host survives: {out}");
}

#[test]
fn scrubs_pem_private_key_block() {
    // Built from split fragments so the source never contains a scanner-detectable
    // private-key block; the runtime value still matches the PEM_BLOCK regex.
    let pem = concat!(
        "leading context\n",
        "-----BEGIN OPENSSH PRIVATE ",
        "KEY-----\n",
        "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAAAAAAAA\n",
        "-----END OPENSSH PRIVATE ",
        "KEY-----\n",
        "trailing context",
    );
    let out = scrub_str(pem);
    assert!(!out.contains("b3BlbnNza"), "key body leaked: {out}");
    assert!(!out.contains("BEGIN OPENSSH PRIVATE KEY"));
    assert!(!out.contains("END OPENSSH PRIVATE KEY"));
    assert!(out.contains(REDACTED));
    assert!(out.contains("leading context"));
    assert!(out.contains("trailing context"));
}

#[test]
fn scrubs_nested_secret_in_json() {
    let v = serde_json::json!({"auth": {"token": "xyz"}, "ok": "keep"});
    let out = scrub_value(v, &Scrubbers::build());
    assert!(
        !out.to_string().contains("xyz"),
        "secret leaked into JSON: {out}"
    );
    assert_eq!(out["ok"], "keep");
    assert_eq!(out["auth"]["token"], REDACTED);
}

#[test]
fn scrub_value_descends_arrays() {
    // The Array arm must redact URL-bearing strings and recurse into nested objects.
    let v = serde_json::json!({ "items": ["ssh://u:pw@h", "plain", {"token": "t"}] });
    let out = scrub_value(v, &Scrubbers::build());
    assert_eq!(out["items"][0], "ssh://u:***REDACTED***@h");
    assert_eq!(out["items"][1], "plain");
    assert_eq!(out["items"][2]["token"], REDACTED);
}

#[test]
fn parse_dsn_disabled_when_unset_or_malformed() {
    assert!(parse_dsn("").is_none());
    assert!(parse_dsn("garbage").is_none());
    // Throwaway valid DSN shape (NOT the real ingest key) parses to Some.
    assert!(parse_dsn("https://deadbeefdeadbeefdeadbeefdeadbeef@example.invalid/1").is_some());
}

#[test]
fn init_is_noop_when_dsn_unset() {
    // init() with no SENTRY_DSN binds a *disabled* client so the app still runs.
    // We don't mutate env (racy under parallel tests); only assert in the normal
    // test/CI state where SENTRY_DSN is unset. The guard unbinds the client on drop.
    if std::env::var_os("SENTRY_DSN").is_some() {
        return;
    }
    let guard = init();
    assert!(
        !guard.is_enabled(),
        "client must be disabled when DSN is unset"
    );
}

#[test]
fn leaves_unrelated_text_intact() {
    assert_eq!(scrub_str("nothing to see here"), "nothing to see here");
    assert_eq!(scrub_str("user=alice port=22"), "user=alice port=22");
}

#[test]
fn paranoid_mode_redacts_everything() {
    // If a regex ever fails to compile, the scrubber fails closed: redact all.
    let s = Scrubbers {
        url: Regex::new("").unwrap(),
        secret: Regex::new("").unwrap(),
        pem: Regex::new("").unwrap(),
        paranoid: true,
    };
    assert_eq!(s.scrub("any old text"), REDACTED);
}

#[test]
fn is_secret_key_matches_case_insensitively() {
    assert!(is_secret_key("PASSWORD"));
    assert!(is_secret_key("Db_Passwd"));
    assert!(is_secret_key("api_token"));
    assert!(is_secret_key("x-dsn"));
    // Substring bias: a stray "compass" redacts rather than risk a leak.
    assert!(is_secret_key("compass"));
    assert!(!is_secret_key("hostname"));
}

#[test]
fn scrub_event_redacts_all_fields() {
    let mut event = Event {
        message: Some("login password=hunter2 failed".into()),
        ..Default::default()
    };
    let ex = sentry::protocol::Exception {
        ty: "Token=leaked boom".into(),
        value: Some("see https://u:secret@host".into()),
        ..Default::default()
    };
    event.exception.values.push(ex);
    let mut crumb = sentry::protocol::Breadcrumb {
        message: Some("connecting to https://key:abc@s/10".into()),
        ..Default::default()
    };
    crumb
        .data
        .insert("pass".into(), Value::String("p@ss".into()));
    event.breadcrumbs.values.push(crumb);
    event
        .extra
        .insert("password".into(), Value::String("x".into()));

    scrub_event(&mut event, &Scrubbers::build());

    assert_eq!(
        event.message.as_deref(),
        Some("login password=***REDACTED***")
    );
    let ex = &event.exception.values[0];
    assert_eq!(ex.ty, "Token=***REDACTED***");
    assert_eq!(
        ex.value.as_deref(),
        Some("see https://u:***REDACTED***@host")
    );
    let crumb = &event.breadcrumbs.values[0];
    assert_eq!(
        crumb.message.as_deref(),
        Some("connecting to https://key:***REDACTED***@s/10")
    );
    assert_eq!(crumb.data["pass"], Value::String(REDACTED.to_string()));
    assert_eq!(event.extra["password"], Value::String(REDACTED.to_string()));
}

#[test]
fn scrub_event_drops_identifying_fields() {
    use sentry::protocol::{Frame, LogEntry, Request, Stacktrace, User};

    let mut headers = sentry::protocol::Map::new();
    headers.insert("Authorization".into(), "Bearer tok123".into());
    let mut other = sentry::protocol::Map::new();
    other.insert("note".into(), Value::String("pass=ctx-secret".into()));
    let mut frame = Frame::default();
    frame
        .vars
        .insert("token".into(), Value::String("frame-secret".into()));
    let mut event = Event {
        request: Some(Request {
            headers,
            ..Default::default()
        }),
        user: Some(User {
            ip_address: Some(sentry::protocol::IpAddress::Auto),
            ..Default::default()
        }),
        server_name: Some("box.internal".into()),
        logentry: Some(LogEntry {
            message: "token=log-secret".into(),
            params: vec![Value::String("pass=param-secret".into())],
        }),
        stacktrace: Some(Stacktrace {
            frames: vec![frame],
            ..Default::default()
        }),
        ..Default::default()
    };
    event.tags.insert("api_token".into(), "tag-secret".into());
    event
        .tags
        .insert("note".into(), "password=tag2-secret".into());
    event
        .contexts
        .insert("custom".into(), Context::Other(other));
    event
        .contexts
        .insert("os".into(), Context::Os(Box::default()));

    scrub_event(&mut event, &Scrubbers::build());

    assert!(event.request.is_none());
    assert!(event.user.is_none());
    assert!(event.server_name.is_none());
    assert!(event.contexts.contains_key("os"), "system context kept");
    assert!(!event.contexts.contains_key("custom"));
    let out = serde_json::to_string(&event).expect("serialize");
    for secret in [
        "tok123",
        "box.internal",
        "log-secret",
        "param-secret",
        "frame-secret",
        "tag-secret",
        "tag2-secret",
        "ctx-secret",
    ] {
        assert!(!out.contains(secret), "{secret} leaked: {out}");
    }
}

#[test]
fn scrubs_authorization_and_cookie_secrets() {
    // Regression: `authorization`/`cookie` structured keys redact by key name…
    let v = serde_json::json!({
        "authorization": "Bearer abc123",
        "headers": { "cookie": "sid=s3cr3t; theme=dark" },
    });
    let out = scrub_value(v, &Scrubbers::build());
    assert!(!out.to_string().contains("abc123"), "bearer leaked: {out}");
    assert!(!out.to_string().contains("s3cr3t"), "cookie leaked: {out}");
    assert_eq!(out["authorization"], REDACTED);
    assert_eq!(out["headers"]["cookie"], REDACTED);

    // …and the free-text forms redact in full — including the token after "Bearer",
    // not just the single word "Bearer".
    let out = scrub_str("Authorization: Bearer abc123");
    assert!(
        !out.contains("abc123"),
        "bearer token leaked in text: {out}"
    );
    assert!(out.contains(REDACTED));

    let out = scrub_str("Cookie: sid=s3cr3t");
    assert!(
        !out.contains("s3cr3t"),
        "cookie value leaked in text: {out}"
    );
    assert!(out.contains(REDACTED));
}
