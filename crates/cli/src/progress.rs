//! Running a sync while showing it.
//!
//! INTENT: a first index of a large repository takes minutes, and the CLI is the
//! only place a human is actually watching one. The server has no stream to
//! offer, so watching is a loop of polls against `GET /v1/jobs/{id}` — and that
//! loop lives here, at the edge. A streaming route in the API would be a
//! protocol in service of a progress bar, paid for by every client that never
//! draws one.

use findery_client::Client;
use findery_core::Result;
use findery_core::api::{JobState, Progress, SyncRequest, SyncResponse};
use findery_core::ids::ProjectId;
use indicatif::{ProgressBar, ProgressStyle};
use std::io::IsTerminal;
use std::time::Duration;

/// How often the CLI asks where a job is.
///
/// INTENT: each poll is a full HTTP round trip, and the bar moves in units of
/// files, which take far longer than this to change. Going faster buys nothing
/// but load on a daemon that is busy embedding; going slower makes a live bar
/// feel stuck.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// A live bar, or nothing at all.
///
/// Two refusals, both deliberate:
///
/// * **stderr, never stdout** — the same channel as a warning. A pipe and
///   `--json` must see the answer and nothing else, and a bar on stdout would
///   end up inside whatever is reading it.
/// * **nothing when nobody is watching** — redirected output and CI logs get the
///   summary line they can grep, not a few hundred carriage returns.
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

/// Draws one snapshot of a job's counters.
///
/// The total only appears once the walk has returned, so the bar grows from
/// "0 of 0" to its real length exactly once, and never runs backwards.
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

/// Runs a sync, drawing it while it runs.
///
/// With no bar this is a plain call: the request already asked the server to
/// wait, so the answer comes back complete. With a bar the request asked for a
/// job, and this loop turns its counters into something a person can read.
pub(crate) async fn watch_sync(
    client: &Client,
    project: &ProjectId,
    request: &SyncRequest,
    bar: Option<ProgressBar>,
) -> Result<SyncResponse> {
    let job = match client.sync(project, request).await? {
        SyncResponse::Completed { report } => return Ok(SyncResponse::Completed { report }),
        SyncResponse::Accepted { job } => job,
    };
    let Some(bar) = bar else {
        return Ok(SyncResponse::Accepted { job });
    };

    loop {
        let current = client.job(&job.id).await?;
        if let Some(progress) = &current.progress {
            draw(&bar, progress);
        }
        if current.state.is_terminal() {
            bar.finish_and_clear();
            return terminal(current);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Runs a project's first index to completion, drawing it when there is a
/// terminal to draw on.
///
/// INTENT: `findery init --sync` is where a large repository is met for the
/// first time, which makes it the single most important place to show something
/// moving. It is the same watch loop as `findery sync`, so it is the same
/// function.
pub(crate) async fn sync_now(
    client: &Client,
    project: &ProjectId,
    json: bool,
) -> Result<findery_core::model::SyncReport> {
    let bar = if json { None } else { bar() };
    let request = SyncRequest {
        mode: None,
        paths: None,
        wait: bar.is_none(),
    };
    match watch_sync(client, project, &request, bar).await? {
        SyncResponse::Completed { report } => Ok(report),
        // Unreachable while `wait` mirrors the bar, and still handled: an
        // `unreachable!` on a production path is a panic waiting for a refactor.
        SyncResponse::Accepted { job } => Err(findery_core::Error::Internal(format!(
            "the sync was accepted as job `{}` instead of running to completion",
            job.id
        ))),
    }
}

/// Turns a finished job into this call's answer.
///
/// INTENT: a sync that failed must fail *here*, loudly, with a non-zero exit
/// code. Returning the job would let the CLI print "queued" for work that is
/// already over and will never happen.
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
