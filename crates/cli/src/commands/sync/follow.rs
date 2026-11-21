//! Running a sync while showing it.
//!
//! The server has no stream to offer, so watching is a poll loop against
//! `GET /v1/jobs/{id}` — and it lives here, at the edge. A streaming route would
//! be a protocol in service of a progress bar.

use std::time::Duration;

use findery_core::Result;
use findery_core::api::{Job, JobState, SyncRequest, SyncResponse};
use findery_core::ids::ProjectId;

use findery_client::Client;

use crate::ui::board::{Board, ago};

/// Each poll is an HTTP round trip, and a bar redrawn five times a second
/// already reads as continuous.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Runs a sync and, unless the caller asked not to wait, follows it to the end.
///
/// INTENT: always a JOB, never one request held open for the length of the run.
/// Asking the server to block works right up to the moment the sync outlives the
/// client's request timeout, which then aborts the indexing mid-write and
/// reports the daemon as unreachable while it is working perfectly well. A job
/// belongs to the daemon, so a client that goes away does not interrupt it, and
/// every request stays short whatever the corpus weighs.
///
/// `wait = false` is the `--background` promise: hand back the job, now.
pub(crate) async fn watch_sync(
    client: &Client,
    project: &ProjectId,
    request: &SyncRequest,
    board: Option<Board>,
    wait: bool,
) -> Result<SyncResponse> {
    let job = match client.sync(project, request).await? {
        // A sync that finished inside the round trip still answers inline.
        SyncResponse::Completed { report } => return Ok(SyncResponse::Completed { report }),
        SyncResponse::Accepted { job } => job,
    };
    if !wait {
        return Ok(SyncResponse::Accepted { job });
    }
    // A board is a luxury; waiting is not. Without one — a pipe, `--json`, a
    // log — the run is followed just the same, silently.
    if let (Some(board), Some(note)) = (&board, attached(&job)) {
        board.note(&note);
    }

    loop {
        let current = client.job(&job.id).await?;
        if let Some(board) = &board {
            draw(board, &current);
        }
        if current.state.is_terminal() {
            if let Some(board) = &board {
                board.clear();
            }
            return terminal(current);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// A job already running before this command asked is someone else's pass,
/// joined midway: say so, or its first frame reads as a jump. A job this
/// command started is still queued when the answer comes back.
fn attached(job: &Job) -> Option<String> {
    match (job.state, &job.progress) {
        (JobState::Running, Some(progress)) => Some(format!(
            "attached to the running sync (started {} ago)",
            ago(progress.elapsed_ms)
        )),
        _ => None,
    }
}

fn draw(board: &Board, job: &Job) {
    match (job.state, &job.progress) {
        (JobState::Queued, _) => board.queued(),
        (_, Some(progress)) => board.draw(progress),
        _ => {}
    }
}

/// `findery init --sync` is where a large repository is met for the first time,
/// so it watches the run the same way `findery sync` does.
pub(crate) async fn sync_now(
    client: &Client,
    project: &ProjectId,
    json: bool,
) -> Result<findery_core::model::SyncReport> {
    let board = if json { None } else { Board::new() };
    let request = SyncRequest::default();
    match watch_sync(client, project, &request, board, true).await? {
        SyncResponse::Completed { report } => Ok(report),
        // Unreachable while `wait` mirrors the bar, and still handled: an
        // `unreachable!` on a production path is a panic waiting for a refactor.
        SyncResponse::Accepted { job } => Err(findery_core::Error::Internal(format!(
            "the sync was accepted as job `{}` instead of running to completion",
            job.id
        ))),
    }
}

/// A sync that failed must fail *here*, with a non-zero exit code: returning the
/// job would let the CLI print "queued" for work that is already over.
///
/// The failure is the variant the daemon raised, rebuilt by [`Job::failure`], so
/// the user reads `findery: recipe mismatch: …` and the command that repairs it —
/// not `findery: internal: config: …`, which blames a bug that is not there.
///
/// [`Job::failure`]: findery_core::api::Job::failure
fn terminal(job: Job) -> Result<SyncResponse> {
    if let Some(failure) = job.failure() {
        return Err(failure);
    }
    match (job.state, job.report) {
        (JobState::Succeeded, Some(report)) => Ok(SyncResponse::Completed { report }),
        (state, _) => Err(findery_core::Error::Internal(format!(
            "job `{}` ended as {state:?} without a report",
            job.id
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(state: JobState, error: Option<&str>, code: Option<&str>) -> Job {
        Job {
            id: "3f2a".into(),
            project: "axle".into(),
            kind: "full".into(),
            state,
            created_at: 0,
            finished_at: Some(1),
            progress: None,
            report: None,
            error: error.map(str::to_string),
            error_code: code.map(str::to_string),
        }
    }

    #[test]
    fn a_failed_sync_is_reported_as_the_error_the_daemon_raised() {
        let failed = job(
            JobState::Failed,
            Some("bucket `work` holds vectors built with `a:1`"),
            Some("recipe_mismatch"),
        );
        let err = terminal(failed).expect_err("a failed job fails the command");
        assert_eq!(
            err.to_string(),
            "recipe mismatch: bucket `work` holds vectors built with `a:1`"
        );
    }

    #[test]
    fn joining_a_running_sync_says_since_when_and_a_fresh_one_says_nothing() {
        let mut running = job(JobState::Running, None, None);
        running.progress = Some(findery_core::api::Progress {
            elapsed_ms: 12_300,
            ..Default::default()
        });
        assert_eq!(
            attached(&running).as_deref(),
            Some("attached to the running sync (started 12s ago)")
        );
        assert_eq!(attached(&job(JobState::Queued, None, None)), None);
    }

    #[test]
    fn a_success_without_a_report_is_still_an_error() {
        let err = terminal(job(JobState::Succeeded, None, None)).expect_err("no report");
        assert_eq!(err.code(), "internal");
    }
}
