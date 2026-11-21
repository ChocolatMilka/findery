//! Job registry for long-running syncs, and the engine's way of starting one.
//!
//! INTENT: a first full index of a large repository takes minutes. An MCP
//! client that blocks on that HTTP call times out and retries, which starts a
//! second sync. So syncs are jobs: the request returns an id immediately, a
//! retry of the same request joins the job already running, and concurrency is
//! capped by a semaphore so two clients cannot saturate the embedding runtime
//! between them.
//!
//! The jobs are spawned HERE, in L2, and not by the HTTP handler: a drain
//! (`detach`, `rm`) has to find the job running on a project and, failing a
//! clean stop, abort it — which only the layer that spawned the task can do.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::AbortHandle;
use tokio::time::Instant;

use findery_core::ids::ProjectId;
use findery_core::model::{SyncMode, SyncReport};
use findery_core::support::clock::now_unix;
use findery_core::{Error, Result};

use crate::jobs::progress::ProgressHandle;

pub mod progress;
mod run;
pub mod watch;

// The job types are wire vocabulary and therefore live in L0, next to every
// other shape a client can observe.
pub use findery_core::api::{Job, JobState};

/// Bounded, in-memory job history.
pub struct JobRegistry {
    history: Mutex<History>,
    /// Live counters per non-terminal job. Apart from the history, so a
    /// finished job can carry none.
    progress: Mutex<HashMap<String, ProgressHandle>>,
    /// The task of each job still running, and its project: what a drain
    /// aborts when the cancel request was not enough.
    tasks: Mutex<HashMap<String, (ProjectId, AbortHandle)>>,
    permits: Arc<Semaphore>,
    /// Woken each time a job ends, for whoever waits on one.
    finished: Notify,
}

/// The jobs, newest first, and the pass each one runs.
struct History {
    jobs: VecDeque<Job>,
    passes: HashMap<String, SyncMode>,
    capacity: usize,
}

impl History {
    fn push(&mut self, job: Job, pass: SyncMode) {
        self.passes.insert(job.id.clone(), pass);
        self.jobs.push_front(job);
        self.evict_finished();
    }

    /// Drops the oldest FINISHED jobs past the capacity. A queued or running
    /// job is never evicted: a retry must still join it, and a drain find it.
    fn evict_finished(&mut self) {
        while self.jobs.len() > self.capacity {
            let Some(oldest) = self.jobs.iter().rposition(|j| j.state.is_terminal()) else {
                return;
            };
            if let Some(job) = self.jobs.remove(oldest) {
                self.passes.remove(&job.id);
            }
        }
    }

    /// The non-terminal job of `project` running `pass`, when there is one.
    fn active(&self, project: &str, pass: &SyncMode) -> Option<&Job> {
        self.jobs.iter().find(|job| {
            job.project == project
                && !job.state.is_terminal()
                && self.passes.get(&job.id).is_some_and(|p| same_pass(p, pass))
        })
    }
}

/// Two requests for the same pass: one mode, and for targeted syncs, the same
/// set of paths in whatever order.
fn same_pass(a: &SyncMode, b: &SyncMode) -> bool {
    match (a, b) {
        (SyncMode::Paths(x), SyncMode::Paths(y)) => {
            x.iter().collect::<BTreeSet<_>>() == y.iter().collect::<BTreeSet<_>>()
        }
        _ => a == b,
    }
}

/// How a sync request became a job.
pub enum Admission {
    /// An equivalent job was already queued or running.
    Joined(Job),
    /// A new job, queued behind whatever already runs on the project.
    Created(Job),
}

impl JobRegistry {
    pub fn new(capacity: usize, concurrency: usize) -> Self {
        Self {
            history: Mutex::new(History {
                jobs: VecDeque::new(),
                passes: HashMap::new(),
                capacity: capacity.max(1),
            }),
            progress: Mutex::new(HashMap::new()),
            tasks: Mutex::new(HashMap::new()),
            permits: Arc::new(Semaphore::new(concurrency.max(1))),
            finished: Notify::new(),
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

    /// Registers a queued job running `pass` on `project`, and returns it.
    pub fn create(&self, project: &str, pass: &SyncMode) -> Job {
        let job = queued(project, pass);
        self.progress
            .lock()
            .insert(job.id.clone(), ProgressHandle::detached());
        self.history.lock().push(job.clone(), pass.clone());
        job
    }

    /// The job already queued or running `pass` on `project`, else a new one —
    /// decided under one lock, so two requests at once never both create.
    pub fn active_or_create(&self, project: &ProjectId, pass: &SyncMode) -> Admission {
        let mut history = self.history.lock();
        if let Some(active) = history.active(project.as_str(), pass) {
            let mut job = active.clone();
            drop(history);
            self.stamp(&mut job);
            return Admission::Joined(job);
        }
        let job = queued(project.as_str(), pass);
        self.progress
            .lock()
            .insert(job.id.clone(), ProgressHandle::detached());
        history.push(job.clone(), pass.clone());
        Admission::Created(job)
    }

    /// The counters of a running job, for whoever drives it. `None` once it is
    /// terminal: its numbers are its report.
    pub fn handle(&self, id: &str) -> Option<ProgressHandle> {
        self.progress.lock().get(id).cloned()
    }

    /// Moves a queued job to `Running`.
    pub fn start(&self, id: &str) {
        self.update(id, |job| job.state = JobState::Running);
    }

    /// Records a successful outcome, and drops the counters: what the job did is
    /// the report, and a second copy could only disagree.
    pub fn succeed(&self, id: &str, report: SyncReport) {
        self.end(id, |job| {
            job.state = JobState::Succeeded;
            job.report = Some(report);
        });
    }

    /// Records a failure as the wire carries an error: its code, and its
    /// payload without the prefix the variant renders.
    ///
    /// INTENT: a job is read back through `GET /v1/jobs/{id}`, long after the
    /// error value is gone. Storing `to_string()` alone flattened every failure
    /// into one string a client could only report as `internal: config: …`;
    /// the code is what lets [`Job::failure`] rebuild the variant.
    pub fn fail(&self, id: &str, error: &Error) {
        self.end(id, |job| {
            job.state = JobState::Failed;
            job.error = Some(error.detail());
            job.error_code = Some(error.code().to_string());
        });
    }

    /// The first end of a job is its end: a later one — the abort that raced a
    /// success — changes nothing.
    fn end(&self, id: &str, outcome: impl FnOnce(&mut Job)) {
        let ended = self.update(id, |job| {
            outcome(job);
            job.finished_at = Some(now_unix());
        });
        if ended {
            self.progress.lock().remove(id);
            self.history.lock().evict_finished();
            self.finished.notify_waiters();
        }
    }

    /// The job once it ended, or as it stands when `within` runs out; `None`
    /// for a job this registry does not hold.
    pub async fn wait(&self, id: &str, within: Duration) -> Option<Job> {
        self.wait_until(id, Some(Instant::now() + within)).await
    }

    /// The job once it ended, however long that takes.
    pub async fn wait_end(&self, id: &str) -> Option<Job> {
        self.wait_until(id, None).await
    }

    async fn wait_until(&self, id: &str, deadline: Option<Instant>) -> Option<Job> {
        loop {
            // Registered before the state is read, so an end that lands in
            // between still wakes this loop.
            let notified = self.finished.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let job = self.get(id)?;
            if job.state.is_terminal() {
                return Some(job);
            }
            match deadline {
                Some(at) => {
                    if tokio::time::timeout_at(at, notified).await.is_err() {
                        return self.get(id);
                    }
                }
                None => notified.await,
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<Job> {
        let mut job = self
            .history
            .lock()
            .jobs
            .iter()
            .find(|j| j.id == id)
            .cloned()?;
        self.stamp(&mut job);
        Some(job)
    }

    /// Newest first.
    pub fn list(&self) -> Vec<Job> {
        let mut jobs: Vec<Job> = self.history.lock().jobs.iter().cloned().collect();
        for job in &mut jobs {
            self.stamp(job);
        }
        jobs
    }

    /// The non-terminal job already covering this project, whatever its pass.
    pub fn has_active(&self, project: &str) -> Option<Job> {
        let mut job = self
            .history
            .lock()
            .jobs
            .iter()
            .find(|j| j.project == project && !j.state.is_terminal())
            .cloned()?;
        self.stamp(&mut job);
        Some(job)
    }

    /// Remembers the task running a job. A job already over by the time it is
    /// tracked is not, so a finished task is never what a drain aborts.
    pub(crate) fn track(&self, id: &str, project: &ProjectId, task: AbortHandle) {
        if self.get(id).is_some_and(|job| job.state.is_terminal()) {
            return;
        }
        self.tasks
            .lock()
            .insert(id.to_string(), (project.clone(), task));
    }

    /// Forgets a job's task once it ended.
    pub(crate) fn untrack(&self, id: &str) {
        self.tasks.lock().remove(id);
    }

    /// The job still running on `project`, and the handle that aborts it.
    pub(crate) fn running_for(&self, project: &ProjectId) -> Option<(String, AbortHandle)> {
        let candidates: Vec<(String, AbortHandle)> = self
            .tasks
            .lock()
            .iter()
            .filter(|(_, (p, _))| p == project)
            .map(|(id, (_, task))| (id.clone(), task.clone()))
            .collect();
        candidates.into_iter().find(|(id, _)| {
            self.get(id)
                .is_some_and(|job| job.state == JobState::Running)
        })
    }

    /// Applies `f` to a job that has not ended; true when it did.
    fn update(&self, id: &str, f: impl FnOnce(&mut Job)) -> bool {
        let mut history = self.history.lock();
        let found = history
            .jobs
            .iter_mut()
            .find(|j| j.id == id && !j.state.is_terminal());
        match found {
            Some(job) => {
                f(job);
                true
            }
            None => false,
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

/// A job that has not started yet.
fn queued(project: &str, pass: &SyncMode) -> Job {
    Job {
        id: uuid::Uuid::new_v4().to_string(),
        project: project.to_string(),
        kind: pass.as_str().to_string(),
        state: JobState::Queued,
        created_at: now_unix(),
        finished_at: None,
        progress: None,
        report: None,
        error: None,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASS: SyncMode = SyncMode::Incremental;

    #[test]
    fn jobs_move_through_their_lifecycle() {
        let reg = JobRegistry::new(8, 1);
        let job = reg.create("demo", &PASS);
        assert_eq!(job.state, JobState::Queued);
        assert_eq!(job.kind, "incremental");
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

    /// The bug this locks: an abort recorded after a success turned a job
    /// that had finished its work into a failed one.
    #[test]
    fn only_the_first_end_of_a_job_counts() {
        let reg = JobRegistry::new(8, 1);
        let job = reg.create("demo", &PASS);
        reg.succeed(&job.id, SyncReport::default());
        let demo = ProjectId::new("demo").expect("id");
        reg.fail(&job.id, &Error::cancelled(&demo, "a late abort"));
        let done = reg.get(&job.id).expect("job");
        assert_eq!(done.state, JobState::Succeeded);
        assert!(done.error.is_none());
    }

    /// A running job reports where it is, from the handle the sync writes into.
    #[test]
    fn a_running_job_carries_the_counters_it_was_written() {
        let reg = JobRegistry::new(8, 1);
        let job = reg.create("demo", &PASS);
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
        let job = reg.create("demo", &PASS);
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
        let job = reg.create("demo", &PASS);
        reg.start(&job.id);
        reg.handle(&job.id)
            .expect("handle")
            .write(|p| p.files_done = 3);

        reg.fail(&job.id, &Error::upstream("embedder", "unreachable"));
        assert!(reg.get(&job.id).expect("job").progress.is_none());
    }

    /// The message is stored without the variant's prefix, and the code beside
    /// it: together they rebuild the very error the sync returned.
    #[test]
    fn failures_keep_their_variant_and_their_message() {
        let reg = JobRegistry::new(8, 1);
        let job = reg.create("demo", &PASS);
        let raised = Error::recipe_mismatch("work", "a:128", "b:128", true);
        reg.fail(&job.id, &raised);
        let done = reg.get(&job.id).expect("job");
        assert_eq!(done.state, JobState::Failed);
        assert_eq!(done.error_code.as_deref(), Some("recipe_mismatch"));
        assert_eq!(done.error.as_deref(), Some(raised.detail().as_str()));
        assert!(
            !done
                .error
                .as_deref()
                .unwrap_or_default()
                .starts_with("recipe mismatch:"),
            "the prefix is the variant's to render, once"
        );
        let rebuilt = done.failure().expect("a failed job has a failure");
        assert_eq!(rebuilt.to_string(), raised.to_string());
    }

    /// A job that has not failed carries no code, so nothing reads one into it.
    #[test]
    fn a_job_that_succeeded_has_no_error_code() {
        let reg = JobRegistry::new(8, 1);
        let job = reg.create("demo", &PASS);
        reg.succeed(&job.id, SyncReport::default());
        let done = reg.get(&job.id).expect("job");
        assert!(done.error_code.is_none());
        assert!(done.failure().is_none());
    }

    #[test]
    fn history_is_bounded_and_newest_first() {
        let reg = JobRegistry::new(2, 1);
        for project in ["a", "b", "c"] {
            let job = reg.create(project, &PASS);
            reg.succeed(&job.id, SyncReport::default());
        }
        let list = reg.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].project, "c");
        assert!(list.iter().all(|j| j.project != "a"));
    }

    /// The bug this locks: a full history evicted a job still running, so a
    /// retry stacked a second pass and a drain could not find the first.
    #[test]
    fn a_job_that_has_not_ended_is_never_evicted() {
        let reg = JobRegistry::new(2, 1);
        let running = reg.create("a", &PASS);
        reg.start(&running.id);
        let queued = reg.create("b", &PASS);
        let third = reg.create("c", &PASS);
        assert_eq!(reg.list().len(), 3, "nothing finished, nothing evicted");

        reg.succeed(&third.id, SyncReport::default());
        let ids: Vec<String> = reg.list().into_iter().map(|j| j.id).collect();
        assert_eq!(ids, [queued.id, running.id], "the finished one went first");
    }

    /// A retry of the same pass joins the job; another pass is a job of its own.
    #[test]
    fn only_an_equivalent_request_joins_a_running_job() {
        let reg = JobRegistry::new(8, 1);
        let demo = ProjectId::new("demo").expect("id");
        let paths =
            |listed: &[&str]| SyncMode::Paths(listed.iter().map(|path| path.to_string()).collect());
        let Admission::Created(first) = reg.active_or_create(&demo, &paths(&["a.rs", "b.rs"]))
        else {
            panic!("nothing ran yet");
        };
        let Admission::Joined(joined) = reg.active_or_create(&demo, &paths(&["b.rs", "a.rs"]))
        else {
            panic!("the same paths are the same pass");
        };
        assert_eq!(joined.id, first.id);
        assert!(matches!(
            reg.active_or_create(&demo, &paths(&["c.rs"])),
            Admission::Created(_)
        ));
        assert!(matches!(
            reg.active_or_create(&demo, &SyncMode::Full),
            Admission::Created(_)
        ));
    }

    #[tokio::test]
    async fn a_wait_returns_the_job_as_soon_as_it_ends() {
        let reg = Arc::new(JobRegistry::new(8, 1));
        let job = reg.create("demo", &PASS);
        let ender = reg.clone();
        let id = job.id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            ender.succeed(&id, SyncReport::default());
        });
        let seen = reg
            .wait(&job.id, Duration::from_secs(30))
            .await
            .expect("held");
        assert_eq!(seen.state, JobState::Succeeded);
    }

    #[tokio::test]
    async fn a_wait_that_runs_out_answers_the_job_as_it_stands() {
        let reg = JobRegistry::new(8, 1);
        let job = reg.create("demo", &PASS);
        reg.start(&job.id);
        let seen = reg
            .wait(&job.id, Duration::from_millis(10))
            .await
            .expect("held");
        assert_eq!(seen.state, JobState::Running);
        assert!(reg.wait("nope", Duration::from_millis(1)).await.is_none());
    }

    #[tokio::test]
    async fn concurrency_is_capped_by_the_semaphore() {
        let reg = JobRegistry::new(8, 1);
        let held = reg.acquire().await.expect("permit");
        let blocked = tokio::time::timeout(Duration::from_millis(50), reg.acquire()).await;
        assert!(blocked.is_err(), "a second job must wait");
        drop(held);
        let _next = reg.acquire().await.expect("permit after release");
    }
}
