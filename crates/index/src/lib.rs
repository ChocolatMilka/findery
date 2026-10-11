//! # findery-index — layer L2
//!
//! The engine. It owns the buckets, the vector stores, the embedder and the
//! chunkers, and it is the only place where those four meet. Everything above —
//! HTTP, MCP, CLI — is a thin translation of [`Indexer`]'s methods.
//!
//! INTENT: keeping orchestration in one layer is what lets the transports stay
//! dumb. A bug in "what sync does" has exactly one home.

pub mod buckets;
pub mod jobs;
pub mod progress;
pub mod projects;
pub mod search;
pub mod sync;
pub mod watch;

pub use findery_walk as walker;

pub use buckets::BucketHandle;
pub use progress::ProgressHandle;

use findery_core::config::Config;
use findery_core::ids::ProjectId;
use findery_core::model::{IndexStats, SearchHit, SearchRequest, SyncMode, SyncReport};
use findery_core::path_map::PathMapper;
use findery_core::traits::Embedder;
use findery_core::{Error, Result};
use findery_store::{ExportFormat, ExportReport};
use parking_lot::RwLock;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

pub use jobs::{Job, JobRegistry, JobState};

// `RegisterProject` and `HealthReport` are wire vocabulary: they live in
// L0 and are re-exported here so callers keep one import path.
pub use findery_core::api::{HealthReport, RegisterProject};

pub struct Indexer {
    pub cfg: Config,
    pub embedder: Arc<dyn Embedder>,
    pub chunkers: findery_chunk::Registry,
    pub jobs: JobRegistry,
    /// Every bucket the daemon has open, keyed by name. Rebuilt from the
    /// filesystem at boot, then maintained only by the daemon's own writes.
    buckets: RwLock<BTreeMap<String, Arc<BucketHandle>>>,
    /// Which bucket holds each registered project. A duplicate takes a
    /// hand-edited database, and [`Indexer::home_of`] takes the first in order.
    homes: RwLock<HashMap<ProjectId, String>>,
    /// Buckets on disk that could not be opened at boot, as reasons. Set once:
    /// the daemon keeps serving the others and says so in [`Indexer::health`].
    broken: Vec<String>,
    mapper: PathMapper,
}

impl Indexer {
    /// Wires everything together: the width the vectors already have, then the
    /// embedder, then every bucket on disk.
    pub async fn new(cfg: Config) -> Result<Arc<Self>> {
        cfg.validate()?;
        // Before anything opens a handle: an install that predates the current
        // file names is moved onto them, so every later read sees the same files.
        findery_store::migrate_legacy_layout(&cfg.store)?;
        // The width comes from the disk first: an index built earlier lets the
        // daemon start even while the embedding runtime is down, which is what
        // keeps `findery doctor` answerable.
        let known_dim = buckets::remembered_dim(&cfg.store, &cfg.embedder.model).await?;
        let embedder = findery_embed::build(&cfg.embedder, known_dim).await?;
        let recipe = format!("{}:{}", embedder.model(), embedder.dim());
        // A bucket built with another recipe is opened anyway: the conflict is
        // refused at the operation that would misuse it, never at boot.
        let opened = buckets::open_all(&cfg, &recipe, embedder.dim()).await?;
        for reason in &opened.broken {
            tracing::warn!("{reason}");
        }

        let jobs = JobRegistry::new(cfg.server.job_history, cfg.server.job_concurrency);
        let mapper = cfg.path_mapper();
        Ok(Arc::new(Self {
            cfg,
            embedder,
            chunkers: findery_chunk::Registry::new(),
            jobs,
            buckets: RwLock::new(opened.buckets),
            homes: RwLock::new(opened.homes),
            broken: opened.broken,
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

    // -- indexing -----------------------------------------------------------

    /// Synchronizes a project against the filesystem. `progress` is written as
    /// the pass advances; a caller with nobody watching passes
    /// [`ProgressHandle::detached`].
    pub async fn sync(
        &self,
        id: &ProjectId,
        mode: SyncMode,
        progress: &ProgressHandle,
    ) -> Result<SyncReport> {
        let cat = self.bucket_for(id)?;
        let project = cat.bucket.require_project(id).await?;
        sync::run(self, &cat, &project, mode, progress).await
    }

    /// Adds or refreshes one document. With `text`, the content is taken from
    /// the request and the file never has to exist on disk.
    pub async fn add_document(
        &self,
        id: &ProjectId,
        rel_path: &str,
        text: Option<String>,
    ) -> Result<SyncReport> {
        let cat = self.bucket_for(id)?;
        let project = cat.bucket.require_project(id).await?;
        let rel = sanitize_rel(rel_path)?;
        match text {
            Some(body) => sync::index_inline(self, &cat, &project, &rel, &body).await,
            None => {
                sync::run(
                    self,
                    &cat,
                    &project,
                    SyncMode::Paths(vec![rel]),
                    &ProgressHandle::detached(),
                )
                .await
            }
        }
    }

    /// Indexes a batch of documents pushed by a client.
    ///
    /// This is the entry point for a daemon that cannot see the project: the
    /// client walks, reads and sends, so no bind mount and no path translation
    /// are involved at all.
    pub async fn push_documents(
        &self,
        id: &ProjectId,
        documents: Vec<findery_core::api::AddDocument>,
    ) -> Result<SyncReport> {
        let cat = self.bucket_for(id)?;
        let project = cat.bucket.require_project(id).await?;
        let mut cleaned = Vec::with_capacity(documents.len());
        for doc in documents {
            let text = doc.text.ok_or_else(|| {
                Error::Invalid(format!("document `{}` was pushed without text", doc.path))
            })?;
            cleaned.push((sanitize_rel(&doc.path)?, text));
        }
        sync::push_documents(self, &cat, &project, cleaned).await
    }

    /// Removes pushed files the client no longer has, given everything it saw.
    pub async fn prune(&self, id: &ProjectId, keep: Vec<String>) -> Result<u64> {
        let cat = self.bucket_for(id)?;
        let project = cat.bucket.require_project(id).await?;
        let mut wanted = std::collections::HashSet::with_capacity(keep.len());
        for path in keep {
            wanted.insert(sanitize_rel(&path)?);
        }
        sync::prune(&cat, &project, &wanted).await
    }

    /// Everything the bucket knows about a project's files.
    ///
    /// A pushing client diffs against this to decide what is worth sending.
    pub async fn file_manifest(&self, id: &ProjectId) -> Result<Vec<findery_core::FileRecord>> {
        let cat = self.bucket_for(id)?;
        let project = cat.bucket.require_project(id).await?;
        let mut files: Vec<_> = cat
            .bucket
            .file_map(&project.id)
            .await?
            .into_values()
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    /// Drops one document from the index.
    pub async fn remove_document(&self, id: &ProjectId, rel_path: &str) -> Result<u64> {
        let cat = self.bucket_for(id)?;
        let project = cat.bucket.require_project(id).await?;
        let rel = sanitize_rel(rel_path)?;
        sync::forget_file(&cat, &project, &rel).await
    }

    // -- querying -----------------------------------------------------------

    pub async fn search(&self, req: &SearchRequest) -> Result<Vec<SearchHit>> {
        search::search(self, req).await
    }

    pub async fn stats(&self, id: &ProjectId) -> Result<IndexStats> {
        let cat = self.bucket_for(id)?;
        let project = cat.bucket.require_project(id).await?;
        let (files, chunks, last_sync_at) = cat.bucket.counts(&project.id).await?;
        Ok(IndexStats {
            project: project.id.to_string(),
            bucket: cat.name.clone(),
            files,
            chunks,
            vectors: cat.store.count(&project.id).await.unwrap_or(0),
            dim: self.embedder.dim(),
            model: self.embedder.model().to_string(),
            store_backend: cat.store.backend().to_string(),
            last_sync_at,
            disk_bytes: cat.bucket.disk_bytes().await,
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
        let cat = self.bucket_for(id)?;
        let project = cat.bucket.require_project(id).await?;
        let target = match dir {
            Some(d) => self.mapper.to_container(d),
            None => self.cfg.store.export_dir.clone(),
        };
        findery_store::export_project(
            &cat.bucket,
            &project.id,
            &cat.recipe,
            self.embedder.model(),
            format,
            Path::new(&target),
        )
        .await
    }

    /// Probes the embedding runtime and reports what the daemon is serving.
    pub async fn health(&self) -> Result<HealthReport> {
        let mut projects = 0usize;
        let mut backend = None;
        for handle in self.handles() {
            projects += handle.bucket.list_projects().await.map(|p| p.len())?;
            backend.get_or_insert_with(|| handle.store.backend().to_string());
        }
        let (embedder_ok, embedder_detail) = match self.embedder.health().await {
            Ok(()) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };
        // The daemon is up, so `ok` stays true — but a bucket it could not open
        // is a project gone from every listing, and belongs in the detail line.
        let detail = match (embedder_detail, self.broken.is_empty()) {
            (Some(d), false) => Some(format!("{d}; {}", self.broken.join("; "))),
            (Some(d), true) => Some(d),
            (None, false) => Some(self.broken.join("; ")),
            (None, true) => None,
        };
        Ok(HealthReport {
            ok: embedder_ok,
            model: self.embedder.model().to_string(),
            dim: self.embedder.dim(),
            store_backend: backend.unwrap_or_else(|| self.cfg.store.backend.as_str().to_string()),
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
}
