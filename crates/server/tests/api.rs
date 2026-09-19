//! End-to-end API test over the real stack: router, indexer, chunkers, SQLite
//! catalog and vector store. Only the embedding runtime is swapped for the
//! deterministic mock, because a model is the one thing a test cannot host.
//!
//! What this pins down is the behaviour the README promises: register, sync,
//! search, re-sync for free, add, remove, export, and refuse what should be
//! refused.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use findery_core::config::{Config, EmbedderBackend};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::path::Path;
use tower::ServiceExt;

struct Harness {
    app: Router,
    project_dir: tempfile::TempDir,
    _data_dir: tempfile::TempDir,
}

async fn harness_with(token: Option<&str>) -> Harness {
    let data_dir = tempfile::tempdir().expect("data dir");
    let project_dir = tempfile::tempdir().expect("project dir");
    let root = project_dir.path();

    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    std::fs::write(
        root.join("src/net.rs"),
        "//! Networking helpers.\n\n\
         /// Retries an operation with exponential backoff.\n\
         pub fn retry_with_backoff(attempts: u32) -> bool {\n    attempts < 5\n}\n\n\
         /// Opens a pooled connection.\n\
         pub fn connect(url: &str) -> Option<()> {\n    let _ = url;\n    Some(())\n}\n",
    )
    .expect("write");
    std::fs::write(
        root.join("README.md"),
        "# Demo\n\nA fixture project.\n\n## Networking\n\nRetry policy lives in src/net.rs.\n",
    )
    .expect("write");
    std::fs::write(root.join("notes.txt"), "not indexed").expect("write");

    let mut cfg = Config::default();
    cfg.embedder.backend = EmbedderBackend::Mock;
    cfg.embedder.dim = 128;
    cfg.store.data_dir = data_dir.path().display().to_string();
    cfg.store.export_dir = data_dir.path().join("exports").display().to_string();
    cfg.server.api_token = token.map(str::to_string);
    cfg.chunk.min_chars = 1;

    let state = findery_server::state_from_config(cfg).await.expect("state");
    Harness {
        app: findery_server::app(state),
        project_dir,
        _data_dir: data_dir,
    }
}

async fn harness() -> Harness {
    harness_with(None).await
}

impl Harness {
    fn root(&self) -> String {
        self.project_dir
            .path()
            .display()
            .to_string()
            .replace('\\', "/")
    }

    fn file(&self, rel: &str) -> std::path::PathBuf {
        self.project_dir.path().join(rel)
    }

    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.call_with_auth(method, uri, body, None).await
    }

    async fn call_with_auth(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        token: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            builder = builder.header("authorization", format!("Bearer {t}"));
        }
        let request = match body {
            Some(v) => builder
                .header("content-type", "application/json")
                .body(Body::from(v.to_string()))
                .expect("request"),
            None => builder.body(Body::empty()).expect("request"),
        };
        let response = self.app.clone().oneshot(request).await.expect("response");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).to_string()))
        };
        (status, value)
    }

    /// Registers the fixture project and returns its id.
    async fn register(&self) -> String {
        let (status, body) = self
            .call("POST", "/v1/projects", Some(json!({ "root": self.root() })))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["id"].as_str().expect("id").to_string()
    }

    /// Runs a synchronous sync and returns its report.
    async fn sync(&self, id: &str, extra: Value) -> Value {
        let mut body = json!({ "wait": true });
        if let Some(map) = extra.as_object() {
            for (k, v) in map {
                body[k] = v.clone();
            }
        }
        let (status, report) = self
            .call("POST", &format!("/v1/projects/{id}/sync"), Some(body))
            .await;
        assert_eq!(status, StatusCode::OK, "{report}");
        report["report"].clone()
    }
}

#[tokio::test]
async fn healthz_is_open_and_cheap() {
    let h = harness().await;
    let (status, _) = h.call("GET", "/healthz", None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn register_sync_and_search_round_trip() {
    let h = harness().await;
    let id = h.register().await;

    let report = h.sync(&id, json!({})).await;
    assert_eq!(
        report["files_added"], 2,
        "only .rs and .md are indexed: {report}"
    );
    assert!(
        report["chunks_written"].as_u64().expect("chunks") >= 2,
        "{report}"
    );
    assert!(
        report["embeddings_computed"].as_u64().expect("computed") > 0,
        "{report}"
    );

    let (status, hits) = h
        .call(
            "POST",
            "/v1/search",
            Some(json!({ "project": id, "query": "retry backoff", "k": 5 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{hits}");
    let hits = hits.as_array().expect("array");
    assert!(!hits.is_empty(), "search returned nothing");

    let first = &hits[0];
    assert!(
        first["host_path"]
            .as_str()
            .expect("host_path")
            .contains(&h.root())
    );
    assert!(first["chunk"]["start_line"].as_u64().expect("line") >= 1);
    assert!(first["score"].as_f64().is_some());
}

#[tokio::test]
async fn a_second_sync_costs_no_embedding_at_all() {
    let h = harness().await;
    let id = h.register().await;
    h.sync(&id, json!({})).await;

    let again = h.sync(&id, json!({})).await;
    assert_eq!(again["files_unchanged"], 2, "{again}");
    assert_eq!(again["embeddings_computed"], 0, "{again}");
    assert_eq!(again["files_updated"], 0, "{again}");
}

#[tokio::test]
async fn a_full_sync_still_reuses_the_content_addressed_cache() {
    let h = harness().await;
    let id = h.register().await;
    h.sync(&id, json!({})).await;

    // `full` bypasses the metadata and hash gates, but not the vector cache.
    let full = h.sync(&id, json!({ "mode": "full" })).await;
    assert_eq!(full["embeddings_computed"], 0, "{full}");
    assert!(
        full["embeddings_cached"].as_u64().expect("cached") > 0,
        "{full}"
    );
}

#[tokio::test]
async fn edits_additions_and_deletions_are_all_picked_up() {
    let h = harness().await;
    let id = h.register().await;
    h.sync(&id, json!({})).await;

    std::fs::write(
        h.file("src/net.rs"),
        "pub fn retry_with_backoff() -> u8 {\n    7\n}\n",
    )
    .expect("edit");
    let edited = h.sync(&id, json!({})).await;
    assert_eq!(edited["files_updated"], 1, "{edited}");

    std::fs::write(h.file("src/extra.rs"), "pub const LIMIT: usize = 3;\n").expect("add");
    let added = h.sync(&id, json!({})).await;
    assert_eq!(added["files_added"], 1, "{added}");

    std::fs::remove_file(h.file("src/extra.rs")).expect("delete");
    let removed = h.sync(&id, json!({})).await;
    assert_eq!(removed["files_removed"], 1, "{removed}");
    assert!(
        removed["chunks_deleted"].as_u64().expect("deleted") > 0,
        "{removed}"
    );
}

#[tokio::test]
async fn inline_documents_are_indexed_and_survive_a_sync() {
    let h = harness().await;
    let id = h.register().await;
    h.sync(&id, json!({})).await;

    let (status, report) = h
        .call(
            "POST",
            &format!("/v1/projects/{id}/documents"),
            Some(json!({
                "path": "notes/decisions.md",
                "text": "# Decisions\n\nWe chose reciprocal rank fusion for retrieval.\n"
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert!(
        report["chunks_written"].as_u64().expect("chunks") > 0,
        "{report}"
    );

    // A normal sync must not sweep away a document that has no file on disk.
    let after = h.sync(&id, json!({})).await;
    assert_eq!(after["files_removed"], 0, "{after}");

    let (_, stats) = h
        .call("GET", &format!("/v1/projects/{id}/stats"), None)
        .await;
    assert_eq!(stats["files"], 3, "{stats}");

    let (status, removed) = h
        .call(
            "DELETE",
            &format!("/v1/projects/{id}/documents?path=notes/decisions.md"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert!(
        removed["removed"].as_u64().expect("removed") > 0,
        "{removed}"
    );
}

#[tokio::test]
async fn export_writes_a_reloadable_file() {
    let h = harness().await;
    let id = h.register().await;
    h.sync(&id, json!({})).await;

    let (status, report) = h
        .call(
            "POST",
            &format!("/v1/projects/{id}/export"),
            Some(json!({ "format": "binary" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["dim"], 128, "{report}");
    assert!(report["vectors"].as_u64().expect("vectors") > 0, "{report}");

    let dir = report["directory"].as_str().expect("directory");
    let matrix = Path::new(dir).join("vectors.f32");
    let bytes = std::fs::metadata(&matrix).expect("matrix written").len();
    assert_eq!(
        bytes % (128 * 4),
        0,
        "matrix must be a whole number of rows"
    );
}

#[tokio::test]
async fn asynchronous_sync_returns_a_job_that_completes() {
    let h = harness().await;
    let id = h.register().await;

    let (status, accepted) = h
        .call("POST", &format!("/v1/projects/{id}/sync"), Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    let job_id = accepted["job"]["id"].as_str().expect("job id").to_string();

    let mut state = String::new();
    for _ in 0..100 {
        let (_, job) = h.call("GET", &format!("/v1/jobs/{job_id}"), None).await;
        state = job["state"].as_str().unwrap_or_default().to_string();
        if state == "succeeded" || state == "failed" {
            assert_eq!(state, "succeeded", "{job}");
            assert!(
                job["report"]["files_added"].as_u64().expect("added") > 0,
                "{job}"
            );
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("job never finished, last state: {state}");
}

#[tokio::test]
async fn deleting_a_project_removes_it_from_the_catalog() {
    let h = harness().await;
    let id = h.register().await;
    h.sync(&id, json!({})).await;

    let (status, body) = h.call("DELETE", &format!("/v1/projects/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], true);

    let (status, _) = h.call("GET", &format!("/v1/projects/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn bad_requests_get_the_status_they_deserve() {
    let h = harness().await;

    let (status, body) = h.call("GET", "/v1/projects/ghost", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "not_found");

    let (status, body) = h
        .call(
            "POST",
            "/v1/projects",
            Some(json!({ "root": "/nowhere/at/all" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid");

    let id = h.register().await;
    let (status, body) = h
        .call(
            "POST",
            &format!("/v1/projects/{id}/sync"),
            Some(json!({ "mode": "turbo" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, _) = h.call("GET", "/v1/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_configured_token_is_enforced_everywhere_but_healthz() {
    let h = harness_with(Some("s3cret")).await;

    let (status, _) = h.call("GET", "/healthz", None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "health checks must not need a secret"
    );

    let (status, _) = h.call("GET", "/v1/projects", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = h
        .call_with_auth("GET", "/v1/projects", None, Some("wrong"))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, body) = h
        .call_with_auth("GET", "/v1/projects", None, Some("s3cret"))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn search_filters_narrow_the_result_set() {
    let h = harness().await;
    let id = h.register().await;
    h.sync(&id, json!({})).await;

    let (status, hits) = h
        .call(
            "POST",
            "/v1/search",
            Some(json!({
                "project": id,
                "query": "retry",
                "k": 10,
                "filter": { "langs": ["markdown"] }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{hits}");
    for hit in hits.as_array().expect("array") {
        assert_eq!(hit["chunk"]["lang"], "markdown", "{hit}");
    }

    let (status, body) = h
        .call(
            "POST",
            "/v1/search",
            Some(json!({ "project": id, "query": "x", "filter": { "paths": ["["] } })),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a broken glob must be refused: {body}"
    );
}

#[tokio::test]
async fn snippets_respect_the_caller_token_budget() {
    let h = harness().await;
    let id = h.register().await;
    h.sync(&id, json!({})).await;

    let (_, hits) = h
        .call(
            "POST",
            "/v1/search",
            Some(json!({ "project": id, "query": "retry", "k": 3, "max_snippet_chars": 40 })),
        )
        .await;
    for hit in hits.as_array().expect("array") {
        let text = hit["chunk"]["text"].as_str().expect("text");
        assert!(
            text.len() <= 40 + "\n… [truncated]".len(),
            "snippet not trimmed: {text:?}"
        );
    }
}

#[tokio::test]
async fn the_instruction_aware_knobs_reach_the_wire() {
    let h = harness().await;
    let id = h.register().await;
    h.sync(&id, json!({})).await;

    // A named preset.
    let (status, hits) = h
        .call(
            "POST",
            "/v1/search",
            Some(json!({ "project": id, "query": "retry_with_backoff", "task": "symbol", "k": 3 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{hits}");

    // A free-form instruction.
    let (status, hits) = h
        .call(
            "POST",
            "/v1/search",
            Some(json!({
                "project": id,
                "query": "retry",
                "instruction": "Given a behaviour, retrieve the tests that assert it.",
                "k": 3
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{hits}");

    // An unknown preset is refused rather than silently ignored.
    let (status, body) = h
        .call(
            "POST",
            "/v1/search",
            Some(json!({ "project": id, "query": "x", "task": "telepathy" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}
