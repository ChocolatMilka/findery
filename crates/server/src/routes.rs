//! Route table and handlers. Each one does exactly three things: validate,
//! call one [`findery_index::Indexer`] method, serialize.

use crate::dto::{
    AddDocument, Deleted, DocumentQuery, ExportRequest, PruneRequest, PushDocuments, Removed,
    Stopping, SyncRequest, SyncResponse, WatchRequest, Watching,
};
use crate::{ApiResult, AppState};
use axum::extract::{Path, Query, State};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use findery_core::api::{
    BucketInfo, BucketRemoved, CreateBucket, HealthReport, Job, RegisterProject,
};
use findery_core::ids::ProjectId;
use findery_core::model::{FileRecord, IndexStats, Project, SearchHit, SearchRequest, SyncReport};
use findery_index::ProgressHandle;
use findery_store::{ExportFormat, ExportReport};
use std::time::Duration;

/// The full API surface.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/health", get(health))
        .route("/v1/projects", get(list_projects).post(register_project))
        .route("/v1/projects/{id}", get(get_project).delete(delete_project))
        .route("/v1/projects/{id}/stats", get(project_stats))
        .route("/v1/projects/{id}/sync", post(sync_project))
        .route("/v1/projects/{id}/export", post(export_project))
        .route("/v1/projects/{id}/watch", post(set_watch))
        .route(
            "/v1/projects/{id}/documents",
            post(add_document).delete(remove_document),
        )
        .route("/v1/projects/{id}/documents/batch", post(push_documents))
        .route("/v1/projects/{id}/files", get(file_manifest))
        .route("/v1/projects/{id}/prune", post(prune))
        .route("/v1/buckets", get(list_buckets).post(create_bucket))
        .route("/v1/buckets/{name}", delete(delete_bucket))
        .route("/v1/search", post(search))
        .route("/v1/shutdown", post(shutdown))
        .route("/v1/jobs", get(list_jobs))
        .route("/v1/jobs/{id}", get(get_job))
        // Kept last so a typo in a path yields 404 rather than a confusing 405.
        .fallback(not_found)
}

/// Liveness only. No auth, no dependency probe: a container health check that
/// fails because the model is warming up would restart a healthy daemon.
async fn healthz() -> &'static str {
    "ok"
}

/// Readiness: probes the embedding runtime and reports what is being served.
async fn health(State(state): State<AppState>) -> ApiResult<Json<HealthReport>> {
    Ok(Json(state.indexer.health().await?))
}

async fn list_projects(State(state): State<AppState>) -> ApiResult<Json<Vec<Project>>> {
    Ok(Json(state.indexer.list_projects().await?))
}

async fn register_project(
    State(state): State<AppState>,
    Json(body): Json<RegisterProject>,
) -> ApiResult<Json<Project>> {
    Ok(Json(state.indexer.register(body).await?))
}

async fn get_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Project>> {
    Ok(Json(state.indexer.get_project(&ProjectId::new(id)?).await?))
}

async fn delete_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Deleted>> {
    let id = ProjectId::new(id)?;
    state.watches.lock().remove(id.as_str());
    Ok(Json(Deleted {
        deleted: state.indexer.forget_project(&id).await?,
    }))
}

async fn project_stats(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<IndexStats>> {
    Ok(Json(state.indexer.stats(&ProjectId::new(id)?).await?))
}

/// Starts a sync. Returns a job unless `wait` was requested.
async fn sync_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SyncRequest>,
) -> ApiResult<Json<SyncResponse>> {
    let id = ProjectId::new(id)?;
    let mode = body.resolve()?;
    // Fail here rather than inside a background task nobody is watching.
    state.indexer.get_project(&id).await?;

    if body.wait {
        let _permit = state.indexer.jobs.acquire().await?;
        // A buffered response cannot show anything mid-flight, so a caller who
        // asked to wait gets the report and nothing else.
        let report = state
            .indexer
            .sync(&id, mode, &ProgressHandle::detached())
            .await?;
        return Ok(Json(SyncResponse::Completed { report }));
    }

    if let Some(active) = state.indexer.jobs.has_active(id.as_str()) {
        return Ok(Json(SyncResponse::Accepted { job: active }));
    }
    let job = state.indexer.jobs.create(id.as_str(), mode.as_str());
    let job_id = job.id.clone();
    // The counters the spawned task will write into are the ones the registry
    // hands back on `GET /v1/jobs/{id}` — that is the whole channel.
    let progress = state.indexer.jobs.handle(&job_id).unwrap_or_default();
    let indexer = state.indexer.clone();
    tokio::spawn(async move {
        let Ok(_permit) = indexer.jobs.acquire().await else {
            indexer.jobs.fail(&job_id, "job queue closed");
            return;
        };
        indexer.jobs.start(&job_id);
        match indexer.sync(&id, mode, &progress).await {
            Ok(report) => indexer.jobs.succeed(&job_id, report),
            Err(e) => indexer.jobs.fail(&job_id, &e.to_string()),
        }
    });
    Ok(Json(SyncResponse::Accepted { job }))
}

async fn add_document(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AddDocument>,
) -> ApiResult<Json<SyncReport>> {
    let id = ProjectId::new(id)?;
    Ok(Json(
        state
            .indexer
            .add_document(&id, &body.path, body.text)
            .await?,
    ))
}

async fn remove_document(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<DocumentQuery>,
) -> ApiResult<Json<Removed>> {
    let id = ProjectId::new(id)?;
    Ok(Json(Removed {
        removed: state.indexer.remove_document(&id, &query.path).await?,
    }))
}

/// Ingests documents a client walked and read itself.
async fn push_documents(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PushDocuments>,
) -> ApiResult<Json<SyncReport>> {
    let id = ProjectId::new(id)?;
    Ok(Json(
        state.indexer.push_documents(&id, body.documents).await?,
    ))
}

/// What the bucket knows about a project's files, so a pushing client can send
/// only what changed.
async fn file_manifest(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<FileRecord>>> {
    Ok(Json(
        state.indexer.file_manifest(&ProjectId::new(id)?).await?,
    ))
}

/// Removes pushed files absent from the list the client saw.
async fn prune(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PruneRequest>,
) -> ApiResult<Json<Removed>> {
    let id = ProjectId::new(id)?;
    Ok(Json(Removed {
        removed: state.indexer.prune(&id, body.keep).await?,
    }))
}

async fn search(
    State(state): State<AppState>,
    Json(body): Json<SearchRequest>,
) -> ApiResult<Json<Vec<SearchHit>>> {
    Ok(Json(state.indexer.search(&body).await?))
}

async fn export_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ExportRequest>,
) -> ApiResult<Json<ExportReport>> {
    let id = ProjectId::new(id)?;
    let format = match body.format.as_deref() {
        Some(raw) => ExportFormat::parse(raw)?,
        None => ExportFormat::default(),
    };
    Ok(Json(
        state
            .indexer
            .export(&id, format, body.dir.as_deref())
            .await?,
    ))
}

/// Turns filesystem watching on or off for one project.
async fn set_watch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<WatchRequest>,
) -> ApiResult<Json<Watching>> {
    let id = ProjectId::new(id)?;
    if !body.enabled {
        // Dropping the handle is what stops the watcher task.
        state.watches.lock().remove(id.as_str());
        return Ok(Json(Watching {
            project: id.to_string(),
            watching: false,
        }));
    }
    let project = state.indexer.get_project(&id).await?;
    let handle = findery_index::watch::spawn(
        state.indexer.clone(),
        project,
        Duration::from_millis(body.debounce_ms.clamp(100, 60_000)),
    )?;
    // Replacing an existing handle drops it, which stops the previous task.
    state.watches.lock().insert(id.to_string(), handle);
    Ok(Json(Watching {
        project: id.to_string(),
        watching: true,
    }))
}

/// Asks the daemon to stop, gracefully.
///
/// Requires the bearer token when one is configured, like everything else: a
/// daemon anyone on the network can stop is not a daemon.
async fn shutdown(State(state): State<AppState>) -> Json<Stopping> {
    tracing::info!("shutdown requested over the API");
    state.shutdown.notify_one();
    Json(Stopping { stopping: true })
}

async fn list_buckets(State(state): State<AppState>) -> ApiResult<Json<Vec<BucketInfo>>> {
    Ok(Json(state.indexer.list_buckets().await?))
}

async fn create_bucket(
    State(state): State<AppState>,
    Json(body): Json<CreateBucket>,
) -> ApiResult<Json<BucketInfo>> {
    Ok(Json(state.indexer.create_bucket(&body.name).await?))
}

/// Deletes a bucket, its projects and their vectors.
///
/// It goes through the daemon rather than the CLI's own filesystem call: the
/// daemon is the only process that knows every handle on the file, and a
/// bucket still open cannot be removed at all.
async fn delete_bucket(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<BucketRemoved>> {
    let removed = state.indexer.forget_bucket(&name).await?;
    // The watchers of the projects that went with it would otherwise keep
    // re-syncing an id that no longer resolves.
    {
        let mut watches = state.watches.lock();
        for project in &removed.projects {
            watches.remove(project.as_str());
        }
    }
    Ok(Json(removed))
}

async fn list_jobs(State(state): State<AppState>) -> Json<Vec<Job>> {
    Json(state.indexer.jobs.list())
}

async fn get_job(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Job>> {
    state
        .indexer
        .jobs
        .get(&id)
        .map(Json)
        .ok_or_else(|| findery_core::Error::NotFound(format!("job `{id}`")).into())
}

async fn not_found() -> crate::ApiError {
    crate::ApiError(findery_core::Error::NotFound("route".into()))
}
