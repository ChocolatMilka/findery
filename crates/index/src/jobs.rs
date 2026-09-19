//! Job registry for long-running syncs.
//!
//! INTENT: a first full index of a large repository takes minutes. An MCP
//! client that blocks on that HTTP call times out and retries, which starts a
//! second sync. So syncs are jobs: the request returns an id immediately, and
//! concurrency is capped by a semaphore so two clients cannot saturate the
//! embedding runtime between them.

use findery_core::clock::now_unix;
use findery_core::model::SyncReport;
use findery_core::{Error, Result};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

// The job types are wire vocabulary and therefore live in L0, next to every
// other shape a client can observe.
pub use findery_core::api::{Job, JobState};

/// Bounded, in-memory job history.
pub struct JobRegistry {
    jobs: Mutex<VecDeque<Job>>,
    capacity: usize,
    permits: Arc<Semaphore>,
}

impl JobRegistry {
    pub fn new(capacity: usize, concurrency: usize) -> Self {
        Self {
            jobs: Mutex::new(VecDeque::new()),
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
            report: None,
            error: None,
        };
        let mut jobs = self.jobs.lock();
        jobs.push_front(job.clone());
        while jobs.len() > self.capacity {
            jobs.pop_back();
        }
        job
    }

    /// Moves a job to `Running`.
    pub fn start(&self, id: &str) {
        self.update(id, |job| job.state = JobState::Running);
    }

    /// Records a successful outcome.
    pub fn succeed(&self, id: &str, report: SyncReport) {
        self.update(id, |job| {
            job.state = JobState::Succeeded;
            job.finished_at = Some(now_unix());
            job.report = Some(report.clone());
        });
    }

    /// Records a failure, keeping the message clients will read.
    pub fn fail(&self, id: &str, error: &str) {
        self.update(id, |job| {
            job.state = JobState::Failed;
            job.finished_at = Some(now_unix());
            job.error = Some(error.to_string());
        });
    }

    pub fn get(&self, id: &str) -> Option<Job> {
        self.jobs.lock().iter().find(|j| j.id == id).cloned()
    }

    /// Newest first.
    pub fn list(&self) -> Vec<Job> {
        self.jobs.lock().iter().cloned().collect()
    }

    /// True when a non-terminal job already covers this project, so a caller
    /// can refuse to queue a redundant sync.
    pub fn has_active(&self, project: &str) -> Option<Job> {
        self.jobs
            .lock()
            .iter()
            .find(|j| j.project == project && !j.state.is_terminal())
            .cloned()
    }

    fn update(&self, id: &str, f: impl FnOnce(&mut Job)) {
        if let Some(job) = self.jobs.lock().iter_mut().find(|j| j.id == id) {
            f(job);
        }
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
