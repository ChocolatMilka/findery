//! Search over a real project: what reaches the embedder, and in which order
//! the instruction ladder is read.

use findery_core::api::SearchResponse;
use findery_core::embedding::prompt::QueryTask;

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

async fn run(idx: &Indexer, req: &SearchRequest) -> SearchResponse {
    idx.search(req).await.expect("search")
}

#[tokio::test]
async fn a_search_answers_its_hits_and_no_warning() {
    let Fixture { idx, id, .. } = synced("").await;
    let response = run(&idx, &query(&id, "retry backoff", SearchMode::Hybrid)).await;
    assert!(!response.hits.is_empty());
    assert!(response.warnings.is_empty(), "{:?}", response.warnings);
}

#[tokio::test]
async fn a_task_puts_its_instruction_in_front_of_the_query() {
    let Fixture { idx, id, log, .. } = synced("").await;
    let req = query(&id, "retry backoff", SearchMode::Vector).with_task(QueryTask::Usage);
    run(&idx, &req).await;
    let expected = format!(
        "Instruct: {}\nQuery: retry backoff",
        QueryTask::Usage.instruction()
    );
    assert_eq!(logged(&log), [expected]);
}

#[tokio::test]
async fn a_free_instruction_beats_the_task() {
    let Fixture { idx, id, log, .. } = synced("").await;
    let mut req = query(&id, "retry backoff", SearchMode::Vector).with_task(QueryTask::Docs);
    req.instruction = Some("Given a migration question, retrieve the upgrade notes.".into());
    run(&idx, &req).await;
    let queries = logged(&log);
    assert_eq!(queries.len(), 1, "{queries:?}");
    assert!(
        queries[0].starts_with("Instruct: Given a migration question"),
        "{queries:?}"
    );
}

#[tokio::test]
async fn an_unknown_scope_is_refused_before_anything_is_embedded() {
    let Fixture { idx, id, log, .. } = synced("").await;
    let req = query(&id, "retry backoff", SearchMode::Hybrid).with_scope("nope");
    let err = error_of(idx.search(&req).await);
    assert_eq!(err.code(), "invalid");
    assert!(logged(&log).is_empty());
}
