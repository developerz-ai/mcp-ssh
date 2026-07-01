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

// Nothing here is referenced until `main` binds the guard (next change: init must
// run before the tokio runtime starts, so the wiring lands with that refactor).
// Drop this allow once `init()` is called from `main`.
#![allow(dead_code)]

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
pub(crate) fn scrub_str(s: &str) -> String {
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
    fn redacts_password_key_value() {
        assert_eq!(scrub_str("password=hunter2"), "password=***REDACTED***");
        assert_eq!(
            scrub_str("db password: s3cr3t!"),
            "db password: ***REDACTED***"
        );
        assert_eq!(
            scrub_str("TOKEN=abc.def.ghi end"),
            "TOKEN=***REDACTED*** end"
        );
        // MCP_SSH_PASS-shaped leakage.
        assert_eq!(
            scrub_str("MCP_SSH_PASS=letmein"),
            "MCP_SSH_PASS=***REDACTED***"
        );
    }

    #[test]
    fn redacts_url_credentials() {
        assert_eq!(
            scrub_str("https://alice:hunter2@example.com/path"),
            "https://alice:***REDACTED***@example.com/path",
        );
        // No password → untouched.
        assert_eq!(
            scrub_str("ssh://alice@example.com"),
            "ssh://alice@example.com"
        );
        // Password containing colons is fully redacted up to the host.
        assert_eq!(scrub_str("ftp://u:a:b@c"), "ftp://u:***REDACTED***@c");
    }

    #[test]
    fn redacts_pem_private_key_block() {
        let pem = "preamble\n\
                   -----BEGIN RSA PRIVATE KEY-----\n\
                   MIIEpAIBAAKCAQEA0Z3VS5Jo0...\n\
                   -----END RSA PRIVATE KEY-----\n\
                   trailer";
        let out = scrub_str(pem);
        assert!(
            !out.contains("MIIEpAIBAAKCAQEA0Z3VS5Jo0"),
            "key body leaked: {out}"
        );
        assert!(!out.contains("BEGIN RSA PRIVATE KEY"));
        assert!(out.contains(REDACTED));
        assert!(out.contains("preamble"));
        assert!(out.contains("trailer"));
    }

    #[test]
    fn leaves_unrelated_text_intact() {
        assert_eq!(scrub_str("nothing to see here"), "nothing to see here");
        assert_eq!(scrub_str("user=alice port=22"), "user=alice port=22");
    }

    #[test]
    fn paranoid_mode_redacts_everything() {
        let s = Scrubbers {
            url: Regex::new("").unwrap(),
            secret: Regex::new("").unwrap(),
            pem: Regex::new("").unwrap(),
            paranoid: true,
        };
        assert_eq!(s.scrub("any old text"), REDACTED);
    }

    #[test]
    fn parse_dsn_rejects_invalid_and_accepts_valid() {
        // A valid ingest DSN shape (public test key) parses.
        assert!(
            parse_dsn("https://16a3cee40cba4b40a62a3e2b5ad1ea6f@glitchtip.example/10").is_some()
        );
        // Malformed → None, no panic.
        assert!(parse_dsn("not a dsn").is_none());
        assert!(parse_dsn("https://example/10").is_none());
        assert!(parse_dsn("").is_none());
    }

    #[test]
    fn no_op_when_dsn_unset() {
        // init() with no SENTRY_DSN yields a disabled client (the whole point: the
        // app keeps running). We can't easily call init() without polluting the global
        // hub, so assert the decision logic it relies on: an unset/empty DSN resolves
        // to None.
        let resolved = std::env::var("SENTRY_DSN")
            .ok()
            .filter(|s| !s.is_empty())
            .as_deref()
            .and_then(parse_dsn);
        // In the test env SENTRY_DSN is unset → disabled.
        if std::env::var_os("SENTRY_DSN").is_none() {
            assert!(resolved.is_none(), "expected disabled client");
        }
    }

    #[test]
    fn is_secret_key_matches_case_insensitively() {
        assert!(is_secret_key("PASSWORD"));
        assert!(is_secret_key("Db_Passwd"));
        assert!(is_secret_key("api_token"));
        assert!(is_secret_key("x-dsn"));
        assert!(!is_secret_key("hostname"));
    }

    #[test]
    fn scrub_value_redacts_secret_keyed_json() {
        let scrubbers = Scrubbers::build();
        let v = serde_json::json!({
            "user": "alice",
            "password": "hunter2",
            "nested": { "TOKEN": "abc" },
            "list": ["ssh://u:pw@h", "plain"],
        });
        let out = scrub_value(v, &scrubbers);
        assert_eq!(out["user"], "alice");
        assert_eq!(out["password"], REDACTED);
        assert_eq!(out["nested"]["TOKEN"], REDACTED);
        assert_eq!(out["list"][0], "ssh://u:***REDACTED***@h");
        assert_eq!(out["list"][1], "plain");
    }

    #[test]
    fn scrub_event_redacts_fields() {
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
        // URL creds redacted in the breadcrumb message.
        assert_eq!(
            crumb.message.as_deref(),
            Some("connecting to https://key:***REDACTED***@s/10")
        );
        assert_eq!(crumb.data["pass"], Value::String(REDACTED.to_string()));
        assert_eq!(event.extra["password"], Value::String(REDACTED.to_string()));
    }
}
