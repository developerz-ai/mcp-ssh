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
use sentry::protocol::{Context, Event};
use sentry::types::Dsn;
use sentry::{BeforeCallback, ClientOptions};
use serde_json::Value;
use tracing::warn;

/// Literal every redaction replaces a secret with.
const REDACTED: &str = "***REDACTED***";

/// Key names whose value is always secret (case-insensitive). Mirrors the free-text
/// regex so structured JSON keys and ad-hoc `key=value` text agree on what's secret.
const SECRET_KEYS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "dsn",
    "pwd",
    "pass",
    "authorization",
    "cookie",
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

/// Redact the human-readable + JSON fields of an outbound event in place, and drop
/// the fields that only ever identify the host or a client (request, user,
/// hostname, template, non-system contexts). Fail closed: anything not known to be
/// harmless is removed rather than scrubbed.
fn scrub_event(event: &mut Event<'_>, scrubbers: &Scrubbers) {
    event.request = None;
    event.user = None;
    event.server_name = None;
    event.template = None;
    event
        .contexts
        .retain(|_, c| matches!(c, Context::Os(_) | Context::Runtime(_) | Context::Device(_)));

    for (k, v) in event.tags.iter_mut() {
        *v = if is_secret_key(k) {
            REDACTED.to_string()
        } else {
            scrubbers.scrub(v)
        };
    }

    if let Some(entry) = event.logentry.as_mut() {
        entry.message = scrubbers.scrub(&entry.message);
        for p in entry.params.iter_mut() {
            *p = scrub_value(std::mem::take(p), scrubbers);
        }
    }

    let traces = event
        .exception
        .values
        .iter_mut()
        .flat_map(|ex| ex.stacktrace.as_mut())
        .chain(event.stacktrace.as_mut())
        .chain(
            event
                .threads
                .values
                .iter_mut()
                .flat_map(|t| t.stacktrace.as_mut()),
        );
    for trace in traces {
        for frame in trace.frames.iter_mut() {
            frame.vars.clear();
        }
    }

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

    // sentry 0.49 made `ClientOptions` `#[non_exhaustive]`: build from `new()`
    // and set fields, with tracing off through the builder.
    let mut opts = ClientOptions::new()
        .environment(environment)
        .release(env!("CARGO_PKG_VERSION"))
        .send_default_pii(false)
        .traces_sample_rate(0.0);
    opts.dsn = dsn;
    opts.before_send = Some(before_send);

    sentry::init(opts)
}

/// Capture a `std::error::Error` to GlitchTip (no-op when the client is disabled —
/// DSN unset). Thin forwarder: `main` names the local `sentry` module, which
/// shadows the SDK crate, so it can't call `sentry::capture_error` directly.
pub(crate) fn capture_error(error: &dyn std::error::Error) {
    sentry::capture_error(error);
}

// --- patterns ---------------------------------------------------------------
// URL credentials: `scheme://user:pass@host` → blank the password (capture 3).
//   1 = scheme://, 2 = user, 3 = password, 4 = @. Userinfo without a password
//   (no `:`) doesn't match, so `ssh://host` is left untouched. The password is
//   `[^\s]+` — greedy, so it backtracks to the LAST `@` in the token: secrets
//   containing `/` (base64) or `@` are fully blanked. A `@` later in the
//   host/path over-redacts up to it, which is the documented bias — the old
//   `[^\s/@]+` instead leaked such passwords entirely (no match) or partially.
const URL_CREDS: &str = r#"(?i)([a-z][a-z0-9+.-]*://)([^\s/@:]*):([^\s]+)(@)"#;
// `key=value` / `key: value` for secret keys (case-insensitive). 1 = key+separator,
// value (capture 2) is dropped. Structured JSON keys are handled separately by
// `scrub_value`; this covers free text in messages/exceptions/breadcrumbs. The value
// group spans to end of line so multi-word secrets like `Authorization: Bearer <tok>`
// redact in full — bias toward over-redaction rather than leak a trailing token.
const SECRET_KV: &str =
    r#"(?i)((?:authorization|cookie|password|passwd|secret|token|dsn|pwd|pass)\s*[:=]\s*)(.+)"#;
// PEM private-key blocks, dotall so `.` spans newlines. Covers RSA/EC/OPENSSH/PRIVATE.
const PEM_BLOCK: &str =
    r#"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----"#;

#[cfg(test)]
mod tests;
