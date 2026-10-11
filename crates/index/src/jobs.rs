//! Job registry for long-running syncs.
//!
//! INTENT: a first full index of a large repository takes minutes. An MCP
//! client that blocks on that HTTP call times out and retries, which starts a
//! second sync. So syncs are jobs: the request returns an id immediately, and
//! concurrency is capped by a semaphore so two clients cannot saturate the
//! embedding runtime between them.

use crate::progress::ProgressHandle;
use findery_core::clock::now_unix;
use findery_core::model::SyncReport;
use findery_core::{Error, Result};
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

// The job types are wire vocabulary and therefore live in L0, next to every
// other shape a client can observe.
pub use findery_core::api::{Job, JobState};

/// Bounded, in-memory job history.
pub struct JobRegistry {
    jobs: Mutex<VecDeque<Job>>,
    /// Live counters per non-terminal job. Apart from the deque, which is
    /// history, so a finished job can carry none.
    progress: Mutex<HashMap<String, ProgressHandle>>,
    capacity: usize,
    permits: Arc<Semaphore>,
}

impl JobRegistry {
    pub fn new(capacity: usize, concurrency: usize) -> Self {
        Self {
            jobs: Mutex::new(VecDeque::new()),
            progress: Mutex::new(HashMap::new()),
            capacity: capacity.max(1),
            permits: Arc::new(Semaphore::new(concurrency.max(1))),
        }
    }

    /// Waits for a slot. Held for the whole job, released on drop.
    pub async fn acquire(&self) -> Result<OwnedSemaphorePermit> {
        self.permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Internal("job semaphore closed".into()))
    }

    /// Registers a queued job and returns it.
    pub fn create(&self, project: &str, kind: &str) -> Job {
        let job = Job {
            id: uuid::Uuid::new_v4().to_string(),
            project: project.to_string(),
            kind: kind.to_string(),
            state: JobState::Queued,
            created_at: now_unix(),
            finished_at: None,
            progress: None,
            report: None,
            error: None,
        };
        self.progress
            .lock()
            .insert(job.id.clone(), ProgressHandle::detached());
        let mut jobs = self.jobs.lock();
        jobs.push_front(job.clone());
        while jobs.len() > self.capacity {
            if let Some(evicted) = jobs.pop_back() {
                self.progress.lock().remove(&evicted.id);
            }
        }
        job
    }

    /// The counters of a running job, for whoever drives it. `None` once it is
    /// terminal: its numbers are its report.
    pub fn handle(&self, id: &str) -> Option<ProgressHandle> {
        self.progress.lock().get(id).cloned()
    }

    /// Moves a job to `Running`.
    pub fn start(&self, id: &str) {
        self.update(id, |job| job.state = JobState::Running);
    }

    /// Records a successful outcome, and drops the counters: what the job did is
    /// the report, and a second copy could only disagree.
    pub fn succeed(&self, id: &str, report: SyncReport) {
        self.update(id, |job| {
            job.state = JobState::Succeeded;
            job.finished_at = Some(now_unix());
            job.report = Some(report.clone());
        });
        self.progress.lock().remove(id);
    }

    /// Records a failure, keeping the message clients will read.
    pub fn fail(&self, id: &str, error: &str) {
        self.update(id, |job| {
            job.state = JobState::Failed;
            job.finished_at = Some(now_unix());
            job.error = Some(error.to_string());
        });
        self.progress.lock().remove(id);
    }

    pub fn get(&self, id: &str) -> Option<Job> {
        let mut job = self.jobs.lock().iter().find(|j| j.id == id).cloned()?;
        self.stamp(&mut job);
        Some(job)
    }

    /// Newest first.
    pub fn list(&self) -> Vec<Job> {
        let mut jobs: Vec<Job> = self.jobs.lock().iter().cloned().collect();
        for job in &mut jobs {
            self.stamp(job);
        }
        jobs
    }

    /// The non-terminal job already covering this project, so a caller can
    /// refuse to queue a redundant sync.
    pub fn has_active(&self, project: &str) -> Option<Job> {
        let mut job = self
            .jobs
            .lock()
            .iter()
            .find(|j| j.project == project && !j.state.is_terminal())
            .cloned()?;
        self.stamp(&mut job);
        Some(job)
    }

    fn update(&self, id: &str, f: impl FnOnce(&mut Job)) {
        if let Some(job) = self.jobs.lock().iter_mut().find(|j| j.id == id) {
            f(job);
        }
    }

    /// Fills in the counters a job has *right now*. The record and the counters
    /// are kept apart, so a job is never read back with a stale snapshot. Only a
    /// running job has any: a queued one has not started, a finished one is
    /// described by its report.
    fn stamp(&self, job: &mut Job) {
        job.progress = match job.state {
            JobState::Running => self.handle(&job.id).map(|h| h.snapshot()),
            _ => None,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_move_through_their_lifecycle() {
        let reg = JobRegistry::new(8, 1);
        let job = reg.create("demo", "sync");
        assert_eq!(job.state, JobState::Queued);
        assert!(reg.has_active("demo").is_some());

        reg.start(&job.id);
        assert_eq!(reg.get(&job.id).expect("job").state, JobState::Running);

        reg.succeed(
            &job.id,
            SyncReport {
                files_added: 3,
                ..SyncReport::default()
            },
        );
        let done = reg.get(&job.id).expect("job");
        assert_eq!(done.state, JobState::Succeeded);
        assert_eq!(done.report.expect("report").files_added, 3);
        assert!(done.finished_at.is_some());
        assert!(reg.has_active("demo").is_none());
    }

    /// A running job reports where it is, from the handle the sync writes into.
    #[test]
    fn a_running_job_carries_the_counters_it_was_written() {
        let reg = JobRegistry::new(8, 1);
        let job = reg.create("demo", "sync");
        assert!(
            reg.get(&job.id).expect("job").progress.is_none(),
            "a queued job has not started, so it has nothing to report"
        );

        reg.start(&job.id);
        let handle = reg.handle(&job.id).expect("handle");
        handle.write(|p| {
            p.files_total = 200;
            p.files_done = 50;
        });
        let running = reg.get(&job.id).expect("job");
        let progress = running.progress.expect("progress");
        assert_eq!(progress.files_total, 200);
        assert_eq!(progress.files_done, 50);
    }

    /// Once a job is over, its report is the only description of it. A second
    /// record of the same fact could only disagree.
    #[test]
    fn a_finished_job_carries_no_progress_at_all() {
        let reg = JobRegistry::new(8, 1);
        let job = reg.create("demo", "sync");
        reg.start(&job.id);
        reg.handle(&job.id)
            .expect("handle")
            .write(|p| p.files_done = 3);

        reg.succeed(&job.id, SyncReport::default());
        assert!(reg.get(&job.id).expect("job").progress.is_none());
        assert!(
            reg.handle(&job.id).is_none(),
            "the counters are dropped too"
        );
    }

    /// A failure leaves no counters either: "50/200" on a stopped job reads as
    /// progress that is still coming.
    #[test]
    fn a_failed_job_carries_no_progress_either() {
        let reg = JobRegistry::new(8, 1);
        let job = reg.create("demo", "sync");
        reg.start(&job.id);
        reg.handle(&job.id)
            .expect("handle")
            .write(|p| p.files_done = 3);

        reg.fail(&job.id, "embedder unreachable");
        assert!(reg.get(&job.id).expect("job").progress.is_none());
    }

    #[test]
    fn failures_keep_their_message() {
        let reg = JobRegistry::new(8, 1);
        let job = reg.create("demo", "sync");
        reg.fail(&job.id, "embedder unreachable");
        let done = reg.get(&job.id).expect("job");
        assert_eq!(done.state, JobState::Failed);
        assert_eq!(done.error.as_deref(), Some("embedder unreachable"));
    }

    #[test]
    fn history_is_bounded_and_newest_first() {
        let reg = JobRegistry::new(2, 1);
        reg.create("a", "sync");
        reg.create("b", "sync");
        let third = reg.create("c", "sync");
        let list = reg.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, third.id);
        assert!(list.iter().all(|j| j.project != "a"));
    }

    #[tokio::test]
    async fn concurrency_is_capped_by_the_semaphore() {
        let reg = JobRegistry::new(8, 1);
        let held = reg.acquire().await.expect("permit");
        let blocked =
            tokio::time::timeout(std::time::Duration::from_millis(50), reg.acquire()).await;
        assert!(blocked.is_err(), "a second job must wait");
        drop(held);
        let _next = reg.acquire().await.expect("permit after release");
    }
}
