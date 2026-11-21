//! The vocabulary of background jobs: their lifecycle and how a failed one
//! hands its error back. Their live counters are `progress.rs`.
//!
//! Split from the rest of the wire vocabulary because a job is the one shape a
//! client polls rather than receives once — it has a lifecycle, a progress
//! model and an error round trip of its own, and those read better together.

use serde::{Deserialize, Serialize};

use super::Progress;
use crate::Error;
use crate::model::SyncReport;

/// Lifecycle of a background job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Succeeded,
    Failed,
}

impl JobState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }
}

/// One unit of background work, as clients observe it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub project: String,
    /// `incremental`, `full` or `paths`.
    pub kind: String,
    pub state: JobState,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    /// Counters of a job that is still running; absent before and after.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<Progress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<SyncReport>,
    /// What a failed job's error says, for a human: the payload of the error,
    /// without the prefix its variant renders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The [`Error::code`] of a failed job's error, so a client rebuilds the
    /// variant instead of reporting every failure as `internal`. Absent from an
    /// older daemon, which is why it defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

impl Job {
    /// The error a failed job ended with, rebuilt as the variant the daemon
    /// raised; `None` for a job that has not failed.
    ///
    /// INTENT: one decoder for every consumer — client, CLI and MCP bridge —
    /// through [`Error::from_wire`]. A job from a daemon that predates
    /// `error_code` still fails, as `Internal`, with its message intact.
    pub fn failure(&self) -> Option<Error> {
        if self.state != JobState::Failed {
            return None;
        }
        // Copied rather than moved: a job is read through `&` by whoever polls
        // it, and the error is the caller's to own.
        let message = self
            .error
            .clone()
            .unwrap_or_else(|| format!("job `{}` failed without a message", self.id));
        Some(match &self.error_code {
            Some(code) => Error::from_wire(code, message),
            None => Error::Internal(message),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A job read from an older daemon has no `progress` key at all, and that
    /// must deserialize rather than fail: a client newer than its server is the
    /// ordinary case during a rolling upgrade.
    #[test]
    fn a_job_without_progress_still_deserializes() {
        let job: Job = serde_json::from_str(
            r#"{"id":"1","project":"p","kind":"full","state":"running","created_at":0}"#,
        )
        .expect("parse");
        assert!(job.progress.is_none());
    }

    /// A failed job keeps the variant its error was raised as, end to end
    /// through JSON — not `internal` for everything.
    #[test]
    fn a_failed_job_rebuilds_the_error_it_ended_with() {
        let job: Job = serde_json::from_str(
            r#"{"id":"1","project":"p","kind":"full","state":"failed","created_at":0,
                "error":"bucket `w` holds vectors built with `a:1`","error_code":"recipe_mismatch"}"#,
        )
        .expect("parse");
        let failure = job.failure().expect("a failed job has a failure");
        assert!(matches!(failure, Error::RecipeMismatch(_)), "{failure:?}");
        assert_eq!(
            failure.to_string(),
            "recipe mismatch: bucket `w` holds vectors built with `a:1`"
        );
    }

    /// An older daemon sends no `error_code`: the failure is still a failure.
    #[test]
    fn a_failed_job_from_an_older_daemon_is_internal_with_its_message() {
        let job: Job = serde_json::from_str(
            r#"{"id":"1","project":"p","kind":"full","state":"failed","created_at":0,
                "error":"embedder unreachable"}"#,
        )
        .expect("parse");
        let failure = job.failure().expect("failure");
        assert_eq!(failure.code(), "internal");
        assert_eq!(failure.detail(), "embedder unreachable");
    }

    #[test]
    fn a_job_that_has_not_failed_has_no_failure() {
        let job: Job = serde_json::from_str(
            r#"{"id":"1","project":"p","kind":"full","state":"running","created_at":0,
                "error_code":"busy"}"#,
        )
        .expect("parse");
        assert!(job.failure().is_none());
    }
}
