//! `GET /v1/buckets` per file, and `DELETE /v1/buckets/{name}?model=` — the
//! listing and the removal of one model's file, over the real stack with the
//! mock embedder.
//!
//! Kept apart from `api.rs` because it needs one fixture that file has no use
//! for: a project indexed under two models, so its bucket holds two files.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use findery_core::config::{Config, EmbedderBackend};

struct Harness {
    app: Router,
    project: tempfile::TempDir,
    _data: tempfile::TempDir,
}

/// A daemon with two mock profiles of one width — `embedder` (`mock:16`) and
/// `other` (`other:16`) — and one project synced under both.
async fn indexed_under_two_models() -> (Harness, String) {
    let data = tempfile::tempdir().expect("data dir");
    let project = tempfile::tempdir().expect("project dir");
    std::fs::write(
        project.path().join("lib.rs"),
        "/// Retries with backoff.\npub fn retry_with_backoff() -> bool { true }\n",
    )
    .expect("write");

    let mut cfg = Config::default();
    cfg.embedder.backend = EmbedderBackend::Mock;
    cfg.embedder.dim = 16;
    cfg.store.data_dir = data.path().display().to_string();
    cfg.chunk.min_chars = 1;
    cfg.models.insert(
        "other".into(),
        toml::from_str("weights = \"other\"").expect("profile"),
    );
    let state = findery_server::state_from_config(cfg).await.expect("state");
    let h = Harness {
        app: findery_server::app(state),
        project,
        _data: data,
    };

    let root = h.project.path().display().to_string().replace('\\', "/");
    let (status, body) = h
        .call("POST", "/v1/projects", Some(json!({ "root": root })))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["id"].as_str().expect("id").to_string();
    h.sync(&id).await;
    let (status, body) = h
        .call(
            "POST",
            "/v1/models/default",
            Some(json!({ "alias": "other" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    h.sync(&id).await;
    (h, id)
}

impl Harness {
    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let builder = Request::builder().method(method).uri(uri);
        let request = match body {
            Some(v) => builder
                .header("content-type", "application/json")
                .body(Body::from(v.to_string())),
            None => builder.body(Body::empty()),
        }
        .expect("request");
        let response = self.app.clone().oneshot(request).await.expect("response");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
        (status, value)
    }

    async fn sync(&self, id: &str) {
        let (status, body) = self
            .call(
                "POST",
                &format!("/v1/projects/{id}/sync"),
                Some(json!({ "wait": true })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    /// The listing entry of the project's own bucket.
    async fn bucket(&self, name: &str) -> Value {
        let (status, list) = self.call("GET", "/v1/buckets", None).await;
        assert_eq!(status, StatusCode::OK, "{list}");
        list.as_array()
            .expect("list")
            .iter()
            .find(|b| b["name"] == name)
            .cloned()
            .expect("the project's bucket is listed")
    }
}

#[tokio::test]
async fn a_bucket_lists_one_entry_per_model_file_and_marks_the_serving_one() {
    let (h, id) = indexed_under_two_models().await;
    let bucket = h.bucket(&id).await;
    let members = bucket["members"].as_array().expect("members");
    assert_eq!(members.len(), 2, "{bucket}");
    let serving: Vec<&Value> = members.iter().filter(|m| m["serving"] == true).collect();
    assert_eq!(serving.len(), 1, "{bucket}");
    assert_eq!(serving[0]["recipe"], "other:16");
    for member in members {
        assert_eq!(member["projects"], 1, "{member}");
        assert!(member["chunks"].as_u64() > Some(0), "{member}");
        let path = member["path"].as_str().expect("path");
        assert!(std::path::Path::new(path).is_file(), "{path}");
        assert!(
            path.contains(member["tag"].as_str().expect("tag")),
            "{path}"
        );
    }
    assert_eq!(
        bucket["recipe"], "other:16",
        "family fields follow the default"
    );
}

#[tokio::test]
async fn removing_one_model_file_keeps_the_project_on_the_other() {
    let (h, id) = indexed_under_two_models().await;
    let before = h.bucket(&id).await;
    let mock_file = before["members"]
        .as_array()
        .expect("members")
        .iter()
        .find(|m| m["recipe"] == "mock:16")
        .and_then(|m| m["path"].as_str())
        .expect("the embedder's file")
        .to_string();

    let (status, body) = h
        .call("DELETE", &format!("/v1/buckets/{id}?model=embedder"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["removed"], true, "{body}");
    assert_eq!(body["projects_removed"], 0, "{body}");
    assert!(!std::path::Path::new(&mock_file).exists(), "{mock_file}");

    let after = h.bucket(&id).await;
    assert_eq!(after["members"].as_array().expect("members").len(), 1);
    let (status, project) = h.call("GET", &format!("/v1/projects/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "the project stays: {project}");
    let search = json!({ "project": id, "query": "retry_with_backoff", "mode": "vector" });
    let (status, hits) = h.call("POST", "/v1/search", Some(search)).await;
    assert_eq!(status, StatusCode::OK, "{hits}");
}

#[tokio::test]
async fn the_last_model_file_and_an_unknown_model_are_refused_with_nothing_changed() {
    let (h, id) = indexed_under_two_models().await;
    let (status, body) = h
        .call("DELETE", &format!("/v1/buckets/{id}?model=harier"), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "model_unknown", "{body}");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|m| m.contains("embedder, other")),
        "{body}"
    );

    let (status, _) = h
        .call("DELETE", &format!("/v1/buckets/{id}?model=embedder"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = h
        .call("DELETE", &format!("/v1/buckets/{id}?model=other"), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid", "{body}");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|m| m.contains(&format!("findery bucket rm {id}"))),
        "the refusal names the command that removes the bucket: {body}"
    );
    assert_eq!(
        h.bucket(&id).await["members"].as_array().map(Vec::len),
        Some(1)
    );

    // A misspelled parameter must not fall through to removing everything.
    let (status, _) = h
        .call("DELETE", &format!("/v1/buckets/{id}?modle=other"), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        h.bucket(&id).await["members"].as_array().map(Vec::len),
        Some(1)
    );
}
