//! `JobStatus`: the bare status word for a job row. Distinct from `JobState`
//! (which additionally carries the exit code / error payload) — `JobStatus` is
//! what the `jobs.status` column holds and what callers compare/match against.
//! `as_str`/`FromStr` are the single place the on-disk strings are spelled; see
//! the schema comment in `src/db.rs` for the column itself.
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Running,
    Exited,
    Failed,
}

impl JobStatus {
    /// The exact string stored in the `jobs.status` column for this variant.
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Running => "running",
            JobStatus::Exited => "exited",
            JobStatus::Failed => "failed",
        }
    }
}

impl fmt::Display for JobStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A `jobs.status` value that matches no known variant — a corrupt row
/// (hand-edited DB, or a future writer bug), never silently coerced.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("unknown job status: {0:?}")]
pub struct UnknownJobStatus(String);

impl FromStr for JobStatus {
    type Err = UnknownJobStatus;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "running" => Ok(JobStatus::Running),
            "exited" => Ok(JobStatus::Exited),
            "failed" => Ok(JobStatus::Failed),
            other => Err(UnknownJobStatus(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str_maps_the_exact_on_disk_strings() {
        assert_eq!("running".parse::<JobStatus>().unwrap(), JobStatus::Running);
        assert_eq!("exited".parse::<JobStatus>().unwrap(), JobStatus::Exited);
        assert_eq!("failed".parse::<JobStatus>().unwrap(), JobStatus::Failed);
    }

    #[test]
    fn as_str_round_trips_through_from_str_for_every_variant() {
        for status in [JobStatus::Running, JobStatus::Exited, JobStatus::Failed] {
            assert_eq!(status.as_str().parse::<JobStatus>().unwrap(), status);
        }
    }

    #[test]
    fn from_str_rejects_unknown_status_explicitly_instead_of_coercing() {
        let err = "corrupted".parse::<JobStatus>().unwrap_err();
        assert_eq!(err, UnknownJobStatus("corrupted".to_string()));
    }
}
