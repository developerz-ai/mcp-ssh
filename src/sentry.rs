//! GlitchTip (Sentry-protocol) error tracking: init + a fail-closed `before_send`
//! scrubber. Error tracking ONLY — no traces, no profiling, no PII.
//!
//! This server handles SSH credentials (`MCP_SSH_PASS`, keys, host/user info), so
//! every outgoing event is redacted before it leaves the process. Scrubbing is
//! paranoid by construction: if a redaction regex ever fails to compile, the whole
//! string is replaced rather than risk a leak.
//!
//! Init is owned by `main` (it binds the guard for the program lifetime); this
//! module never logs or returns secrets.

use std::sync::Arc;

use regex::{Captures, Regex};
use sentry::protocol::Event;
use sentry::types::Dsn;
use sentry::{BeforeCallback, ClientOptions};
use serde_json::Value;
use tracing::warn;

/// Literal every redaction replaces a secret with.
const REDACTED: &str = "***REDACTED***";

/// Key names whose value is always secret (case-insensitive). Mirrors the free-text
/// regex so structured JSON keys and ad-hoc `key=value` text agree on what's secret.
const SECRET_KEYS: &[&str] = &[
    "password", "passwd", "secret", "token", "dsn", "pwd", "pass",
];

/// Compiled redaction patterns. Built once; the `before_send` closure shares one
/// instance via `Arc`. If any pattern fails to compile, `paranoid` flips on and every
/// string becomes `REDACTED` — fail closed, never leak.
struct Scrubbers {
    url: Regex,
    secret: Regex,
    pem: Regex,
    paranoid: bool,
}

impl Scrubbers {
    /// Compile all three patterns; on any failure return a paranoid scrubber.
    fn build() -> Self {
        let url = Regex::new(URL_CREDS);
        let secret = Regex::new(SECRET_KV);
        let pem = Regex::new(PEM_BLOCK);
        match (url, secret, pem) {
            (Ok(url), Ok(secret), Ok(pem)) => Self {
                url,
                secret,
                pem,
                paranoid: false,
            },
            _ => {
                // Should be unreachable — patterns are compile-time literals — but a
                // future edit could break one. Fail closed: redact everything.
                warn!("sent scrubber regex failed to compile; entering paranoid mode");
                Self::paranoid()
            }
        }
    }

    fn paranoid() -> Self {
        // Harmless placeholder patterns; `paranoid` short-circuits before they run.
        let blank = || Regex::new("").unwrap_or_else(|_| Regex::new("$^").unwrap());
        Self {
            url: blank(),
            secret: blank(),
            pem: blank(),
            paranoid: true,
        }
    }

    /// Apply all three redactions to a single string.
    fn scrub(&self, s: &str) -> String {
        if self.paranoid {
            return REDACTED.to_string();
        }
        let s = self.url.replace_all(s, |c: &Captures<'_>| {
            format!("{}{}:{REDACTED}{}", &c[1], &c[2], &c[4])
        });
        let s = self
            .secret
            .replace_all(&s, |c: &Captures<'_>| format!("{}{REDACTED}", &c[1]));
        self.pem.replace_all(&s, REDACTED).into_owned()
    }
}

/// Pure redaction of a single string — the testable unit. Rebuilds the patterns each
/// call; cheap, and keeps the function self-contained. Runtime events instead reuse a
/// single cached `Scrubbers` via `init`'s `Arc`.
#[cfg(test)]
fn scrub_str(s: &str) -> String {
    Scrubbers::build().scrub(s)
}

/// Recursively redact a JSON value: secret-keyed object values are blanked, string
/// leaves run through the free-text scrubber, objects/arrays descended into.
fn scrub_value(value: Value, scrubbers: &Scrubbers) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| {
                    let v = if is_secret_key(&k) {
                        Value::String(REDACTED.to_string())
                    } else {
                        scrub_value(v, scrubbers)
                    };
                    (k, v)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|v| scrub_value(v, scrubbers))
                .collect(),
        ),
        Value::String(s) => Value::String(scrubbers.scrub(&s)),
        other => other,
    }
}

/// Redact the human-readable + JSON fields of an outbound event in place.
fn scrub_event(event: &mut Event<'_>, scrubbers: &Scrubbers) {
    event.message = event.message.take().map(|m| scrubbers.scrub(&m));

    for ex in event.exception.values.iter_mut() {
        ex.ty = scrubbers.scrub(&ex.ty);
        ex.value = ex.value.take().map(|v| scrubbers.scrub(&v));
    }

    for crumb in event.breadcrumbs.values.iter_mut() {
        crumb.message = crumb.message.take().map(|m| scrubbers.scrub(&m));
        for (k, v) in crumb.data.iter_mut() {
            *v = if is_secret_key(k) {
                Value::String(REDACTED.to_string())
            } else {
                scrub_value(std::mem::take(v), scrubbers)
            };
        }
    }

    for (k, v) in event.extra.iter_mut() {
        *v = if is_secret_key(k) {
            Value::String(REDACTED.to_string())
        } else {
            scrub_value(std::mem::take(v), scrubbers)
        };
    }
}

/// True if `key` (case-insensitive) names a secret field. Substring match biases
/// toward over-redaction: a stray `"compass"` is preferable to a leaked password.
fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    SECRET_KEYS.iter().any(|s| key.contains(s))
}

/// Parse a DSN string, returning `None` on anything malformed (no panic).
fn parse_dsn(s: &str) -> Option<Dsn> {
    s.parse::<Dsn>().ok()
}

/// Initialize Sentry/GlitchTip. Reads `SENTRY_DSN` at runtime (never hardcoded);
/// unset/empty/malformed → a disabled no-op client, and the app still runs. The
/// returned guard must be held for the whole program lifetime (bound in `main`).
pub(crate) fn init() -> sentry::ClientInitGuard {
    let dsn = std::env::var("SENTRY_DSN")
        .ok()
        .filter(|s| !s.is_empty())
        .as_deref()
        .and_then(parse_dsn);

    let environment = std::env::var("SENTRY_ENVIRONMENT")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "development".to_string());

    let scrubbers = Arc::new(Scrubbers::build());
    let before_send: BeforeCallback<Event<'static>> = {
        let scrubbers = Arc::clone(&scrubbers);
        Arc::new(move |mut event| {
            scrub_event(&mut event, &scrubbers);
            Some(event)
        })
    };

    let opts = ClientOptions {
        dsn,
        environment: Some(environment.into()),
        release: Some(env!("CARGO_PKG_VERSION").into()),
        send_default_pii: false,
        traces_sample_rate: 0.0,
        before_send: Some(before_send),
        ..Default::default()
    };

    sentry::init(opts)
}

// --- patterns ---------------------------------------------------------------
// URL credentials: `scheme://user:pass@host` → blank the password (capture 3).
//   1 = scheme://, 2 = user, 3 = password, 4 = @. Userinfo without a password
//   (no `:`) doesn't match, so `ssh://host` is left untouched.
const URL_CREDS: &str = r#"(?i)([a-z][a-z0-9+.-]*://)([^\s/@:]*):([^\s/@]+)(@)"#;
// `key=value` / `key: value` for secret keys (case-insensitive). 1 = key+separator,
// value (capture 2) is dropped. Structured JSON keys are handled separately by
// `scrub_value`; this covers free text in messages/exceptions/breadcrumbs.
const SECRET_KV: &str = r#"(?i)((?:password|passwd|secret|token|dsn|pwd|pass)\s*[:=]\s*)(\S+)"#;
// PEM private-key blocks, dotall so `.` spans newlines. Covers RSA/EC/OPENSSH/PRIVATE.
const PEM_BLOCK: &str =
    r#"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----"#;

#[cfg(test)]
mod tests {
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
    fn scrubs_pem_private_key_block() {
        let pem = "leading context\n\
                   -----BEGIN OPENSSH PRIVATE KEY-----\n\
                   b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAAAAAAAA\n\
                   -----END OPENSSH PRIVATE KEY-----\n\
                   trailing context";
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
            Some("login password=***REDACTED*** failed")
        );
        let ex = &event.exception.values[0];
        assert_eq!(ex.ty, "Token=***REDACTED*** boom");
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
}
