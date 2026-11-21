//! What a drain may abort, what a job records however its task ends, and how
//! sync requests become jobs: joined when equivalent, queued otherwise.

use findery_core::api::{JobState, SyncRequest, SyncResponse};

use super::*;
use crate::Access;

/// Starts a sync job and waits until its first batch is embedding.
async fn embedding_job(idx: &Arc<Indexer>, id: &ProjectId) -> String {
    let SyncResponse::Accepted { job } = idx
        .start_sync(id, &SyncRequest::default())
        .await
        .expect("started")
    else {
        panic!("a sync without `wait` is a job");
    };
    let handle = idx.jobs.handle(&job.id).expect("counters");
    let started = std::time::Instant::now();
    while handle.snapshot().chunks_pending_embed == 0 {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the job never embedded"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    job.id
}

/// The job once it ended, waited for at most `within`.
async fn ended(idx: &Indexer, job: &str, within: Duration) -> findery_core::api::Job {
    let seen = idx.jobs.wait(job, within).await.expect("held");
    assert!(
        seen.state.is_terminal(),
        "still {:?} after {within:?}",
        seen.state
    );
    seen
}

/// The bug this locks: an aborted task never recorded its end, so its job
/// stayed `running` forever — answering every later sync, holding every
/// watcher back. A job stuck in a flush is the only holder of its project:
/// the drain aborts it, and the job ends `cancelled`.
#[tokio::test]
async fn an_aborted_job_ends_failed_and_frees_its_project() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let cfg = config(data.path(), "[server]\ndrain_timeout_secs = 1\n");
    let idx = boot_with(&cfg, slow_builder(Duration::from_secs(60))).await;
    let id = register(&idx, proj.path(), "demo").await;
    let job = embedding_job(&idx, &id).await;

    idx.detach(&id).await.expect("the stuck job is aborted");

    let seen = ended(&idx, &job, Duration::from_secs(3)).await;
    assert_eq!(seen.state, JobState::Failed);
    assert_eq!(seen.error_code.as_deref(), Some("cancelled"));
    assert!(idx.jobs.has_active("demo").is_none(), "no zombie job");
    assert!(is_released(&local_member(proj.path(), "mock:128")));
}

/// Something else holds the project too — here a lease taken by hand. The
/// job is not the drain's to abort: aborting it would free nothing. The drain
/// gives up, the job finishes its work, and nothing is left running.
#[tokio::test]
async fn a_drain_held_by_another_lease_never_aborts_the_job() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let cfg = config(data.path(), "[server]\ndrain_timeout_secs = 1\n");
    let idx = boot_with(&cfg, slow_builder(Duration::from_secs(2))).await;
    let id = register(&idx, proj.path(), "demo").await;
    let job = embedding_job(&idx, &id).await;
    let held = idx.lease(&id, Access::Read).await.expect("lease");

    let err = error_of(idx.detach(&id).await);
    assert_eq!(err.code(), "busy");
    assert!(err.to_string().contains(&job), "names the job: {err}");
    drop(held);

    let seen = ended(&idx, &job, Duration::from_secs(5)).await;
    assert_eq!(seen.state, JobState::Succeeded, "{:?}", seen.error);
    assert!(idx.jobs.has_active("demo").is_none());
    assert!(idx.get_project(&id).await.is_ok(), "still attached");
}

/// A client that retries joins the job already running the same pass; a
/// caller that waits gets its report, not a second pass.
#[tokio::test]
async fn a_waiting_request_joins_the_equivalent_job_and_answers_its_report() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let idx = boot_with(
        &config(data.path(), ""),
        slow_builder(Duration::from_millis(200)),
    )
    .await;
    let id = register(&idx, proj.path(), "demo").await;
    let first = embedding_job(&idx, &id).await;

    let waited = SyncRequest {
        wait: true,
        ..SyncRequest::default()
    };
    let SyncResponse::Completed { report } = idx.start_sync(&id, &waited).await.expect("waited")
    else {
        panic!("a waiting request answers the report");
    };
    assert_eq!(report.files_added, 3, "{report:?}");
    assert_eq!(idx.jobs.list().len(), 1, "joined, not stacked");
    assert_eq!(idx.jobs.list()[0].id, first);
}

/// A targeted sync asked while a full pass runs is another pass: it is queued
/// behind it, never silently folded into it.
#[tokio::test]
async fn a_different_pass_is_queued_behind_the_running_one() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let idx = boot_with(
        &config(data.path(), ""),
        slow_builder(Duration::from_millis(200)),
    )
    .await;
    let id = register(&idx, proj.path(), "demo").await;
    let first = embedding_job(&idx, &id).await;

    let targeted = SyncRequest {
        paths: Some(vec!["src/net.rs".into()]),
        ..SyncRequest::default()
    };
    let SyncResponse::Accepted { job: second } =
        idx.start_sync(&id, &targeted).await.expect("queued")
    else {
        panic!("a queued pass is a job");
    };
    assert_ne!(second.id, first);
    assert_eq!(second.kind, "paths");

    let first = ended(&idx, &first, Duration::from_secs(5)).await;
    let second = ended(&idx, &second.id, Duration::from_secs(5)).await;
    assert_eq!(
        (first.state, second.state),
        (JobState::Succeeded, JobState::Succeeded)
    );
    let report = second.report.expect("report");
    assert_eq!(
        report.files_unchanged, 1,
        "the first pass wrote it: {report:?}"
    );
}

/// The bug this locks: a registration that moved a project while its sync
/// wrote left chunks and vectors behind in the bucket it left. The move waits
/// for the sync, then takes everything with it.
#[tokio::test]
async fn a_registration_into_another_bucket_waits_for_the_running_sync() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let idx = boot_with(
        &config(data.path(), ""),
        slow_builder(Duration::from_millis(300)),
    )
    .await;
    let id = register(&idx, proj.path(), "demo").await;
    let job = embedding_job(&idx, &id).await;

    let work = RegisterProject {
        bucket: Some(findery_core::manifest::BucketRef {
            name: Some("work".into()),
            path: None,
        }),
        ..RegisterProject::default()
    };
    let moved = registered(&idx, proj.path(), "demo", work).await;

    assert_eq!(
        ended(&idx, &job, Duration::from_secs(1)).await.state,
        JobState::Succeeded
    );
    assert_eq!(idx.home_of(&moved).expect("home"), "work");
    let left = idx.family("demo").expect("still open");
    for member in left.members() {
        let (projects, chunks) = member.bucket.totals().await.expect("totals");
        assert_eq!(
            (projects, chunks),
            (0, 0),
            "{}",
            member.bucket.path().display()
        );
    }
}
