//! End-to-end API test over the real stack: router, indexer, chunkers, SQLite
//! bucket and vector store. Only the embedding runtime is swapped for the
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
    data_dir: tempfile::TempDir,
    cfg: Config,
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

    let state = findery_server::state_from_config(cfg.clone())
        .await
        .expect("state");
    Harness {
        app: findery_server::app(state),
        project_dir,
        data_dir,
        cfg,
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

    /// Project statistics.
    async fn stats(&self, id: &str) -> Value {
        let (status, body) = self
            .call("GET", &format!("/v1/projects/{id}/stats"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    /// Empties the vector store and restarts the daemon over the same bucket.
    ///
    /// That is what pointing the daemon at a fresh backend actually looks like:
    /// the bucket is intact, the store is empty, and the warm matrix is gone
    /// because the process is new.
    /// Where the bucket holding this project lives, as the daemon reports it.
    ///
    /// Asked rather than assumed: a project's bucket is inside the project by
    /// default, so only the daemon knows the path — and a test that guessed
    /// would be testing its own guess.
    async fn bucket_file(&self, id: &str) -> std::path::PathBuf {
        let (_, project) = self.call("GET", &format!("/v1/projects/{id}"), None).await;
        let name = project["bucket"].as_str().expect("bucket").to_string();
        let (_, buckets) = self.call("GET", "/v1/buckets", None).await;
        let entry = buckets
            .as_array()
            .expect("buckets")
            .iter()
            .find(|b| b["name"] == name.as_str())
            .expect("the project's bucket is listed");
        std::path::PathBuf::from(entry["path"].as_str().expect("path"))
    }

    async fn switch_to_a_fresh_vector_store(&mut self, id: &str) {
        let db = self.bucket_file(id).await;
        {
            let conn = rusqlite::Connection::open(&db).expect("open bucket");
            conn.execute("DELETE FROM vectors WHERE project_id = ?1", [id])
                .expect("clear vectors");
        }
        let state = findery_server::state_from_config(self.cfg.clone())
            .await
            .expect("restart");
        self.app = findery_server::app(state);
    }

    /// Rewrites the recipe a bucket records, then restarts the daemon over it.
    ///
    /// That is what a bucket built by another model looks like to this one: the
    /// vectors are the old model's, the daemon serves the new one, and only a
    /// fresh boot reads the changed recipe.
    async fn claim_a_foreign_recipe(&mut self, bucket: &str, recipe: &str) {
        let db = if bucket == findery_core::model::DEFAULT_BUCKET {
            self.data_dir.path().join("bucket.db")
        } else {
            self.data_dir
                .path()
                .join("buckets")
                .join(bucket)
                .join("bucket.db")
        };
        {
            let conn = rusqlite::Connection::open(&db).expect("open bucket");
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('embedding_recipe', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = ?1",
                [recipe],
            )
            .expect("rewrite recipe");
        }
        let state = findery_server::state_from_config(self.cfg.clone())
            .await
            .expect("restart");
        self.app = findery_server::app(state);
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
async fn deleting_a_project_removes_it_from_the_bucket() {
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
    // The PAYLOAD travels, not the rendered message: a client that rebuilds
    // the variant from `code` must render it exactly once.
    assert_eq!(body["message"], "project `ghost`", "{body}");

    let (status, body) = h.call("GET", "/v1/not-a-route", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["message"], "route", "{body}");

    let (status, body) = h
        .call("POST", "/v1/projects", Some(json!({ "root": "   " })))
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

/// The push path end to end: a project fed entirely over HTTP, never read from
/// disk by the daemon.
#[tokio::test]
async fn pushed_documents_are_indexed_searchable_and_prunable() {
    let h = harness().await;
    let id = h.register().await;

    let (status, report) = h
        .call(
            "POST",
            &format!("/v1/projects/{id}/documents/batch"),
            Some(json!({ "documents": [
                { "path": "pushed/alpha.rs", "text": "pub fn rotate_credentials() -> bool { true }" },
                { "path": "pushed/beta.md",  "text": "# Beta\n\nNotes about credential rotation.\n" }
            ]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["files_added"], 2, "{report}");
    assert!(
        report["chunks_written"].as_u64().expect("chunks") >= 2,
        "{report}"
    );

    // The manifest is what a pushing client diffs against.
    let (status, files) = h
        .call("GET", &format!("/v1/projects/{id}/files"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{files}");
    let files = files.as_array().expect("array");
    assert_eq!(files.len(), 2, "{files:?}");
    assert!(files.iter().all(|f| f["origin"] == "pushed"), "{files:?}");
    assert!(
        files
            .iter()
            .all(|f| f["content_hash"].as_str().is_some_and(|h| !h.is_empty()))
    );

    // Content that never touched the daemon's filesystem is searchable.
    let (_, hits) = h
        .call(
            "POST",
            "/v1/search",
            Some(
                json!({ "project": id, "query": "rotate_credentials", "mode": "keyword", "k": 5 }),
            ),
        )
        .await;
    let paths: Vec<&str> = hits
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|x| x["chunk"]["path"].as_str())
        .collect();
    assert!(paths.contains(&"pushed/alpha.rs"), "got {paths:?}");

    // Re-pushing identical content costs nothing.
    let (_, again) = h
        .call(
            "POST",
            &format!("/v1/projects/{id}/documents/batch"),
            Some(json!({ "documents": [
                { "path": "pushed/alpha.rs", "text": "pub fn rotate_credentials() -> bool { true }" }
            ]})),
        )
        .await;
    assert_eq!(again["files_unchanged"], 1, "{again}");
    assert_eq!(again["embeddings_computed"], 0, "{again}");

    // Pruning drops exactly what the client no longer has.
    let (status, pruned) = h
        .call(
            "POST",
            &format!("/v1/projects/{id}/prune"),
            Some(json!({ "keep": ["pushed/alpha.rs"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{pruned}");
    assert_eq!(pruned["removed"], 1, "{pruned}");

    let (_, files) = h
        .call("GET", &format!("/v1/projects/{id}/files"), None)
        .await;
    assert_eq!(files.as_array().expect("array").len(), 1);
}

/// Each side may only prune what it fed. A daemon-side sync sweeping away a
/// client's pushed files would be silent data loss.
#[tokio::test]
async fn a_daemon_sync_never_prunes_pushed_files() {
    let h = harness().await;
    let id = h.register().await;
    h.sync(&id, json!({})).await;

    h.call(
        "POST",
        &format!("/v1/projects/{id}/documents/batch"),
        Some(json!({ "documents": [
            { "path": "generated/notes.md", "text": "# Generated\n\nKept across syncs.\n" }
        ]})),
    )
    .await;

    let after = h.sync(&id, json!({ "mode": "full" })).await;
    assert_eq!(after["files_removed"], 0, "{after}");

    let (_, files) = h
        .call("GET", &format!("/v1/projects/{id}/files"), None)
        .await;
    let pushed: Vec<&str> = files
        .as_array()
        .expect("array")
        .iter()
        .filter(|f| f["origin"] == "pushed")
        .filter_map(|f| f["path"].as_str())
        .collect();
    assert_eq!(pushed, vec!["generated/notes.md"], "{files:?}");
}

/// A root the daemon cannot read is not an error: it is a project the client
/// will feed. Rejecting it was what forced every project under a bind mount.
#[tokio::test]
async fn an_unreachable_root_becomes_a_push_project() {
    let h = harness().await;

    let (status, body) = h
        .call(
            "POST",
            "/v1/projects",
            Some(json!({ "id": "remote", "root": "/nowhere/at/all" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["spec"]["push"], true, "{body}");
    // The host path is kept verbatim: there is no daemon-side path to map from.
    assert_eq!(body["host_root"], "/nowhere/at/all", "{body}");

    // A project the daemon can read stays in pull mode.
    let (_, local) = h
        .call("POST", "/v1/projects", Some(json!({ "root": h.root() })))
        .await;
    assert_eq!(local["spec"]["push"], false, "{local}");
}

/// Switching vector backend leaves the bucket full and the new store empty.
/// An incremental pass would find every file unchanged and write nothing,
/// leaving search silently broken.
#[tokio::test]
async fn an_empty_vector_store_is_rebuilt_instead_of_reported_clean() {
    let mut h = harness().await;
    let id = h.register().await;
    h.sync(&id, json!({})).await;

    let before = h.stats(&id).await;
    assert!(before["vectors"].as_u64().expect("vectors") > 0, "{before}");

    // Stand in for "the operator pointed the daemon at a fresh backend".
    h.switch_to_a_fresh_vector_store(&id).await;
    let empty = h.stats(&id).await;
    assert_eq!(empty["vectors"], 0, "{empty}");
    assert!(
        empty["chunks"].as_u64().expect("chunks") > 0,
        "the bucket survives: {empty}"
    );

    let report = h.sync(&id, json!({})).await;
    assert_eq!(
        report["mode"], "full",
        "an incremental request must upgrade itself: {report}"
    );
    assert!(
        report["chunks_written"].as_u64().expect("written") > 0,
        "{report}"
    );
    // Rebuilding costs no embedding: the vectors were cached all along.
    assert_eq!(report["embeddings_computed"], 0, "{report}");
    assert!(
        report["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .any(|w| w
                .as_str()
                .is_some_and(|t| t.contains("vector store was empty"))),
        "the rebuild must be visible, not silent: {report}"
    );

    let after = h.stats(&id).await;
    assert_eq!(after["vectors"], before["vectors"], "{after}");
}

/// A bucket whose vectors belong to another embedding space refuses the write
/// as well as the read, and stays listable, keyword-searchable and deletable.
///
/// The foreign recipe keeps this daemon's dimension on purpose: a width check
/// cannot see this conflict, so only the recipe comparison catches it.
#[tokio::test]
async fn a_bucket_built_by_another_model_refuses_a_sync_but_stays_repairable() {
    let mut h = harness().await;
    let (status, created) = h
        .call("POST", "/v1/buckets", Some(json!({ "name": "work" })))
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");

    let (status, project) = h
        .call(
            "POST",
            "/v1/projects",
            Some(json!({ "root": h.root(), "bucket": { "name": "work" } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{project}");
    assert_eq!(project["bucket"], "work", "{project}");
    let id = project["id"].as_str().expect("id").to_string();

    h.sync(&id, json!({})).await;
    assert!(h.stats(&id).await["vectors"].as_u64().expect("vectors") > 0);

    h.claim_a_foreign_recipe("work", "another-model:128").await;

    // The write is refused: two embedding spaces must not share a store. A
    // full pass is asked for on purpose — an incremental one would find nothing
    // to do and could refuse for the wrong reason.
    let (status, body) = h
        .call(
            "POST",
            &format!("/v1/projects/{id}/sync"),
            Some(json!({ "wait": true, "mode": "full" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let message = body["message"].as_str().expect("message");
    assert!(message.contains("another-model:128"), "{message}");
    assert!(message.contains("findery bucket rm work"), "{message}");

    // Everything that touches no vector keeps working — that is what makes the
    // conflict readable and the repair reachable.
    let (status, hits) = h
        .call(
            "POST",
            "/v1/search",
            Some(json!({ "project": id, "query": "retry_with_backoff", "mode": "keyword" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{hits}");
    assert!(
        !hits.as_array().expect("hits").is_empty(),
        "the keyword leg has no vector to compare: {hits}"
    );

    let (status, buckets) = h.call("GET", "/v1/buckets", None).await;
    assert_eq!(status, StatusCode::OK, "{buckets}");
    let work = buckets
        .as_array()
        .expect("buckets")
        .iter()
        .find(|c| c["name"] == "work")
        .expect("work listed");
    assert!(
        work["conflict"]
            .as_str()
            .is_some_and(|m| m.contains("another-model:128")),
        "a conflicted bucket is reported, not hidden: {work}"
    );

    let (status, removed) = h.call("DELETE", "/v1/buckets/work", None).await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert_eq!(removed["removed"], true, "{removed}");
    assert_eq!(removed["projects"], json!([id]), "{removed}");
}

/// A scope narrows the same index. It is a filter applied before scoring, not a
/// second index and not a cut applied after the ranking — which is what the
/// caller can tell apart by what comes back.
#[tokio::test]
async fn a_scope_narrows_the_same_index_without_a_second_one() {
    let h = harness().await;
    let (status, project) = h
        .call(
            "POST",
            "/v1/projects",
            Some(json!({
                "root": h.root(),
                "scopes": { "net": { "paths": ["src/net.rs"] } }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{project}");
    let id = project["id"].as_str().expect("id").to_string();
    h.sync(&id, json!({})).await;

    let (status, everywhere) = h
        .call(
            "POST",
            "/v1/search",
            Some(json!({ "project": id, "query": "retry", "mode": "keyword" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{everywhere}");
    let (_, before) = h.call("GET", "/v1/buckets", None).await;
    let everywhere_chunks = before
        .as_array()
        .expect("buckets")
        .iter()
        .find(|b| b["name"] == id.as_str())
        .expect("the project's bucket")
        .get("chunks")
        .cloned()
        .expect("chunks");

    let (status, narrowed) = h
        .call(
            "POST",
            "/v1/search",
            Some(json!({
                "project": id, "query": "retry", "mode": "keyword", "scope": "net"
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{narrowed}");
    let paths: Vec<&str> = narrowed
        .as_array()
        .expect("hits")
        .iter()
        .filter_map(|h| h["chunk"]["path"].as_str())
        .collect();
    assert!(!paths.is_empty(), "the scope must not empty the answer");
    assert!(
        paths.iter().all(|p| *p == "src/net.rs"),
        "a scope is a restriction, not a suggestion: {paths:?}"
    );

    // The index itself is untouched: the same project, in the same one bucket.
    // A scope that had built a second index would show up here.
    let (_, buckets) = h.call("GET", "/v1/buckets", None).await;
    let holding: Vec<&Value> = buckets
        .as_array()
        .expect("buckets")
        .iter()
        .filter(|b| b["projects"].as_u64().unwrap_or(0) > 0)
        .collect();
    assert_eq!(holding.len(), 1, "a scope must not add an index: {buckets}");
    assert_eq!(holding[0]["chunks"], everywhere_chunks, "{buckets}");
}

/// A scope nobody declared is refused with the list of the ones that exist —
/// the caller's next move is to pick one, and it cannot guess.
#[tokio::test]
async fn an_unknown_scope_names_the_ones_the_project_declares() {
    let h = harness().await;
    let (status, project) = h
        .call(
            "POST",
            "/v1/projects",
            Some(json!({
                "root": h.root(),
                "scopes": { "net": { "paths": ["src/net.rs"] } }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{project}");
    let id = project["id"].as_str().expect("id").to_string();

    let (status, body) = h
        .call(
            "POST",
            "/v1/search",
            Some(json!({ "project": id, "query": "retry", "scope": "docs" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["message"].as_str().is_some_and(|m| m.contains("net")),
        "{body}"
    );
}

/// One unopenable bucket must not take the daemon down with it. The others keep
/// serving, and the one that failed is named rather than swallowed: a bucket
/// that silently did not open is a project that silently disappeared from every
/// listing.
#[tokio::test]
async fn a_corrupt_bucket_does_not_stop_the_daemon_from_booting() {
    let h = harness().await;
    let bad = h.data_dir.path().join("buckets").join("bad");
    std::fs::create_dir_all(&bad).expect("mkdir");
    std::fs::write(bad.join("bucket.db"), b"this is not a database").expect("write");

    // A fresh process over the same directories — the boot that has to survive.
    let state = findery_server::state_from_config(h.cfg.clone())
        .await
        .expect("a broken bucket must not refuse the boot");
    let app = findery_server::app(state);

    let request = Request::builder()
        .method("GET")
        .uri("/v1/health")
        .body(Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(
        &response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes(),
    )
    .expect("json");

    assert_eq!(body["ok"], true, "the daemon is up: {body}");
    assert!(
        body["detail"].as_str().is_some_and(|d| d.contains("bad")),
        "the bucket that failed is named: {body}"
    );
}

/// One file is one bucket, whatever it is called. Two names over one SQLite file
/// would be two handles and two writers on it, and deleting one would unlink the
/// file the other is still serving.
#[tokio::test]
async fn a_second_name_over_one_file_is_refused() {
    let h = harness().await;
    let shared = h.data_dir.path().join("shared.db");

    let (status, first) = h
        .call(
            "POST",
            "/v1/projects",
            Some(json!({
                "root": h.root(), "id": "one",
                "bucket": { "name": "shared", "path": shared.display().to_string() }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");

    let (status, second) = h
        .call(
            "POST",
            "/v1/projects",
            Some(json!({
                "root": h.root(), "id": "two",
                "bucket": { "name": "another-name", "path": shared.display().to_string() }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{second}");
    assert!(
        second["message"]
            .as_str()
            .is_some_and(|m| m.contains("already open as `shared`")),
        "{second}"
    );
}

/// The ordinary case, and the one everything else is a departure from: a
/// project indexed into a bucket that lives inside it.
#[tokio::test]
async fn a_project_local_bucket_is_created_inside_the_project() {
    let h = harness().await;
    let id = h.register().await;

    let file = h.project_dir.path().join(".findery").join("bucket.db");
    assert!(file.is_file(), "the index lives with the code: {file:?}");

    let (_, buckets) = h.call("GET", "/v1/buckets", None).await;
    let mine = buckets
        .as_array()
        .expect("buckets")
        .iter()
        .find(|b| b["name"] == id.as_str())
        .unwrap_or_else(|| panic!("`{id}` is listed: {buckets}"));
    assert_eq!(mine["path"], file.display().to_string().replace('\\', "/"));
}

/// An arbitrary path is honoured as written — a bucket on a shared drive is the
/// reason the name and the location came apart.
#[tokio::test]
async fn a_bucket_at_an_explicit_path_is_opened_where_it_is() {
    let h = harness().await;
    let shared = h.data_dir.path().join("elsewhere").join("team.db");
    let (status, project) = h
        .call(
            "POST",
            "/v1/projects",
            Some(json!({
                "root": h.root(),
                "bucket": { "name": "team", "path": shared.display().to_string() }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{project}");
    assert_eq!(project["bucket"], "team", "{project}");
    assert!(shared.is_file(), "opened where it was asked for");
}

/// A root the daemon cannot write to makes a bucket inside it impossible. The
/// registration still succeeds — it is a fact about the deployment, not a
/// mistake — and the bucket it actually used is reported back.
#[tokio::test]
async fn a_root_the_daemon_cannot_reach_gets_a_bucket_it_can_write() {
    let h = harness().await;
    let (status, project) = h
        .call(
            "POST",
            "/v1/projects",
            Some(json!({ "id": "remote", "root": "/nowhere/at/all" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{project}");
    assert_eq!(project["spec"]["push"], true, "{project}");

    let name = project["bucket"].as_str().expect("bucket");
    let (_, buckets) = h.call("GET", "/v1/buckets", None).await;
    let mine = buckets
        .as_array()
        .expect("buckets")
        .iter()
        .find(|b| b["name"] == name)
        .unwrap_or_else(|| panic!("`{name}` is listed: {buckets}"));
    let path = mine["path"].as_str().expect("path").replace('\\', "/");
    let data_dir = h.data_dir.path().display().to_string().replace('\\', "/");
    assert!(
        path.starts_with(&data_dir),
        "the substituted bucket is one the daemon can write: {path}"
    );
}

/// The index must never index itself: a bucket inside the tree it describes
/// would be a file that every sync changes, which is a write loop.
#[tokio::test]
async fn the_bucket_inside_a_project_is_never_indexed() {
    let h = harness().await;
    let id = h.register().await;
    h.sync(&id, json!({})).await;

    let (status, files) = h
        .call("GET", &format!("/v1/projects/{id}/files"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{files}");
    let paths: Vec<&str> = files
        .as_array()
        .expect("files")
        .iter()
        .filter_map(|f| f["path"].as_str())
        .collect();
    assert!(
        !paths.iter().any(|p| p.contains(".findery")),
        "the index indexed itself: {paths:?}"
    );

    // And a second pass is a no-op rather than a growing corpus.
    let report = h.sync(&id, json!({})).await;
    assert_eq!(
        report["files_added"], 0,
        "a bucket in the tree must not add files every pass: {report}"
    );
}

/// A bucket that lives outside the data directory has to survive a restart:
/// nothing about its path can be derived from its name.
#[tokio::test]
async fn an_out_of_place_bucket_is_found_again_after_a_restart() {
    let mut h = harness().await;
    let shared = h.data_dir.path().join("elsewhere").join("team.db");
    let (status, project) = h
        .call(
            "POST",
            "/v1/projects",
            Some(json!({
                "root": h.root(),
                "id": "axle",
                "bucket": { "name": "team", "path": shared.display().to_string() }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{project}");

    // A fresh process over the same directories: the registry file is the only
    // thing that remembers where `team` lives.
    let state = findery_server::state_from_config(h.cfg.clone())
        .await
        .expect("restart");
    h.app = findery_server::app(state);

    let (status, buckets) = h.call("GET", "/v1/buckets", None).await;
    assert_eq!(status, StatusCode::OK, "{buckets}");
    assert!(
        buckets
            .as_array()
            .expect("buckets")
            .iter()
            .any(|b| b["name"] == "team" && b["projects"] == 1),
        "the out-of-place bucket came back with its project: {buckets}"
    );
}
