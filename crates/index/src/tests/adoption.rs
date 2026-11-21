//! A bucket written before members existed: adopted at boot when it can be,
//! served in place when it cannot — and never shadowed by an empty file.

use findery_core::model::SearchMode;

use super::*;

/// Builds an index, closes it, and turns its member back into the single
/// `bucket.db` an older daemon wrote. Returns the hits a search gave before.
async fn a_legacy_bucket(data: &Path, root: &Path) -> (ProjectId, Vec<(String, u32)>) {
    let idx = boot(&config(data, "")).await;
    let id = register(&idx, root, "demo").await;
    sync(&idx, &id).await;
    let before = located(&idx, &id).await;
    idx.close().await.expect("close");
    drop(idx);
    std::fs::rename(local_member(root, "mock:128"), local_base(root)).expect("back to legacy");
    (id, before)
}

/// What a search answers, as (path, start line) — what an agent cites.
async fn located(idx: &Indexer, id: &ProjectId) -> Vec<(String, u32)> {
    idx.search(&query(id, "retry backoff connection", SearchMode::Hybrid))
        .await
        .expect("search")
        .hits
        .into_iter()
        .map(|h| (h.chunk.path, h.chunk.start_line))
        .collect()
}

#[tokio::test]
async fn a_legacy_bucket_is_adopted_at_boot_and_serves_the_same_hits() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let (id, before) = a_legacy_bucket(data.path(), proj.path()).await;
    assert!(!before.is_empty());

    let idx = boot(&config(data.path(), "")).await;
    assert!(
        !local_base(proj.path()).exists(),
        "the legacy file was renamed"
    );
    assert!(local_member(proj.path(), "mock:128").is_file());
    let health = idx.health().await.expect("health");
    assert!(
        health.notices.iter().any(|n| n.contains("is now")),
        "the adoption is reported: {:?}",
        health.notices
    );
    assert_eq!(
        located(&idx, &id).await,
        before,
        "the same index, under its new name"
    );
    assert_eq!(sync(&idx, &id).await.embeddings_computed, 0);
}

/// Another process holding the legacy file keeps it where it is. It still
/// serves — and the resolver, which consults it before creating, never puts an
/// empty member beside it.
#[tokio::test]
async fn a_legacy_file_kept_in_place_still_serves_and_is_never_recreated_empty() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let (id, before) = a_legacy_bucket(data.path(), proj.path()).await;
    let holder = rusqlite::Connection::open(local_base(proj.path())).expect("holder");
    let _: i64 = holder
        .query_row("SELECT count(*) FROM projects", [], |r| r.get(0))
        .expect("the holder has read the file, so it holds its journals");

    let idx = boot(&config(data.path(), "")).await;
    let health = idx.health().await.expect("health");
    assert!(
        health
            .notices
            .iter()
            .any(|n| n.contains("stays where it is")),
        "{:?}",
        health.notices
    );
    assert_eq!(located(&idx, &id).await, before);
    let again = sync(&idx, &id).await;
    assert_eq!(again.embeddings_computed, 0, "{again:?}");
    assert!(local_base(proj.path()).is_file());
    assert!(
        !local_member(proj.path(), "mock:128").exists(),
        "no empty member was created beside the file that serves"
    );
    idx.close().await.expect("close");
    drop(holder);
}

/// The bug this locks: a base file holding rows but no recipe — registered,
/// never embedded — was not opened, so its projects had no home and vanished
/// from every listing. Its rows count; the first write claims the file.
#[tokio::test]
async fn an_unclaimed_base_file_with_rows_keeps_its_projects() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let cfg = config(data.path(), "");
    {
        let idx = boot(&cfg).await;
        let default = RegisterProject {
            bucket: Some(findery_core::manifest::BucketRef {
                name: Some(findery_core::model::DEFAULT_BUCKET.into()),
                path: None,
            }),
            ..RegisterProject::default()
        };
        registered(&idx, proj.path(), "demo", default).await;
        idx.close().await.expect("close");
    }
    // The member as an older daemon left it: the base name, no recipe.
    let base = data.path().join("bucket.db");
    let member = member_files(&base).pop().expect("the member");
    std::fs::rename(&member, &base).expect("back to the base name");
    let conn = rusqlite::Connection::open(&base).expect("open");
    conn.execute("DELETE FROM meta WHERE key = 'embedding_recipe'", [])
        .expect("forget the recipe");
    drop(conn);

    let idx = boot(&cfg).await;
    let listed: Vec<String> = idx
        .list_projects()
        .await
        .expect("list")
        .into_iter()
        .map(|p| p.id.to_string())
        .collect();
    assert_eq!(listed, ["demo"]);
    let id = ProjectId::new("demo").expect("id");
    assert_eq!(sync(&idx, &id).await.files_added, 3);
    assert_eq!(
        member_files(&base),
        std::slice::from_ref(&base),
        "claimed, no file beside it"
    );
}
