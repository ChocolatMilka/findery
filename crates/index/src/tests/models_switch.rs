//! Several models over one bucket: reads that never create, writes that seed,
//! switching back for free, and a search that degrades instead of failing.

use findery_core::model::SearchMode;

use super::*;
use crate::Access;

/// A read under a model the project was never indexed with creates no file:
/// it answers from the sibling's chunks, says so, and refuses only the
/// question that needs vectors.
#[tokio::test]
async fn a_read_under_a_new_model_never_creates_its_file_and_degrades_to_keyword() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let idx = boot(&config(data.path(), PROFILES)).await;
    let id = register(&idx, proj.path(), "demo").await;
    sync(&idx, &id).await;
    let other_file = local_member(proj.path(), "other:128");

    idx.set_default_model("other").expect("switch");
    let project = idx
        .get_project(&id)
        .await
        .expect("the row is read from a sibling");
    assert_eq!(project.model.as_ref().map(|m| m.as_str()), Some("other"));
    let stats = idx.stats(&id).await.expect("stats");
    assert_eq!(
        (stats.files, stats.vectors),
        (0, 0),
        "nothing indexed under `other` yet"
    );
    assert!(idx.file_manifest(&id).await.expect("manifest").is_empty());

    let hybrid = idx
        .search(&query(&id, "retry_with_backoff", SearchMode::Hybrid))
        .await
        .expect("a hybrid search degrades, it does not fail");
    assert!(!hybrid.hits.is_empty(), "the keyword leg answered");
    assert_eq!(hybrid.warnings.len(), 1, "{:?}", hybrid.warnings);
    let warning = &hybrid.warnings[0];
    assert!(warning.contains("`other`"), "names the model: {warning}");
    assert!(
        warning.contains("findery sync -p demo"),
        "names the fix: {warning}"
    );

    let err = idx
        .search(&query(&id, "retry", SearchMode::Vector))
        .await
        .expect_err("a vector search has nothing to compare with");
    assert_eq!(err.code(), "vectors_unavailable");
    assert_eq!(err.http_status(), 409);

    assert!(!other_file.exists(), "no read created the member");
    assert_eq!(member_files(&local_base(proj.path())).len(), 1);
}

/// The first write under a new model creates its member seeded with the
/// project's row only — so the sync re-chunks and embeds from zero, and the
/// old model's file is untouched beside it.
#[tokio::test]
async fn the_first_write_under_a_new_model_seeds_the_row_and_embeds_from_zero() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let idx = boot(&config(data.path(), PROFILES)).await;
    let id = register(&idx, proj.path(), "demo").await;
    let first = sync(&idx, &id).await;
    assert!(first.embeddings_computed > 0);

    idx.set_default_model("other").expect("switch");
    let under_other = sync(&idx, &id).await;
    assert_eq!(
        under_other.files_added, first.files_added,
        "every file is new here"
    );
    assert_eq!(
        under_other.embeddings_cached, 0,
        "a new space caches nothing"
    );
    assert_eq!(under_other.embeddings_computed, first.embeddings_computed);
    assert!(local_member(proj.path(), "other:128").is_file());
    assert!(local_member(proj.path(), "mock:128").is_file());

    let hits = idx
        .search(&query(&id, "retry backoff", SearchMode::Hybrid))
        .await
        .expect("search");
    assert!(hits.warnings.is_empty(), "{:?}", hits.warnings);
    assert!(!hits.hits.is_empty());
}

/// Switching back is free: the first model's vectors never moved.
#[tokio::test]
async fn switching_back_to_a_model_re_embeds_nothing() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let idx = boot(&config(data.path(), PROFILES)).await;
    let id = register(&idx, proj.path(), "demo").await;
    sync(&idx, &id).await;
    idx.set_default_model("other").expect("switch");
    sync(&idx, &id).await;

    idx.set_default_model("embedder").expect("switch back");
    let back = sync(&idx, &id).await;
    assert_eq!(back.embeddings_computed, 0, "{back:?}");
    let full = idx
        .sync(&id, SyncMode::Full, &crate::ProgressHandle::detached())
        .await
        .expect("full");
    assert_eq!(full.embeddings_computed, 0, "gate 3 still holds: {full:?}");
}

/// Two aliases of one identity and width are one vector space: one file, and
/// the second alias embeds nothing.
#[tokio::test]
async fn two_aliases_with_the_same_recipe_share_one_file() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let idx = boot(&config(data.path(), PROFILES)).await;
    let id = register(&idx, proj.path(), "demo").await;
    idx.set_default_model("other").expect("switch");
    assert!(sync(&idx, &id).await.embeddings_computed > 0);

    idx.set_default_model("twin").expect("switch");
    let twin = sync(&idx, &id).await;
    assert_eq!(twin.embeddings_computed, 0, "{twin:?}");
    assert_eq!(
        member_files(&local_base(proj.path())).len(),
        2,
        "embedder + other"
    );
}

/// A pin names the model of one project, whatever the default; a pin no
/// profile defines is refused before anything is written.
#[tokio::test]
async fn a_pinned_project_follows_its_pin_and_an_unknown_pin_writes_nothing() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let idx = boot(&config(data.path(), PROFILES)).await;
    let pinned = RegisterProject {
        model: Some("other".into()),
        ..RegisterProject::default()
    };
    let id = registered(&idx, proj.path(), "demo", pinned).await;
    sync(&idx, &id).await;
    let lease = idx.lease(&id, Access::Read).await.expect("lease");
    assert_eq!(lease.alias.as_str(), "other");
    assert_eq!(lease.recipe.as_str(), "other:128");
    drop(lease);
    assert!(
        !local_member(proj.path(), "mock:128").exists(),
        "only the pin's file"
    );

    let other = project_dir();
    let err = idx
        .register(RegisterProject {
            root: root_of(other.path()),
            model: Some("harier".into()),
            ..RegisterProject::default()
        })
        .await
        .expect_err("an unknown pin is refused");
    assert_eq!(err.code(), "model_unknown");
    assert!(
        !other.path().join(".findery").exists(),
        "nothing was written"
    );
}

/// Re-registering writes the row into every member, so a scope declared once
/// is honoured under every model.
#[tokio::test]
async fn a_registration_refreshes_the_row_of_every_member() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let idx = boot(&config(data.path(), PROFILES)).await;
    let id = register(&idx, proj.path(), "demo").await;
    sync(&idx, &id).await;
    idx.set_default_model("other").expect("switch");
    sync(&idx, &id).await;

    let scoped = RegisterProject {
        scopes: Some(
            [(
                "net".to_string(),
                findery_core::model::SearchFilter {
                    paths: vec!["src/net.rs".into()],
                    ..Default::default()
                },
            )]
            .into(),
        ),
        ..RegisterProject::default()
    };
    registered(&idx, proj.path(), "demo", scoped).await;
    let lease = idx.lease(&id, Access::Read).await.expect("lease");
    for member in lease.family.members() {
        let row = member
            .bucket
            .get_project(&id)
            .await
            .expect("read")
            .expect("row");
        assert!(
            row.spec.scopes.contains_key("net"),
            "{}",
            member.bucket.path().display()
        );
    }
}

/// A runtime that is down costs the vector leg, not the search, and `health`
/// names the profile that is down.
#[tokio::test]
async fn a_model_whose_runtime_is_down_degrades_search_and_shows_in_health() {
    let down = "[models.down]\nbackend = \"openai_compat\"\nbase_url = \"http://127.0.0.1:1\"\n\
                weights = \"mock\"\ndim = 128\nverify_dim = false\nmax_retries = 0\n\
                timeout_secs = 2\n";
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let idx = boot(&config(data.path(), down)).await;
    let id = register(&idx, proj.path(), "demo").await;
    sync(&idx, &id).await;

    // Same weights, same width: the same file, served by a runtime nobody started.
    idx.set_default_model("down").expect("switch");
    let response = idx
        .search(&query(&id, "retry_with_backoff", SearchMode::Hybrid))
        .await
        .expect("degraded, not failed");
    assert!(!response.hits.is_empty());
    assert_eq!(response.warnings.len(), 1, "{:?}", response.warnings);
    assert!(
        response.warnings[0].contains("`down`") && response.warnings[0].contains("runtime"),
        "{:?}",
        response.warnings
    );

    let health = idx.health().await.expect("health");
    assert!(
        !health.embedder_ok,
        "the default is now the one that is down"
    );
    let reported = health
        .models
        .iter()
        .find(|m| m.profile.alias.as_str() == "down")
        .expect("listed");
    assert!(reported.profile.connected && !reported.ok, "{reported:?}");
    let embedder = health
        .models
        .iter()
        .find(|m| m.profile.alias.as_str() == "embedder")
        .expect("listed");
    assert!(embedder.ok && !embedder.profile.is_default);
}
