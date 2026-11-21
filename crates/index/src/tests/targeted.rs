//! A targeted sync — `add_document` without text, a watcher's touched paths —
//! admits exactly what the project's walk admits.

use super::*;

/// The bug this locks: a named path bypassed every walk rule, so
/// `add_document(".env")` indexed the secrets, and the bucket's own files
/// could be indexed into themselves.
#[tokio::test]
async fn a_targeted_sync_never_indexes_what_the_walk_excludes() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    std::fs::write(proj.path().join(".env"), "SECRET=hunter2\n").expect("write");
    let idx = boot(&config(data.path(), "")).await;
    let id = register(&idx, proj.path(), "demo").await;
    sync(&idx, &id).await;
    let member = local_member(proj.path(), "mock:128");
    let member_rel = format!(
        ".findery/{}",
        member.file_name().and_then(|n| n.to_str()).expect("name")
    );

    for excluded in [".env", member_rel.as_str()] {
        let report = idx
            .add_document(&id, excluded, None)
            .await
            .expect("a refusal is a warning");
        assert_eq!(
            report.files_added + report.files_updated,
            0,
            "{excluded}: {report:?}"
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.starts_with(&format!("{excluded}: excluded"))),
            "{excluded}: {:?}",
            report.warnings
        );
    }
    let escaping = error_of(idx.add_document(&id, "src/../.env", None).await);
    assert_eq!(escaping.code(), "invalid");

    let status = idx.stats(&id).await.expect("stats");
    assert_eq!(status.files, 3, "only the fixture's own files: {status:?}");
}

/// What the walk admits, a targeted sync indexes — the rules narrow it, they
/// do not stop it.
#[tokio::test]
async fn a_targeted_sync_of_an_admitted_file_indexes_it() {
    let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
    let idx = boot(&config(data.path(), "")).await;
    let id = register(&idx, proj.path(), "demo").await;
    std::fs::write(proj.path().join("src/new.rs"), "pub fn fresh() {}\n").expect("write");

    let report = idx
        .add_document(&id, "src/new.rs", None)
        .await
        .expect("synced");
    assert_eq!(report.files_added, 1, "{report:?}");
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
}
