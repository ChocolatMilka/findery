//! `detach`, `rm` and `bucket rm`: what they wait for, what they release,
//! what they delete, and what the next boot finishes for them.

use findery_core::api::{JobState, SyncRequest, SyncResponse};

use findery_store::files::registry::{DetachedEntry, Registry};

use super::*;
use crate::Access;

/// A project of many small files: enough flushes that a drain arrives while
/// a sync is still embedding.
fn a_large_project(files: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("project dir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    for i in 0..files {
        std::fs::write(
            dir.path().join(format!("src/f{i}.rs")),
            format!("pub fn item_{i}() -> u32 {{\n    {i}\n}}\n"),
        )
        .expect("write");
    }
    dir
}

async fn job_state(idx: &Indexer, job: &str) -> (JobState, Option<String>) {
    let job = idx.jobs.get(job).expect("job");
    (job.state, job.error_code)
}

/// The scenario `detach` exists for: a sync is running, the user wants the
/// file. The sync stops at its next check, the file is released, and the next
/// `register` attaches the index again with nothing lost.
#[tokio::test]
async fn detaching_during_a_sync_cancels_the_job_and_releases_the_file() {
    let (data, proj) = (tempfile::tempdir().expect("data"), a_large_project(250));
    let cfg = config(data.path(), "");
    let idx = boot_with(&cfg, slow_builder(Duration::from_millis(300))).await;
    let id = register(&idx, proj.path(), "big").await;

    let SyncResponse::Accepted { job } = idx
        .start_sync(&id, &SyncRequest::default())
        .await
        .expect("started")
    else {
        panic!("a sync without `wait` is a job");
    };
    // Into the first flush, which is where the time goes.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let detached = idx.detach(&id).await.expect("detached");

    assert_eq!(
        job_state(&idx, &job.id).await,
        (JobState::Failed, Some("cancelled".into()))
    );
    let member = local_member(proj.path(), "mock:128");
    assert_eq!(
        detached.closed,
        vec![crate::storage::families::shown(&member)],
        "{detached:?}"
    );
    assert!(detached.still_open.is_none());
    assert!(is_released(&member), "no process holds the file any more");
    assert_eq!(
        idx.get_project(&id).await.map_err(|e| e.code()).err(),
        Some("not_found")
    );
    let (registry, _) = Registry::load(&cfg.store);
    assert!(registry.detached_entry(&id).is_some_and(|e| !e.removing));

    // Attached again, instantly: what the cancelled pass wrote is still there.
    register(&idx, proj.path(), "big").await;
    assert!(idx.stats(&id).await.expect("stats").chunks > 0);
    let rest = sync(&idx, &id).await;
    assert!(
        rest.files_unchanged > 0,
        "the first flush was kept: {rest:?}"
    );
    assert_eq!(
        rest.embeddings_computed + rest.files_unchanged,
        250,
        "only what the cancelled pass never reached is embedded: {rest:?}"
    );
    let (registry, _) = Registry::load(&cfg.store);
    assert!(registry.detached_entry(&id).is_none(), "attached again");
}

/// Something that holds the project and does not let go — here a lease taken
/// by hand — makes the drain give up with `busy`, and nothing changes: the
/// project stays attached, searchable, and its next sync is not cancelled.
#[tokio::test]
async fn a_detach_that_cannot_drain_is_busy_and_changes_nothing() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let cfg = config(data.path(), "[server]\ndrain_timeout_secs = 1\n");
    let idx = boot(&cfg).await;
    let id = register(&idx, proj.path(), "demo").await;
    sync(&idx, &id).await;

    let held = idx.lease(&id, Access::Read).await.expect("lease");
    let err = idx.detach(&id).await.expect_err("cannot drain");
    assert_eq!(err.code(), "busy");
    assert!(err.to_string().contains("nothing was changed"), "{err}");
    drop(held);

    let (registry, _) = Registry::load(&cfg.store);
    assert!(registry.detached_entry(&id).is_none());
    assert!(idx.get_project(&id).await.is_ok());
    let again = sync(&idx, &id).await;
    assert_eq!(again.embeddings_computed, 0, "and not cancelled: {again:?}");
}

/// A project alone in its bucket takes every member with it — one per model
/// it was indexed with — and leaves the manifest it was described by.
#[tokio::test]
async fn removing_a_lone_project_deletes_every_member_and_keeps_the_manifest() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let cfg = config(data.path(), PROFILES);
    let idx = boot(&cfg).await;
    let id = register(&idx, proj.path(), "demo").await;
    sync(&idx, &id).await;
    idx.set_default_model("other").expect("switch");
    sync(&idx, &id).await;
    let manifest = proj.path().join(".findery/config.toml");
    std::fs::write(&manifest, "project = \"demo\"\n").expect("manifest");
    let members = member_files(&local_base(proj.path()));
    assert_eq!(members.len(), 2);

    let removed = idx.forget_project(&id).await.expect("removed");
    assert!(removed.deleted && !removed.rows_only, "{removed:?}");
    assert_eq!(removed.files_deleted.len(), 2, "{removed:?}");
    for file in &members {
        assert!(!file.exists(), "{} is gone", file.display());
    }
    assert!(manifest.is_file(), "the manifest is the user's");
    assert_eq!(
        idx.get_project(&id).await.map_err(|e| e.code()).err(),
        Some("not_found")
    );
    let (registry, _) = Registry::load(&cfg.store);
    assert!(registry.detached.is_empty() && registry.file_of("demo").is_none());
}

/// In a shared bucket only the project's rows go; the other project keeps
/// its files and its index.
#[tokio::test]
async fn removing_a_project_from_a_shared_bucket_deletes_its_rows_only() {
    let data = tempfile::tempdir().expect("data");
    let (one, two) = (project_dir(), project_dir());
    let idx = boot(&config(data.path(), "")).await;
    let work = || RegisterProject {
        bucket: Some(findery_core::manifest::BucketRef {
            name: Some("work".into()),
            path: None,
        }),
        ..RegisterProject::default()
    };
    let a = registered(&idx, one.path(), "a", work()).await;
    let b = registered(&idx, two.path(), "b", work()).await;
    sync(&idx, &a).await;
    sync(&idx, &b).await;

    let removed = idx.forget_project(&a).await.expect("removed");
    assert!(removed.rows_only, "{removed:?}");
    assert_eq!(removed.shared_with, vec!["b".to_string()]);
    assert!(removed.files_deleted.is_empty());
    let hits = idx
        .search(&query(
            &b,
            "retry_with_backoff",
            findery_core::model::SearchMode::Hybrid,
        ))
        .await
        .expect("b still searches");
    assert!(!hits.hits.is_empty() && hits.warnings.is_empty());
    let family = idx.family("work").expect("still open");
    assert!(!family.project_ids().await.expect("ids").contains(&a));
}

/// A removal cut short leaves its tombstone; the next boot finishes it before
/// anything opens the files, so nothing is orphaned and nothing comes back.
#[tokio::test]
async fn an_unfinished_removal_is_completed_at_the_next_boot() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let cfg = config(data.path(), "");
    let idx = boot(&cfg).await;
    let id = register(&idx, proj.path(), "demo").await;
    sync(&idx, &id).await;
    idx.close().await.expect("close");
    drop(idx);
    Registry::detach(
        &cfg.store,
        &id,
        DetachedEntry {
            family: "demo".into(),
            base: local_base(proj.path()).display().to_string(),
            removing: true,
        },
    )
    .expect("tombstone");

    let idx = boot(&cfg).await;
    assert!(
        member_files(&local_base(proj.path())).is_empty(),
        "finished"
    );
    assert_eq!(
        idx.get_project(&id).await.map_err(|e| e.code()).err(),
        Some("not_found")
    );
    let (registry, _) = Registry::load(&cfg.store);
    assert!(registry.is_empty(), "{registry:?}");
    assert!(idx.health().await.expect("health").notices.is_empty());
}

/// Windows refuses to delete a file another process holds. The removal stops
/// with `busy`, keeps its tombstone, and the boot after the file is released
/// finishes it.
#[cfg(windows)]
#[tokio::test]
async fn a_held_file_keeps_the_tombstone_and_the_next_boot_finishes_the_removal() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let cfg = config(data.path(), "");
    let idx = boot(&cfg).await;
    let id = register(&idx, proj.path(), "demo").await;
    sync(&idx, &id).await;
    let member = local_member(proj.path(), "mock:128");
    let holder = rusqlite::Connection::open(&member).expect("holder");

    let err = idx.forget_project(&id).await.expect_err("held");
    assert_eq!(err.code(), "busy");
    let (registry, _) = Registry::load(&cfg.store);
    assert!(
        registry.detached_entry(&id).is_some_and(|e| e.removing),
        "tombstone kept"
    );

    drop(holder);
    idx.close().await.expect("close");
    drop(idx);
    let idx = boot(&cfg).await;
    assert!(!member.exists());
    assert!(idx.get_project(&id).await.is_err());
    let (registry, _) = Registry::load(&cfg.store);
    assert!(registry.detached.is_empty());
}

/// A bucket whose projects are all detached stays closed at the next start,
/// and `register` attaches it again without re-indexing anything.
#[tokio::test]
async fn a_detached_bucket_is_not_reopened_at_boot_and_register_attaches_it_again() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let cfg = config(data.path(), "");
    let idx = boot(&cfg).await;
    let id = register(&idx, proj.path(), "demo").await;
    sync(&idx, &id).await;
    idx.detach(&id).await.expect("detach");
    idx.close().await.expect("close");
    drop(idx);

    let idx = boot(&cfg).await;
    assert!(idx.family("demo").is_err(), "not reopened");
    assert!(idx.list_projects().await.expect("list").is_empty());
    assert!(is_released(&local_member(proj.path(), "mock:128")));

    register(&idx, proj.path(), "demo").await;
    assert_eq!(sync(&idx, &id).await.embeddings_computed, 0);
}

/// A stray handle — a search in flight, a finished job still holding one —
/// used to keep the file open, and Windows refuses to delete an open file. The
/// removal closes every member for every clone, so the files go regardless.
#[tokio::test]
async fn a_bucket_rm_deletes_the_files_while_another_handle_is_alive() {
    let data = tempfile::tempdir().expect("data");
    let idx = boot(&config(data.path(), "")).await;
    idx.create_bucket("work").await.expect("create");
    let stray = idx.family("work").expect("open").members()[0].clone();
    let file = stray.bucket.path().to_path_buf();
    assert!(file.is_file());

    let removed = idx.forget_bucket("work", None).await.expect("removed");
    assert!(removed.removed);
    assert!(!file.exists(), "the file went while a clone was alive");
    assert!(!data.path().join("buckets/work").exists());
    assert_eq!(
        stray.bucket.totals().await.map_err(|e| e.code()).err(),
        Some("not_found")
    );
    assert!(idx.family("work").is_err());
}

/// A bucket at an explicit path loses its registry entry only once its files
/// are gone, and the directory around it is the user's.
#[tokio::test]
async fn a_bucket_elsewhere_is_unregistered_only_after_its_files_are_gone() {
    let data = tempfile::tempdir().expect("data");
    let cfg = config(data.path(), "");
    let idx = boot(&cfg).await;
    let at = data.path().join("elsewhere/team.db");
    let at_str = root_of(&at);
    idx.ensure_bucket_at("team", Some(&at_str))
        .await
        .expect("open");
    idx.create_bucket("team").await.expect("member");
    let (registry, _) = Registry::load(&cfg.store);
    assert_eq!(registry.file_of("team"), Some(at_str.as_str()));

    assert!(
        idx.forget_bucket("team", None)
            .await
            .expect("removed")
            .removed
    );
    assert!(member_files(&at).is_empty());
    let (registry, _) = Registry::load(&cfg.store);
    assert_eq!(registry.file_of("team"), None);
    assert!(
        data.path().join("elsewhere").is_dir(),
        "the user's directory stays"
    );
}

#[tokio::test]
async fn removing_a_bucket_that_was_never_there_is_not_found() {
    let data = tempfile::tempdir().expect("data");
    let idx = boot(&config(data.path(), "")).await;
    let err = error_of(idx.forget_bucket("never", None).await);
    assert_eq!(err.code(), "not_found");
}

/// One model's file of a bucket can go on its own; the bucket's last one is
/// the bucket, and is refused.
#[tokio::test]
async fn a_bucket_rm_of_one_model_deletes_that_member_only() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let idx = boot(&config(data.path(), PROFILES)).await;
    let work = RegisterProject {
        bucket: Some(findery_core::manifest::BucketRef {
            name: Some("work".into()),
            path: None,
        }),
        ..RegisterProject::default()
    };
    let id = registered(&idx, proj.path(), "demo", work).await;
    sync(&idx, &id).await;
    idx.set_default_model("other").expect("switch");
    sync(&idx, &id).await;
    let base = data.path().join("buckets/work/bucket.db");
    assert_eq!(member_files(&base).len(), 2);

    idx.forget_bucket("work", Some("other"))
        .await
        .expect("one member");
    assert_eq!(member_files(&base).len(), 1);
    let err = error_of(idx.forget_bucket("work", Some("embedder")).await);
    assert_eq!(
        err.code(),
        "invalid",
        "the last member is the bucket: {err}"
    );
    idx.set_default_model("embedder").expect("switch back");
    assert_eq!(
        sync(&idx, &id).await.embeddings_computed,
        0,
        "the other model untouched"
    );
}

/// The bug this locks: `bucket rm` on a bucket whose projects were all
/// detached answered `not_found` — the daemon no longer served it — so its
/// files could only go by hand. It is found on disk and removed.
#[tokio::test]
async fn removing_a_bucket_whose_projects_are_all_detached_deletes_its_files() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let cfg = config(data.path(), "");
    let idx = boot(&cfg).await;
    let work = RegisterProject {
        bucket: Some(findery_core::manifest::BucketRef {
            name: Some("work".into()),
            path: None,
        }),
        ..RegisterProject::default()
    };
    let id = registered(&idx, proj.path(), "demo", work).await;
    sync(&idx, &id).await;
    idx.detach(&id).await.expect("detach");
    assert!(idx.family("work").is_err(), "closed with its last project");

    let removed = idx.forget_bucket("work", None).await.expect("removed");
    assert!(removed.removed, "{removed:?}");
    assert_eq!(removed.projects, ["demo"]);
    assert!(!data.path().join("buckets/work").exists());
    let (registry, _) = Registry::load(&cfg.store);
    assert!(registry.detached_entry(&id).is_none(), "{registry:?}");
}

/// The bug this locks: whether a bucket was shared was read from the daemon's
/// map of homes, which misses a project whose row it could not read — and a
/// sibling's `rm` then deleted the files that project's rows live in. The
/// files themselves say who lives there.
#[tokio::test]
async fn removing_a_project_keeps_files_another_projects_rows_live_in() {
    let data = tempfile::tempdir().expect("data");
    let (one, two) = (project_dir(), project_dir());
    let idx = boot(&config(data.path(), "")).await;
    let work = || RegisterProject {
        bucket: Some(findery_core::manifest::BucketRef {
            name: Some("work".into()),
            path: None,
        }),
        ..RegisterProject::default()
    };
    let a = registered(&idx, one.path(), "a", work()).await;
    let b = registered(&idx, two.path(), "b", work()).await;
    sync(&idx, &b).await;
    // `b` homeless, as a row boot could not read leaves it.
    idx.forget_home(&b);

    let removed = idx.forget_project(&a).await.expect("removed");
    assert!(removed.rows_only, "{removed:?}");
    assert_eq!(removed.shared_with, ["b"]);
    let family = idx.family("work").expect("still open");
    assert!(family.project_ids().await.expect("ids").contains(&b));
}

/// The bug this locks: at boot, a Qdrant that did not answer was logged and
/// the removal went on — the file gone, the collection kept, inherited by the
/// next project of the same id and model. The tombstone stays instead.
#[tokio::test]
async fn a_boot_removal_whose_vector_store_is_down_keeps_its_tombstone() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let cfg = config(data.path(), "");
    let idx = boot(&cfg).await;
    let id = register(&idx, proj.path(), "demo").await;
    sync(&idx, &id).await;
    idx.close().await.expect("close");
    drop(idx);
    Registry::detach(
        &cfg.store,
        &id,
        DetachedEntry {
            family: "demo".into(),
            base: local_base(proj.path()).display().to_string(),
            removing: true,
        },
    )
    .expect("tombstone");

    let mut down = cfg.clone();
    down.store.backend = findery_core::config::StoreBackend::Qdrant;
    down.store.qdrant.url = "http://127.0.0.1:1".into();
    down.store.qdrant.timeout_secs = 2;
    let (registry, _) = Registry::load(&down.store);
    let (unfinished, notices) = crate::lifecycle::tombstones::complete(&down, &registry).await;

    assert!(unfinished.families.contains("demo"), "not served meanwhile");
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("qdrant"), "{notices:?}");
    let (registry, _) = Registry::load(&cfg.store);
    assert!(registry.detached_entry(&id).is_some_and(|e| e.removing));
    assert_eq!(
        member_files(&local_base(proj.path())).len(),
        1,
        "the file stays"
    );
}

/// A `default` bucket's tombstone — only a hand edit writes one — is refused:
/// the bucket every install has is kept, and served.
#[tokio::test]
async fn a_tombstone_for_the_default_bucket_is_refused_and_dropped() {
    let data = tempfile::tempdir().expect("data");
    let cfg = config(data.path(), "");
    let idx = boot(&cfg).await;
    idx.create_bucket(findery_core::model::DEFAULT_BUCKET)
        .await
        .expect("member");
    idx.close().await.expect("close");
    drop(idx);
    Registry::mark_removing(&cfg.store, findery_core::model::DEFAULT_BUCKET).expect("tombstone");

    let idx = boot(&cfg).await;
    assert!(
        idx.family(findery_core::model::DEFAULT_BUCKET).is_ok(),
        "still served"
    );
    assert!(
        !member_files(&data.path().join("bucket.db")).is_empty(),
        "its file stays"
    );
    let (registry, _) = Registry::load(&cfg.store);
    assert!(registry.removing_families.is_empty(), "{registry:?}");
    let notices = idx.health().await.expect("health").notices;
    assert!(notices.iter().any(|n| n.contains("refused")), "{notices:?}");
}
