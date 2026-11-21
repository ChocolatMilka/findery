//! How the engine turns a sync request into a job, runs it, and answers it.
//!
//! INTENT: split from the registry (CONV-029). The registry records what jobs
//! exist and how each ended; this is the one place that spawns their tasks,
//! and so the one place that can guarantee every task records its end.

use std::sync::Arc;
use std::time::Duration;

use findery_core::api::{SyncRequest, SyncResponse};
use findery_core::ids::ProjectId;
use findery_core::model::{SyncMode, SyncReport};
use findery_core::{Error, Result};

use super::{Admission, Job, JobState};
use crate::Indexer;
use crate::jobs::progress::ProgressHandle;

/// Records the end of a job whichever way its task ends: an outcome, an
/// abort, or a panic.
///
/// INTENT: an aborted task never runs the code after its last `await`. A job
/// recorded only there stayed `running` forever — answering every later sync
/// of its project, holding its watcher back for good. A guard's `drop` runs
/// on every exit; only the first end of a job counts, so the outcome a task
/// did reach is never overwritten.
struct JobEnd {
    indexer: Arc<Indexer>,
    job: String,
    project: ProjectId,
}

impl JobEnd {
    /// Waits for a slot, then runs the pass.
    async fn run(&self, pass: SyncMode, progress: &ProgressHandle) -> Result<SyncReport> {
        let _slot = self.indexer.jobs.acquire().await?;
        self.indexer.jobs.start(&self.job);
        self.indexer.sync(&self.project, pass, progress).await
    }

    fn record(&self, outcome: Result<SyncReport>) {
        let jobs = &self.indexer.jobs;
        match outcome {
            Ok(report) => jobs.succeed(&self.job, report),
            Err(e) => jobs.fail(&self.job, &e),
        }
    }
}

impl Drop for JobEnd {
    fn drop(&mut self) {
        let cut_short = if std::thread::panicking() {
            Error::Internal(format!("the sync of `{}` panicked", self.project))
        } else {
            Error::cancelled(&self.project, "an abort of its task")
        };
        self.indexer.jobs.fail(&self.job, &cut_short);
        self.indexer.jobs.untrack(&self.job);
    }
}

impl Indexer {
    /// Starts a sync as a job: the one already running the same pass on the
    /// project when there is one — a client that retries does not stack a
    /// second pass — otherwise a new one, queued behind any other pass.
    ///
    /// With `wait`, answers once the job ended; with `wait_ms`, once it ended
    /// or that long passed.
    pub async fn start_sync(
        self: &Arc<Self>,
        id: &ProjectId,
        req: &SyncRequest,
    ) -> Result<SyncResponse> {
        let pass = req.resolve()?;
        // Fail here rather than inside a background task nobody is watching.
        self.get_project(id).await?;
        let job = self.admit_sync(id, pass);
        if req.wait {
            let ended = self.jobs.wait_end(&job.id).await.unwrap_or(job);
            return answered(ended);
        }
        match req.wait_ms {
            Some(ms) => self.answer_within(job, ms).await,
            None => Ok(SyncResponse::Accepted { job: Box::new(job) }),
        }
    }

    /// The job a sync request runs as — joined, or created and spawned.
    pub(crate) fn admit_sync(self: &Arc<Self>, id: &ProjectId, pass: SyncMode) -> Job {
        match self.jobs.active_or_create(id, &pass) {
            Admission::Joined(job) => job,
            Admission::Created(job) => {
                self.spawn_sync(&job, id, pass);
                job
            }
        }
    }

    /// The job's report when it ends within `ms` (capped by
    /// `server.max_sync_wait_ms`), its error when it fails, otherwise the job
    /// as it stands — its live progress included.
    ///
    /// INTENT: an agent wants the answer when the sync is quick and a number
    /// when it is not, in one call; holding the request open to the end is what
    /// a client timeout kills.
    async fn answer_within(&self, job: Job, ms: u64) -> Result<SyncResponse> {
        let within = Duration::from_millis(ms.min(self.cfg.server.max_sync_wait_ms));
        let seen = self.jobs.wait(&job.id, within).await.unwrap_or(job);
        answered(seen)
    }

    /// Spawns the task of a created job and tracks it, so a drain can find it.
    fn spawn_sync(self: &Arc<Self>, job: &Job, id: &ProjectId, pass: SyncMode) {
        // The counters the task writes into are the ones the registry hands
        // back on `GET /v1/jobs/{id}` — that is the whole channel.
        let progress = self.jobs.handle(&job.id).unwrap_or_default();
        let end = JobEnd {
            indexer: self.clone(),
            job: job.id.clone(),
            project: id.clone(),
        };
        let task = tokio::spawn(async move {
            let outcome = end.run(pass, &progress).await;
            end.record(outcome);
        });
        self.jobs.track(&job.id, id, task.abort_handle());
    }
}

/// A job as a sync answer: its report once it succeeded, its error once it
/// failed, itself while it runs.
fn answered(job: Job) -> Result<SyncResponse> {
    if let Some(failure) = job.failure() {
        return Err(failure);
    }
    match (job.state, &job.report) {
        (JobState::Succeeded, Some(report)) => Ok(SyncResponse::Completed {
            report: report.clone(),
        }),
        _ => Ok(SyncResponse::Accepted { job: Box::new(job) }),
    }
}
