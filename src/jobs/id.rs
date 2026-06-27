//! Human-readable job ids: a neutral `job` prefix plus the local `HH:MM` it
//! started, e.g. `job-23:30` — easier to refer to in conversation than an opaque
//! counter. The prefix is hard-coded, never derived from the command: a command
//! can carry a secret in its leading tokens, and the id surfaces in replies,
//! `job(list)`, reaper logs, and the log filename, so the constructor refuses to
//! accept command text at all. A monotonic sequence suffix disambiguates jobs
//! starting within the same minute.

use std::borrow::Borrow;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Local;

/// A human-readable job identifier: `job-<HH:MM>` with an optional `-<seq>`
/// collision suffix. A generated id matches `^job-\d{2}:\d{2}(-\d+)?$`;
/// ids wrapped from client input for lookup are not revalidated.
#[derive(Debug, Clone, Hash, Eq, PartialEq, PartialOrd, Ord, serde::Serialize)]
pub struct JobId(String);

impl JobId {
    /// Build an id from the hard-coded `job` prefix and the current local time.
    /// The prefix is fixed here, never caller-supplied, so command text — which
    /// can carry a secret in its leading tokens — can't leak into the id. `exists`
    /// reports whether a candidate is already taken; only on a clash do we append
    /// a `-<seq>` drawn from `seq`, so clean ids stay clean. The predicate keeps
    /// this decoupled from however the caller tracks live ids.
    pub fn generate(seq: &AtomicU64, exists: impl Fn(&str) -> bool) -> Self {
        let base = format!("job-{}", Local::now().format("%H:%M"));
        if !exists(&base) {
            return Self(base);
        }
        // Same minute: a monotonic suffix keeps the id unique without sacrificing
        // readability.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirror the documented `^job-\d{2}:\d{2}(-\d+)?$` shape without pulling in a
    /// regex dependency. The id has exactly one `:` (prefix and suffix never
    /// contain one), so split on it to isolate the time.
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
        two_digits(hh) && two_digits(mm) && suffix_ok && prefix == "job"
    }

    #[test]
    fn generated_id_matches_documented_format() {
        let seq = AtomicU64::new(1);
        let id = JobId::generate(&seq, |_| false);
        assert!(valid_format(id.as_ref()), "bad format: {}", id.as_ref());
    }

    #[test]
    fn no_collision_leaves_id_clean_and_seq_untouched() {
        let seq = AtomicU64::new(1);
        let id = JobId::generate(&seq, |_| false);
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
        let id = JobId::generate(&seq, |_| true);
        let s = id.as_ref();
        assert!(valid_format(s));
        assert!(s.ends_with("-1"), "expected `-1` seq suffix, got {s}");
        assert_eq!(seq.load(Ordering::Relaxed), 2, "clash must advance seq");
    }

    #[test]
    fn display_and_as_ref_agree() {
        let seq = AtomicU64::new(1);
        let id = JobId::generate(&seq, |_| false);
        assert_eq!(id.to_string(), id.as_ref());
    }

    #[test]
    fn serializes_as_a_plain_string() {
        let seq = AtomicU64::new(1);
        let id = JobId::generate(&seq, |_| false);
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, format!("\"{}\"", id.as_ref()));
    }
}
