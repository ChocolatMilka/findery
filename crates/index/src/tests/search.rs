//! Multi-leg search over a real project: workflow intents, raw queries, `like`
//! by stored vector, pairing and near-duplicates.

use findery_core::api::SearchResponse;

use findery_embed::mock::QueryLog;

use super::*;

/// The mock, recording every query it is handed into `log`.
fn logging_builder(log: QueryLog) -> BuildFn {
    Arc::new(move |cfg: EmbedderConfig, _known: Option<usize>| {
        let log = log.clone();
        Box::pin(async move {
            let embedder: Arc<dyn Embedder> =
                Arc::new(MockEmbedder::new(cfg.dim).with_query_log(log));
            Ok(embedder)
        })
    })
}

/// A synced demo project, its engine and its query log. Fields drop in
/// order: the engine releases its files before the directories go.
struct Fixture {
    idx: Arc<Indexer>,
    id: ProjectId,
    log: QueryLog,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

async fn synced(extra: &str) -> Fixture {
    let data = tempfile::tempdir().expect("data dir");
    let log = QueryLog::default();
    let idx = boot_with(&config(data.path(), extra), logging_builder(log.clone())).await;
    let root = project_dir();
    let id = register(&idx, root.path(), "demo").await;
    sync(&idx, &id).await;
    Fixture {
        idx,
        id,
        log,
        _dirs: (data, root),
    }
}

fn logged(log: &QueryLog) -> Vec<String> {
    log.lock().expect("unpoisoned").clone()
}

fn intent(id: &ProjectId, name: &str) -> SearchRequest {
    query(id, "retry backoff", SearchMode::Hybrid).with_intent(name)
}

async fn run(idx: &Indexer, req: &SearchRequest) -> SearchResponse {
    idx.search(req).await.expect("search")
}

#[tokio::test]
async fn a_workflow_intent_runs_every_leg_and_echoes_them() {
    let Fixture { idx, id, log, .. } = synced("").await;
    let response = run(&idx, &intent(&id, "review")).await;

    let labels: Vec<&str> = response.legs.iter().map(|l| l.label.as_str()).collect();
    assert_eq!(labels, ["callers", "tests", "docs", "similar"]);
    let echo = response.resolved.as_ref().expect("echoed");
    assert_eq!(echo.intent.as_deref(), Some("review"));
    assert_eq!(echo.legs.len(), 4);
    let first: Vec<_> = response.legs[0].hits.iter().map(|h| &h.chunk.id).collect();
    let legacy: Vec<_> = response.hits.iter().map(|h| &h.chunk.id).collect();
    assert_eq!(
        legacy, first,
        "`hits` repeats the first leg for older clients"
    );

    // Four legs, four distinct instructions, each embedded exactly once.
    let queries = logged(&log);
    assert_eq!(queries.len(), 4, "{queries:?}");
    assert!(
        queries[0].contains("a Rust codebase"),
        "the instruction names the project's code languages: {}",
        queries[0]
    );
}

#[tokio::test]
async fn a_single_search_answers_one_list_and_no_legs() {
    let Fixture { idx, id, .. } = synced("").await;
    let response = run(&idx, &intent(&id, "usage")).await;
    assert!(response.legs.is_empty());
    assert!(!response.hits.is_empty());
    let echo = response.resolved.expect("echoed");
    assert_eq!(echo.intent.as_deref(), Some("usage"));
}

#[tokio::test]
async fn an_unknown_intent_is_refused_with_the_known_names() {
    let Fixture { idx, id, .. } = synced("").await;
    let err = error_of(idx.search(&intent(&id, "telepathy")).await);
    assert_eq!(err.code(), "invalid");
    assert!(err.to_string().contains("doc-sync"), "{err}");
}

#[tokio::test]
async fn a_raw_query_reaches_the_embedder_verbatim() {
    let Fixture { idx, id, log, .. } = synced("").await;
    let mut req = query(&id, "fn retry_with_backoff", SearchMode::Vector).with_intent("usage");
    req.raw_query = true;
    run(&idx, &req).await;
    assert_eq!(logged(&log), ["fn retry_with_backoff"]);
}

#[tokio::test]
async fn like_searches_with_the_stored_vector_embeds_nothing_and_leaves_itself_out() {
    let Fixture { idx, id, log, .. } = synced("").await;
    let found = run(&idx, &query(&id, "retry_with_backoff", SearchMode::Keyword)).await;
    let example = found
        .hits
        .first()
        .expect("the keyword list finds it")
        .chunk
        .clone();

    for locator in [example.locator(), example.id.to_string()] {
        let mut req = query(&id, "", SearchMode::Hybrid);
        req.like = Some(locator.clone());
        let response = run(&idx, &req).await;
        assert!(logged(&log).is_empty(), "zero embedding for {locator}");
        assert!(!response.hits.is_empty(), "{locator}: the others are found");
        let ids: Vec<_> = response.hits.iter().map(|h| &h.chunk.id).collect();
        assert!(!ids.contains(&&example.id), "{locator}: itself left out");
        let echo = response.resolved.expect("echoed");
        assert!(echo.legs[0].by_example);
    }
}

#[tokio::test]
async fn like_on_a_line_no_chunk_covers_is_not_found() {
    let Fixture { idx, id, .. } = synced("").await;
    let mut req = query(&id, "", SearchMode::Hybrid);
    req.like = Some("src/net.rs:L900-L950".into());
    assert_eq!(error_of(idx.search(&req).await).code(), "not_found");

    req.like = Some("../outside.rs:L1".into());
    assert_eq!(error_of(idx.search(&req).await).code(), "invalid");
}

#[tokio::test]
async fn pairing_flags_a_doc_whose_best_code_match_is_weak() {
    // The mock's vectors of different texts are nearly orthogonal: every doc
    // is drifted at the default threshold, and none at the lowest one.
    let Fixture { idx, id, log, .. } = synced("").await;
    let response = run(&idx, &intent(&id, "doc-sync")).await;
    assert_eq!(response.legs.len(), 2);
    assert!(
        !response.legs[0].hits.is_empty(),
        "the README section is found"
    );
    assert_eq!(response.pairs.len(), response.legs[0].hits.len());
    assert!(
        response.pairs.iter().all(|p| p.drifted),
        "{:?}",
        response.pairs
    );
    let first = &response.pairs[0];
    assert!(first.matches.len() <= 2 && !first.matches.is_empty());
    assert!(response.hit(first.matches[0].right).is_some());
    assert_eq!(logged(&log).len(), 2, "pairing embeds nothing of its own");

    let Fixture {
        idx: lenient, id, ..
    } = synced("[search]\ndrift_below = -1.0\n").await;
    let response = run(&lenient, &intent(&id, "doc-sync")).await;
    assert!(response.pairs.iter().all(|p| !p.drifted));
}

#[tokio::test]
async fn divergence_reports_near_duplicates_across_files_within_the_band() {
    let wide = "[search]\nduplicate_min = -1.0\nduplicate_max = 1.0\n";
    let Fixture { idx, id, .. } = synced(wide).await;
    let response = run(&idx, &intent(&id, "divergence")).await;
    assert!(
        !response.duplicates.is_empty(),
        "net.rs and pool.rs are compared"
    );
    for duplicate in &response.duplicates {
        let a = response.hit(duplicate.a).expect("a hit");
        let b = response.hit(duplicate.b).expect("a hit");
        assert_ne!(a.chunk.path, b.chunk.path, "never two chunks of one file");
        assert_ne!(a.chunk.lang, findery_core::model::Lang::Markdown);
    }

    let narrow = "[search]\nduplicate_min = 0.999\nduplicate_max = 1.0\n";
    let Fixture { idx, id, .. } = synced(narrow).await;
    let response = run(&idx, &intent(&id, "divergence")).await;
    assert!(response.duplicates.is_empty(), "nothing is that similar");
}

#[tokio::test]
async fn the_intent_listing_carries_the_projects_own_layer() {
    let data = tempfile::tempdir().expect("data dir");
    let idx = boot(&config(data.path(), "")).await;
    let root = project_dir();
    let layer = toml::from_str(
        "[migration]\nsummary = \"Schema migrations.\"\n\
         instruction = \"Given a schema change, retrieve its migrations.\"\n",
    )
    .expect("a layer");
    let extra = RegisterProject {
        intents: Some(layer),
        ..RegisterProject::default()
    };
    let id = registered(&idx, root.path(), "demo", extra).await;

    let daemon = idx.intents(None).await.expect("daemon library");
    assert!(!daemon.iter().any(|i| i.name == "migration"));
    let project = idx.intents(Some(&id)).await.expect("project library");
    assert!(project.iter().any(|i| i.name == "migration"));
}

#[tokio::test]
async fn a_registration_with_a_broken_intent_layer_writes_nothing() {
    let data = tempfile::tempdir().expect("data dir");
    let idx = boot(&config(data.path(), "")).await;
    let root = project_dir();
    let layer = toml::from_str("[usage.hints]\ncobol = [\"x\"]\n").expect("parses");
    let outcome = idx
        .register(RegisterProject {
            root: root_of(root.path()),
            id: Some("demo".into()),
            intents: Some(layer),
            ..RegisterProject::default()
        })
        .await;
    assert_eq!(error_of(outcome).code(), "config");
    let id = ProjectId::new("demo").expect("id");
    assert_eq!(error_of(idx.intents(Some(&id)).await).code(), "not_found");
}
