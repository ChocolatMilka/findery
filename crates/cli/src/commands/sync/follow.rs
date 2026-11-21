//! Running a sync while showing it.
//!
//! The server has no stream to offer, so watching is a poll loop against
//! `GET /v1/jobs/{id}` — and it lives here, at the edge. A streaming route would
//! be a protocol in service of a progress bar.

use findery_client::Client;
use findery_core::Result;
use findery_core::api::{JobState, Progress, SyncRequest, SyncResponse};
use findery_core::ids::ProjectId;
use indicatif::{ProgressBar, ProgressStyle};
use std::io::IsTerminal;
use std::time::Duration;

/// Each poll is an HTTP round trip and the bar moves in units of files, which
/// take far longer to change than this.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// A live bar on stderr, or nothing.
///
/// Never stdout: a pipe and `--json` must see the answer alone. And never when
/// nobody is watching — a redirected log gets the summary line, not hundreds of
/// carriage returns.
pub(crate) fn bar() -> Option<ProgressBar> {
    if !std::io::stderr().is_terminal() {
        return None;
    }
    let Ok(style) = ProgressStyle::with_template("{spinner} {bar:28} {pos}/{len} files · {msg}")
    else {
        // An unparseable template is a bug, and not a reason to lose the run:
        // the report prints either way.
        return None;
    };
    let bar = ProgressBar::new(0);
    bar.set_style(style.progress_chars("=> "));
    // The spinner is what says the run is alive while the walk is still
    // establishing the denominator.
    bar.enable_steady_tick(Duration::from_millis(120));
    Some(bar)
}

/// Draws one snapshot. The total appears only once the walk has returned, so the
/// bar grows to its real length once and never runs backwards.
fn draw(bar: &ProgressBar, p: &Progress) {
    if p.files_total > 0 {
        bar.set_length(p.files_total);
    }
    bar.set_position(p.files_done);
    bar.set_message(format!(
        "{} · {} chunks · {} embedded / {} cached",
        p.phase.as_str(),
        p.chunks_done,
        p.embeddings_computed,
        p.embeddings_cached
    ));
}

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
    bar: Option<ProgressBar>,
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

    loop {
        let current = client.job(&job.id).await?;
        // A bar is a luxury; waiting is not. Without one — a pipe, `--json`, a
        // log — the run is followed just the same, silently.
        if let (Some(bar), Some(progress)) = (&bar, &current.progress) {
            draw(bar, progress);
        }
        if current.state.is_terminal() {
            if let Some(bar) = &bar {
                bar.finish_and_clear();
            }
            return terminal(current);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// `findery init --sync` is where a large repository is met for the first time,
/// so it watches the run the same way `findery sync` does.
pub(crate) async fn sync_now(
    client: &Client,
    project: &ProjectId,
    json: bool,
) -> Result<findery_core::model::SyncReport> {
    let bar = if json { None } else { bar() };
    let request = SyncRequest::default();
    match watch_sync(client, project, &request, bar, true).await? {
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
fn terminal(job: findery_core::api::Job) -> Result<SyncResponse> {
    if job.state == JobState::Succeeded {
        return match job.report {
            Some(report) => Ok(SyncResponse::Completed { report }),
            None => Err(findery_core::Error::Internal(format!(
                "job `{}` succeeded without a report",
                job.id
            ))),
        };
    }
    Err(findery_core::Error::Internal(job.error.unwrap_or_else(
        || format!("job `{}` failed without a message", job.id),
    )))
}
