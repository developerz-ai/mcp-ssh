//! Human-readable job ids: an agent-supplied `title` (or the neutral `job`
//! fallback) plus the local `HH-MM-SS` it started — e.g. `claudetm-doing-mvp-03-01-05`
//! or `job-23-30-07`. The time uses `-`, not `:`: the id *is* the `<id>.log`
//! filename, and a colon, though legal on Linux, is a portability/tooling hazard
//! (Windows forbids it, scp/globbing trip on it) — dashes are safe everywhere. The
//! title lets the agent tell its own jobs apart at a glance (`job(list)` shows what
//! each one is doing). It is NEVER derived from the command: command text can carry
//! a secret in its leading tokens, and the id surfaces in replies, `job(list)`,
//! reaper logs, and the log filename. The title comes from a dedicated `bash` param
//! the agent writes deliberately, and is normalized to a single `[A-Za-z0-9_-]+`
//! path component (so it can't inject shell/path metacharacters into the log
//! filename or escape the job dir). A monotonic sequence suffix disambiguates jobs
//! starting within the same second.

use std::borrow::Borrow;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Local;

/// Cap the sanitized title so ids (and the `<id>.log` filename) stay bounded and
/// readable no matter what the agent passes.
const MAX_TITLE_LEN: usize = 32;

/// A human-readable job identifier: `<label>-<HH-MM-SS>` with an optional `-<seq>`
/// collision suffix, where `<label>` is a normalized title or `job`. A generated id
/// matches `^[A-Za-z0-9_-]+-\d{2}-\d{2}-\d{2}(-\d+)?$`; ids wrapped from client
/// input for lookup are not revalidated.
#[derive(Debug, Clone, Hash, Eq, PartialEq, PartialOrd, Ord, serde::Serialize)]
pub struct JobId(String);

impl JobId {
    /// Build an id from an optional `title` and the current local time. The title
    /// is normalized to one `[A-Za-z0-9_-]+` component (empty/garbage falls back to
    /// the neutral `job` label), so neither command text nor path/shell
    /// metacharacters can reach the id or its log filename. `exists` reports
    /// whether a candidate is taken; only
    /// on a clash do we append a `-<seq>` from `seq`, so clean ids stay clean.
    pub fn generate(seq: &AtomicU64, title: Option<&str>, exists: impl Fn(&str) -> bool) -> Self {
        let label = title
            .and_then(normalize_title)
            .unwrap_or_else(|| "job".to_string());
        let base = format!("{label}-{}", Local::now().format("%H-%M-%S"));
        if !exists(&base) {
            return Self(base);
        }
        // Same second + same title: a monotonic suffix keeps the id unique without
        // sacrificing readability.
        let n = seq.fetch_add(1, Ordering::Relaxed);
        Self(format!("{base}-{n}"))
    }
}

/// Keep the agent's title essentially as-is — just guard the one thing that
/// matters: the id becomes the `<id>.log` filename, so the title must stay a
/// single, bounded path component. Alphanumerics, `-` and `_` pass through
/// unchanged; any other run (spaces, `/`, `.`, `:`) collapses to one `-` so it
/// can't break the filename or escape the job dir. Trimmed and capped at
/// `MAX_TITLE_LEN`; `None` when nothing usable survives (caller falls back to `job`).
fn normalize_title(raw: &str) -> Option<String> {
    let mut out = String::new();
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= MAX_TITLE_LEN {
            break;
        }
    }
    let trimmed = out.trim_matches('-');
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for JobId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Lets a `HashMap<JobId, _>` be probed with a `&str` key — used when generating
/// an id to test a candidate against the live ids.
impl Borrow<str> for JobId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// Wrap a client-supplied id for lookup. Not validated — an unknown or malformed
/// id simply matches no job; only `generate` mints well-formed ids.
impl From<String> for JobId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for JobId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirror the documented `^<label>-\d{2}-\d{2}-\d{2}(-\d+)?$` shape without a
    /// regex dependency. Everything is `-`-separated now, so strip the exact
    /// `<label>-` prefix (the label may itself contain dashes) and check the
    /// remainder is `HH-MM-SS` or `HH-MM-SS-<seq>`.
    fn valid_format(s: &str, expected_label: &str) -> bool {
        let two_digits = |x: &str| x.len() == 2 && x.bytes().all(|b| b.is_ascii_digit());
        let digits = |x: &str| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit());
        let Some(rest) = s.strip_prefix(&format!("{expected_label}-")) else {
            return false;
        };
        match rest.split('-').collect::<Vec<_>>().as_slice() {
            [hh, mm, ss] => two_digits(hh) && two_digits(mm) && two_digits(ss),
            [hh, mm, ss, seq] => two_digits(hh) && two_digits(mm) && two_digits(ss) && digits(seq),
            _ => false,
        }
    }

    #[test]
    fn generated_id_matches_documented_format() {
        let seq = AtomicU64::new(1);
        let id = JobId::generate(&seq, None, |_| false);
        assert!(
            valid_format(id.as_ref(), "job"),
            "bad format: {}",
            id.as_ref()
        );
    }

    #[test]
    fn title_becomes_the_label() {
        let seq = AtomicU64::new(1);
        let id = JobId::generate(&seq, Some("claudetm doing mvp"), |_| false);
        let s = id.as_ref();
        assert!(s.starts_with("claudetm-doing-mvp-"), "title not in id: {s}");
        assert!(valid_format(s, "claudetm-doing-mvp"), "bad format: {s}");
    }

    #[test]
    fn title_normalization_guards_the_filename() {
        // Path separators / dots / colons collapse to single dashes so the id stays
        // one bounded filename component; case and `-`/`_` are preserved.
        let seq = AtomicU64::new(1);
        let id = JobId::generate(&seq, Some("../../etc/Pa ss:wd"), |_| false);
        let s = id.as_ref();
        assert!(!s.contains('/'), "slash must not survive: {s}");
        assert!(!s.contains(".."), "traversal must not survive: {s}");
        assert!(
            s.starts_with("etc-Pa-ss-wd-"),
            "unexpected normalization: {s}"
        );
    }

    #[test]
    fn blank_or_symbolic_title_falls_back_to_job() {
        let seq = AtomicU64::new(1);
        for raw in ["", "   ", "/// ...", "!!!"] {
            let id = JobId::generate(&seq, Some(raw), |_| false);
            assert!(
                id.as_ref().starts_with("job-"),
                "empty title must fall back to `job`: {} ({raw:?})",
                id.as_ref()
            );
        }
    }

    #[test]
    fn no_collision_leaves_id_clean_and_seq_untouched() {
        let seq = AtomicU64::new(1);
        let id = JobId::generate(&seq, None, |_| false);
        let s = id.as_ref();
        assert!(valid_format(s, "job"));
        // No trailing `-<seq>`: after the `job-` label the remainder is exactly the
        // three time components `HH-MM-SS`, nothing more.
        let after_label = s.strip_prefix("job-").unwrap();
        assert_eq!(
            after_label.split('-').count(),
            3,
            "unexpected collision suffix: {s}"
        );
        assert_eq!(
            seq.load(Ordering::Relaxed),
            1,
            "seq advanced without a clash"
        );
    }

    #[test]
    fn collision_appends_sequence_suffix() {
        let seq = AtomicU64::new(1);
        // `exists` always true => the base is "taken", forcing the suffix path.
        let id = JobId::generate(&seq, None, |_| true);
        let s = id.as_ref();
        assert!(valid_format(s, "job"));
        assert!(s.ends_with("-1"), "expected `-1` seq suffix, got {s}");
        assert_eq!(seq.load(Ordering::Relaxed), 2, "clash must advance seq");
    }

    #[test]
    fn display_and_as_ref_agree() {
        let seq = AtomicU64::new(1);
        let id = JobId::generate(&seq, None, |_| false);
        assert_eq!(id.to_string(), id.as_ref());
    }

    #[test]
    fn serializes_as_a_plain_string() {
        let seq = AtomicU64::new(1);
        let id = JobId::generate(&seq, None, |_| false);
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, format!("\"{}\"", id.as_ref()));
    }
}
