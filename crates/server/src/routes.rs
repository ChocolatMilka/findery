//! Route table and handlers. Each one does exactly three things: validate,
//! call one [`findery_index::Indexer`] method, serialize.

use axum::extract::{Path, Query, State};
use axum::routing::{delete, get, post};
use axum::{Json, Router};

use findery_core::api::{
    BucketInfo, BucketRemoved, CreateBucket, FilesQuery, HealthReport, IntentInfo, Job, ModelInfo,
    ProjectDetached, ProjectRemoved, ReadQuery, ReadResponse, RegisterProject, SearchResponse,
    SetDefaultModel,
};
use findery_core::ids::ProjectId;
use findery_core::model::{FileRecord, IndexStats, Project, SearchRequest, SyncReport};

use findery_store::{ExportFormat, ExportReport};

use crate::dto::{
    AddDocument, BucketQuery, DocumentQuery, ExportRequest, IntentsQuery, PruneRequest,
    PushDocuments, Removed, Stopping, SyncRequest, SyncResponse, WatchRequest, Watching,
};
use crate::{ApiResult, AppState};

/// The full API surface.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/health", get(health))
        .route("/v1/projects", get(list_projects).post(register_project))
        .route("/v1/projects/{id}", get(get_project).delete(delete_project))
        .route("/v1/projects/{id}/stats", get(project_stats))
        .route("/v1/projects/{id}/sync", post(sync_project))
        .route("/v1/projects/{id}/detach", post(detach_project))
        .route("/v1/projects/{id}/export", post(export_project))
        .route("/v1/projects/{id}/watch", post(set_watch))
        .route(
            "/v1/projects/{id}/documents",
            post(add_document).delete(remove_document),
        )
        .route("/v1/projects/{id}/documents/batch", post(push_documents))
        .route("/v1/projects/{id}/files", get(list_files))
        .route("/v1/projects/{id}/chunks", get(read_chunk))
        .route("/v1/projects/{id}/prune", post(prune))
        .route("/v1/buckets", get(list_buckets).post(create_bucket))
        .route("/v1/buckets/{name}", delete(delete_bucket))
        .route("/v1/search", post(search))
        .route("/v1/intents", get(list_intents))
        .route("/v1/models", get(list_models))
        .route("/v1/models/default", post(set_default_model))
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

/// Removes a project and its index. The engine stops its watcher and its job
/// first, and says which files went.
async fn delete_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ProjectRemoved>> {
    let id = ProjectId::new(id)?;
    Ok(Json(state.indexer.forget_project(&id).await?))
}

/// Takes a project off the daemon and keeps its index on disk, released.
async fn detach_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ProjectDetached>> {
    let id = ProjectId::new(id)?;
    Ok(Json(state.indexer.detach(&id).await?))
}

async fn project_stats(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<IndexStats>> {
    Ok(Json(state.indexer.stats(&ProjectId::new(id)?).await?))
}

/// Starts a sync: a job, unless `wait` was requested. The engine spawns it, so
/// a `detach` can find it.
async fn sync_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SyncRequest>,
) -> ApiResult<Json<SyncResponse>> {
    let id = ProjectId::new(id)?;
    Ok(Json(state.indexer.start_sync(&id, &body).await?))
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

/// What the bucket knows about a project's files — so a pushing client can
/// send only what changed — or the ones `?glob=` names.
async fn list_files(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<FilesQuery>,
) -> ApiResult<Json<Vec<FileRecord>>> {
    let id = ProjectId::new(id)?;
    Ok(Json(state.indexer.files(&id, query.glob.as_deref()).await?))
}

/// One chunk by id, or the lines a locator names, with `?context=` around.
async fn read_chunk(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<ReadQuery>,
) -> ApiResult<Json<ReadResponse>> {
    let id = ProjectId::new(id)?;
    Ok(Json(state.indexer.read(&id, &query).await?))
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
) -> ApiResult<Json<SearchResponse>> {
    Ok(Json(state.indexer.search(&body).await?))
}

async fn list_intents(
    State(state): State<AppState>,
    Query(query): Query<IntentsQuery>,
) -> ApiResult<Json<Vec<IntentInfo>>> {
    let project = query.project.map(ProjectId::new).transpose()?;
    Ok(Json(state.indexer.intents(project.as_ref()).await?))
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
    Ok(Json(state.indexer.set_watch(&id, &body).await?))
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

/// Deletes a bucket, its projects and their vectors — or, with `?model=`, only
/// that model's file of it.
///
/// It goes through the daemon rather than the CLI's own filesystem call: the
/// daemon is the only process that knows every handle on the file, and a
/// bucket still open cannot be removed at all. The engine stops the watchers
/// and the jobs of the projects that go with it.
async fn delete_bucket(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<BucketQuery>,
) -> ApiResult<Json<BucketRemoved>> {
    let model = query.model.as_deref();
    Ok(Json(state.indexer.forget_bucket(&name, model).await?))
}

/// The model profiles, without probing any runtime.
async fn list_models(State(state): State<AppState>) -> Json<Vec<ModelInfo>> {
    Json(state.indexer.models())
}

/// Switches the model unpinned projects use, from the next operation on.
async fn set_default_model(
    State(state): State<AppState>,
    Json(body): Json<SetDefaultModel>,
) -> ApiResult<Json<ModelInfo>> {
    Ok(Json(state.indexer.set_default_model(&body.alias)?))
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
