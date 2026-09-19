//! # findery-index — layer L2
//!
//! The engine. It owns the catalog, the vector store, the embedder and the
//! chunkers, and it is the only place where those four meet. Everything above —
//! HTTP, MCP, CLI — is a thin translation of [`Indexer`]'s methods.
//!
//! INTENT: keeping orchestration in one layer is what lets the transports stay
//! dumb. A bug in "what sync does" has exactly one home.

pub mod jobs;
pub mod search;
pub mod sync;
pub mod walker;
pub mod watch;

use findery_core::clock::now_unix;
use findery_core::config::Config;
use findery_core::ids::ProjectId;
use findery_core::model::{
    IndexStats, Project, ProjectSpec, SearchHit, SearchRequest, SyncMode, SyncReport,
};
use findery_core::path_map::PathMapper;
use findery_core::traits::{Embedder, VectorStore};
use findery_core::{Error, Result};
use findery_store::{Catalog, ExportFormat, ExportReport};
use std::path::Path;
use std::sync::Arc;

pub use jobs::{Job, JobRegistry, JobState};

// `RegisterProject` and `HealthReport` are wire vocabulary: they live in
// L0 and are re-exported here so callers keep one import path.
pub use findery_core::api::{HealthReport, RegisterProject};

pub struct Indexer {
    pub cfg: Config,
    pub catalog: Catalog,
    pub store: Arc<dyn VectorStore>,
    pub embedder: Arc<dyn Embedder>,
    pub chunkers: findery_chunk::Registry,
    pub jobs: JobRegistry,
    mapper: PathMapper,
}

impl Indexer {
    /// Wires everything together: embedder first (it decides the vector width),
    /// then the catalog, then the store.
    pub async fn new(cfg: Config) -> Result<Arc<Self>> {
        cfg.validate()?;
        let embedder = findery_embed::build(&cfg.embedder).await?;
        let catalog = findery_store::open_catalog(&cfg.store)?;
        let store = findery_store::build_vector_store(&cfg.store, catalog.clone())?;

        let recipe = format!("{}:{}", embedder.model(), embedder.dim());
        if let Some(previous) = catalog.claim_recipe(&recipe).await? {
            return Err(Error::Config(format!(
                "this catalog was built with embedding recipe `{previous}` but the daemon now \
                 offers `{recipe}`; keep the old settings, point FINDERY_DATA_DIR at a fresh \
                 directory, or delete the catalog to re-index"
            )));
        }

        let jobs = JobRegistry::new(cfg.server.job_history, cfg.server.job_concurrency);
        let mapper = cfg.path_mapper();
        Ok(Arc::new(Self {
            cfg,
            catalog,
            store,
            embedder,
            chunkers: findery_chunk::Registry::new(),
            jobs,
            mapper,
        }))
    }

    /// The cache namespace: model plus width, as actually served.
    pub fn recipe(&self) -> String {
        format!("{}:{}", self.embedder.model(), self.embedder.dim())
    }

    pub fn mapper(&self) -> &PathMapper {
        &self.mapper
    }

    // -- projects -----------------------------------------------------------

    /// Registers a project, or updates it when the id already exists.
    pub async fn register(&self, req: RegisterProject) -> Result<Project> {
        if req.root.trim().is_empty() {
            return Err(Error::Invalid("root must not be empty".into()));
        }
        let container_root = self.mapper.to_container(&req.root);
        if !Path::new(&container_root).is_dir() {
            return Err(Error::Invalid(format!(
                "root `{}` resolves to `{container_root}`, which the daemon cannot see; \
                 check the mounts in FINDERY_MOUNTS",
                req.root
            )));
        }

        let defaults = &self.cfg.project_defaults;
        let name = req
            .name
            .clone()
            .or_else(|| basename(&container_root))
            .unwrap_or_else(|| "project".into());
        let id = match &req.id {
            Some(raw) => ProjectId::new(raw.clone())?,
            None => ProjectId::slugify(&name)?,
        };

        let spec = ProjectSpec {
            root: container_root,
            include: req.include.unwrap_or_default(),
            exclude: req.exclude.unwrap_or_else(|| defaults.exclude.clone()),
            extensions: req
                .extensions
                .unwrap_or_else(|| defaults.extensions.clone()),
            follow_gitignore: req.follow_gitignore.unwrap_or(defaults.follow_gitignore),
            include_hidden: req.include_hidden.unwrap_or(defaults.include_hidden),
            max_file_bytes: req.max_file_bytes.unwrap_or(defaults.max_file_bytes),
        };
        let existing = self.catalog.get_project(&id).await?;
        let project = Project {
            host_root: self.mapper.to_host(&spec.root),
            id,
            name,
            spec,
            created_at: existing.map(|p| p.created_at).unwrap_or_else(now_unix),
            updated_at: now_unix(),
        };
        self.catalog.put_project(&project).await?;
        self.store
            .ensure_project(&project.id, self.embedder.dim())
            .await?;
        Ok(project)
    }

    pub async fn list_projects(&self) -> Result<Vec<Project>> {
        self.catalog.list_projects().await
    }

    pub async fn get_project(&self, id: &ProjectId) -> Result<Project> {
        self.catalog.require_project(id).await
    }

    /// Removes a project, its chunks and its vectors.
    pub async fn forget_project(&self, id: &ProjectId) -> Result<bool> {
        // The vector store goes first: a catalog row with no vectors is
        // recoverable, an orphaned collection is not.
        self.store.drop_project(id).await?;
        self.catalog.delete_project(id).await
    }

    // -- indexing -----------------------------------------------------------

    /// Synchronizes a project against the filesystem.
    pub async fn sync(&self, id: &ProjectId, mode: SyncMode) -> Result<SyncReport> {
        let project = self.catalog.require_project(id).await?;
        sync::run(self, &project, mode).await
    }

    /// Adds or refreshes one document. With `text`, the content is taken from
    /// the request and the file never has to exist on disk.
    pub async fn add_document(
        &self,
        id: &ProjectId,
        rel_path: &str,
        text: Option<String>,
    ) -> Result<SyncReport> {
        let project = self.catalog.require_project(id).await?;
        let rel = sanitize_rel(rel_path)?;
        match text {
            Some(body) => sync::index_inline(self, &project, &rel, &body).await,
            None => sync::run(self, &project, SyncMode::Paths(vec![rel])).await,
        }
    }

    /// Drops one document from the index.
    pub async fn remove_document(&self, id: &ProjectId, rel_path: &str) -> Result<u64> {
        let project = self.catalog.require_project(id).await?;
        let rel = sanitize_rel(rel_path)?;
        sync::forget_file(self, &project, &rel).await
    }

    // -- querying -----------------------------------------------------------

    pub async fn search(&self, req: &SearchRequest) -> Result<Vec<SearchHit>> {
        search::search(self, req).await
    }

    pub async fn stats(&self, id: &ProjectId) -> Result<IndexStats> {
        let project = self.catalog.require_project(id).await?;
        let (files, chunks, last_sync_at) = self.catalog.counts(&project.id).await?;
        Ok(IndexStats {
            project: project.id.to_string(),
            files,
            chunks,
            vectors: self.store.count(&project.id).await.unwrap_or(0),
            dim: self.embedder.dim(),
            model: self.embedder.model().to_string(),
            store_backend: self.store.backend().to_string(),
            last_sync_at,
            disk_bytes: self.catalog.disk_bytes().await,
        })
    }

    /// Exports a project's vectors to `dir`, or to the configured export
    /// directory when `dir` is `None`.
    pub async fn export(
        &self,
        id: &ProjectId,
        format: ExportFormat,
        dir: Option<&str>,
    ) -> Result<ExportReport> {
        let project = self.catalog.require_project(id).await?;
        let target = match dir {
            Some(d) => self.mapper.to_container(d),
            None => self.cfg.store.export_dir.clone(),
        };
        findery_store::export_project(
            &self.catalog,
            &project.id,
            &self.recipe(),
            self.embedder.model(),
            format,
            Path::new(&target),
        )
        .await
    }

    /// Probes the embedding runtime and reports what the daemon is serving.
    pub async fn health(&self) -> Result<HealthReport> {
        let projects = self
            .catalog
            .list_projects()
            .await
            .map(|p| p.len())
            .unwrap_or(0);
        let (embedder_ok, detail) = match self.embedder.health().await {
            Ok(()) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };
        Ok(HealthReport {
            ok: embedder_ok,
            model: self.embedder.model().to_string(),
            dim: self.embedder.dim(),
            store_backend: self.store.backend().to_string(),
            projects,
            embedder_ok,
            detail,
        })
    }
}

/// Normalizes a caller-supplied relative path and refuses traversal.
///
/// INTENT: this string arrives from an MCP tool call. `../../etc/passwd` must
/// die here, before it becomes a `root.join(..)`.
fn sanitize_rel(raw: &str) -> Result<String> {
    let cleaned = raw.trim().replace('\\', "/");
    let cleaned = cleaned.trim_start_matches('/').to_string();
    if cleaned.is_empty() {
        return Err(Error::Invalid("path must not be empty".into()));
    }
    if cleaned.split('/').any(|seg| seg == "..") {
        return Err(Error::Invalid(format!(
            "path `{raw}` escapes the project root"
        )));
    }
    Ok(cleaned)
}

/// Last path segment of a `/`-separated path.
fn basename(path: &str) -> Option<String> {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_is_refused_and_separators_normalized() {
        assert_eq!(sanitize_rel("src\\lib.rs").expect("ok"), "src/lib.rs");
        assert_eq!(sanitize_rel("/src/lib.rs").expect("ok"), "src/lib.rs");
        assert!(sanitize_rel("../etc/passwd").is_err());
        assert!(sanitize_rel("src/../../x").is_err());
        assert!(sanitize_rel("   ").is_err());
    }

    #[test]
    fn a_dotted_filename_is_not_traversal() {
        assert_eq!(
            sanitize_rel("src/..hidden.rs").expect("ok"),
            "src/..hidden.rs"
        );
    }

    #[test]
    fn basename_survives_trailing_slashes() {
        assert_eq!(basename("/workspaces/axle/"), Some("axle".into()));
        assert_eq!(basename("/"), None);
    }
}
