//! A registration is a merge: what a request leaves out is kept, an empty
//! value clears, and a root registered again is the same project.

use std::collections::BTreeMap;

use findery_core::model::SearchFilter;

use super::*;

fn declared() -> RegisterProject {
    RegisterProject {
        scopes: Some(BTreeMap::from([(
            "api".to_string(),
            SearchFilter {
                paths: vec!["src/**".into()],
                ..SearchFilter::default()
            },
        )])),
        instruction: Some("Given a question, retrieve the answer.".into()),
        model: Some("other".into()),
        extensions: Some(vec!["rs".into()]),
        ..RegisterProject::default()
    }
}

#[tokio::test]
async fn a_registration_that_names_only_the_root_keeps_everything_else() {
    let data = tempfile::tempdir().expect("data");
    let idx = boot(&config(data.path(), PROFILES)).await;
    let proj = project_dir();
    let id = registered(&idx, proj.path(), "demo", declared()).await;

    let again = idx
        .register(RegisterProject::new(root_of(proj.path())))
        .await
        .expect("register again");
    assert_eq!(again.id, id, "the same root is the same project");
    let spec = again.spec;
    assert!(spec.scopes.contains_key("api"), "{:?}", spec.scopes);
    assert_eq!(
        spec.instruction.as_deref(),
        Some("Given a question, retrieve the answer.")
    );
    assert_eq!(spec.model.as_ref().map(|m| m.as_str()), Some("other"));
    assert_eq!(spec.extensions, ["rs"]);
    assert_eq!(again.bucket, "demo", "the bucket it lives in is kept too");
}

#[tokio::test]
async fn an_explicitly_empty_value_clears_what_was_declared() {
    let data = tempfile::tempdir().expect("data");
    let idx = boot(&config(data.path(), PROFILES)).await;
    let proj = project_dir();
    registered(&idx, proj.path(), "demo", declared()).await;

    let cleared = idx
        .register(RegisterProject {
            id: Some("demo".into()),
            scopes: Some(BTreeMap::new()),
            instruction: Some(String::new()),
            model: Some(String::new()),
            ..RegisterProject::new(root_of(proj.path()))
        })
        .await
        .expect("register");
    let spec = cleared.spec;
    assert!(spec.scopes.is_empty());
    assert_eq!(spec.instruction, None);
    assert_eq!(spec.model, None, "unpinned: it follows the default again");
}

#[tokio::test]
async fn a_pin_no_profile_defines_is_refused_before_anything_changes() {
    let data = tempfile::tempdir().expect("data");
    let idx = boot(&config(data.path(), PROFILES)).await;
    let proj = project_dir();
    let id = registered(&idx, proj.path(), "demo", declared()).await;

    let err = idx
        .register(RegisterProject {
            model: Some("nope".into()),
            ..RegisterProject::new(root_of(proj.path()))
        })
        .await
        .expect_err("unknown profile");
    assert_eq!(err.code(), "model_unknown");
    let kept = idx.get_project(&id).await.expect("project").spec;
    assert_eq!(kept.model.as_ref().map(|m| m.as_str()), Some("other"));
}

fn into_bucket(name: &str) -> RegisterProject {
    RegisterProject {
        bucket: Some(findery_core::manifest::BucketRef {
            name: Some(name.into()),
            path: None,
        }),
        ..RegisterProject::default()
    }
}

/// The bug this locks: a row that could not be read was taken for "not
/// attached", so the spec was reset to the defaults and the project moved out
/// of its real home. Such a registration is refused, and changes nothing.
#[tokio::test]
async fn a_registration_that_cannot_read_the_current_row_changes_nothing() {
    let data = tempfile::tempdir().expect("data");
    let idx = boot(&config(data.path(), PROFILES)).await;
    let proj = project_dir();
    let id = registered(&idx, proj.path(), "demo", declared()).await;
    let home = idx.family("demo").expect("open");
    home.close().await.expect("close");

    let err = idx
        .register(RegisterProject::new(root_of(proj.path())))
        .await
        .expect_err("the row cannot be read");
    assert_eq!(err.code(), "not_found");
    assert_eq!(idx.home_of(&id).expect("home"), "demo", "not moved");
}

/// The bug this locks: a move took the project out of its old bucket before
/// the new one was written, so any failure on the way lost it. The target is
/// written first; a target that fails leaves the project where it was.
#[tokio::test]
async fn a_move_whose_target_fails_leaves_the_project_where_it_was() {
    let data = tempfile::tempdir().expect("data");
    let idx = boot(&config(data.path(), "")).await;
    let proj = project_dir();
    let id = register(&idx, proj.path(), "demo").await;
    let before = sync(&idx, &id).await;
    idx.ensure_bucket("work").await.expect("open");
    idx.family("work")
        .expect("open")
        .close()
        .await
        .expect("close");

    let mut moving = into_bucket("work");
    moving.id = Some("demo".into());
    moving.root = root_of(proj.path());
    let err = idx
        .register(moving)
        .await
        .expect_err("the target is closed");
    assert_eq!(err.code(), "not_found");
    assert_eq!(idx.home_of(&id).expect("home"), "demo");
    let stats = idx.stats(&id).await.expect("stats");
    assert_eq!(
        stats.chunks, before.chunks_written,
        "nothing left the old bucket"
    );
}

/// The bug this locks: the MCP bridge registers with a root alone, and a
/// detached project registered so landed in a fresh bucket — then the
/// reattach dropped the only record of where its index was.
#[tokio::test]
async fn re_registering_a_detached_project_by_root_reattaches_its_bucket() {
    let data = tempfile::tempdir().expect("data");
    let idx = boot(&config(data.path(), "")).await;
    let proj = project_dir();
    let id = registered(&idx, proj.path(), "demo", into_bucket("work")).await;
    sync(&idx, &id).await;
    idx.detach(&id).await.expect("detach");

    let again = idx
        .register(RegisterProject {
            id: Some("demo".into()),
            ..RegisterProject::new(root_of(proj.path()))
        })
        .await
        .expect("attached again");
    assert_eq!(again.bucket, "work");
    assert_eq!(
        sync(&idx, &id).await.embeddings_computed,
        0,
        "nothing re-indexed"
    );
}
