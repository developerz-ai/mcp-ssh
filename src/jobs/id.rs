//! Human-readable job ids: a neutral `job` label plus the local `HH:MM` it
//! started, e.g. `job-23:30` — easier to refer to in conversation than an opaque
//! counter. The id is deliberately free of command text: a command can carry a
//! secret in its leading tokens, and the id surfaces in replies, `job(list)`,
//! reaper logs, and the log filename, so slugging the command would leak it.
//! A monotonic sequence suffix disambiguates jobs starting within the same minute.

use std::borrow::Borrow;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Local;

/// Max slug length before the `-HH:MM` suffix. Short enough to type, long enough
/// to carry the gist of the command.
const MAX_SLUG: usize = 24;

/// A human-readable job identifier: `<slug>-<HH:MM>` with an optional `-<seq>`
/// collision suffix. A generated id matches `^[a-z0-9-]+-\d{2}:\d{2}(-\d+)?$`;
/// ids wrapped from client input for lookup are not revalidated.
#[derive(Debug, Clone, Hash, Eq, PartialEq, PartialOrd, Ord, serde::Serialize)]
pub struct JobId(String);

impl JobId {
    /// Build an id from a neutral `label` and the current local time. Callers
    /// pass a fixed label (e.g. `"job"`), never raw command text, so a secret on
    /// the command line can't leak into the id. `exists` reports whether a
    /// candidate is already taken; only on a clash do we append a `-<seq>` drawn
    /// from `seq`, so clean ids stay clean. The predicate keeps this decoupled
    /// from however the caller tracks live ids.
    pub fn generate(label: &str, seq: &AtomicU64, exists: impl Fn(&str) -> bool) -> Self {
        let base = format!("{}-{}", slug(label), Local::now().format("%H:%M"));
        if !exists(&base) {
            return Self(base);
        }
        // Same slug, same minute: a monotonic suffix keeps the id unique without
        // sacrificing readability.
        let n = seq.fetch_add(1, Ordering::Relaxed);
        Self(format!("{base}-{n}"))
    }
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

/// Sanitize a label into a lowercase `[a-z0-9-]` slug so any caller-supplied
/// string yields a well-formed id: take the first few tokens, collapse runs of
/// non-alphanumerics to single dashes, trim them off both ends, and cap the
/// length. Falls back to `job` when nothing alphanumeric survives.
fn slug(label: &str) -> String {
    let head = label
        .split_whitespace()
        .take(3)
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = String::with_capacity(head.len());
    let mut prev_dash = false;
    for ch in head.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    // Cap first, then trim: the cut can land mid-run and leave a trailing dash.
    let capped: String = out.trim_matches('-').chars().take(MAX_SLUG).collect();
    let trimmed = capped.trim_end_matches('-');
    if trimmed.is_empty() {
        "job".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirror the documented `^[a-z0-9-]+-\d{2}:\d{2}(-\d+)?$` shape without
    /// pulling in a regex dependency. The id has exactly one `:` (slug and
    /// suffix never contain one), so split on it to isolate the time.
    fn valid_format(s: &str) -> bool {
        let two_digits = |x: &str| x.len() == 2 && x.bytes().all(|b| b.is_ascii_digit());
        let Some((left, right)) = s.split_once(':') else {
            return false;
        };
        let Some((prefix, hh)) = left.rsplit_once('-') else {
            return false;
        };
        let (mm, suffix_ok) = match right.split_once('-') {
            Some((mm, sfx)) => (
                mm,
                !sfx.is_empty() && sfx.bytes().all(|b| b.is_ascii_digit()),
            ),
            None => (right, true),
        };
        two_digits(hh)
            && two_digits(mm)
            && suffix_ok
            && !prefix.is_empty()
            && prefix
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    }

    #[test]
    fn slug_lowercases_and_replaces_non_alnum() {
        assert_eq!(slug("ECHO Hello/World"), "echo-hello-world");
    }

    #[test]
    fn slug_takes_first_three_tokens_and_collapses_dashes() {
        // 4th token dropped; the space-then-`-` run collapses to one dash.
        assert_eq!(slug("git commit -m message extra"), "git-commit-m");
    }

    #[test]
    fn slug_caps_length_at_24() {
        let s = slug("abcdefghijklmnopqrstuvwxyz0123456789");
        assert_eq!(s.len(), 24);
        assert_eq!(s, "abcdefghijklmnopqrstuvwx");
    }

    #[test]
    fn slug_trims_dash_left_by_the_cap() {
        // 23 chars then a separator: the cut lands on char 24, leaving a dash.
        let input = format!("{} b", "a".repeat(23));
        assert_eq!(slug(&input), "a".repeat(23));
    }

    #[test]
    fn slug_falls_back_when_nothing_alphanumeric() {
        assert_eq!(slug(""), "job");
        assert_eq!(slug("!@#$ %^&*"), "job");
    }

    #[test]
    fn generated_id_matches_documented_format() {
        let seq = AtomicU64::new(1);
        let id = JobId::generate("cargo build --release", &seq, |_| false);
        assert!(valid_format(id.as_ref()), "bad format: {}", id.as_ref());
    }

    #[test]
    fn no_collision_leaves_id_clean_and_seq_untouched() {
        let seq = AtomicU64::new(1);
        let id = JobId::generate("echo hi", &seq, |_| false);
        let s = id.as_ref();
        assert!(valid_format(s));
        let (_, right) = s.split_once(':').expect("id carries a time");
        assert!(!right.contains('-'), "unexpected collision suffix: {s}");
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
        let id = JobId::generate("echo hi", &seq, |_| true);
        let s = id.as_ref();
        assert!(valid_format(s));
        assert!(s.ends_with("-1"), "expected `-1` seq suffix, got {s}");
        assert_eq!(seq.load(Ordering::Relaxed), 2, "clash must advance seq");
    }

    #[test]
    fn display_and_as_ref_agree() {
        let seq = AtomicU64::new(1);
        let id = JobId::generate("ls", &seq, |_| false);
        assert_eq!(id.to_string(), id.as_ref());
    }

    #[test]
    fn serializes_as_a_plain_string() {
        let seq = AtomicU64::new(1);
        let id = JobId::generate("ls", &seq, |_| false);
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, format!("\"{}\"", id.as_ref()));
    }
}
