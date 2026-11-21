//! # findery-index — layer L2
//!
//! The engine. It owns the buckets, the vector stores, the embedders and the
//! chunkers, and it is the only place where those four meet. Everything above —
//! HTTP, MCP, CLI — is a thin translation of [`Indexer`]'s methods, jobs and
//! watchers included: a transport validates, calls one method, serializes.
//!
//! INTENT: keeping orchestration in one layer is what lets the transports stay
//! dumb. A bug in "what sync does" has exactly one home — and so does "what
//! `detach` must wait for", which is why the jobs and the watchers live here,
//! where a drain can reach them.

pub mod gates;
pub mod jobs;
mod lifecycle;
pub mod models;
pub mod projects;
pub mod search;
pub mod storage;
pub mod sync;

pub use findery_walk as walker;

pub use gates::{Access, Lease};
pub use jobs::progress::ProgressHandle;
pub use jobs::{Job, JobRegistry, JobState};
pub use models::EmbedderPool;
pub use storage::families::{BucketFamily, BucketHandle};

// Wire vocabulary lives in L0 and is re-exported here so callers keep one
// import path.
pub use findery_core::api::{HealthReport, RegisterProject};

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

use findery_core::config::Config;
use findery_core::ids::ProjectId;
use findery_core::model::{SearchRequest, SyncMode, SyncReport};
use findery_core::support::path_map::PathMapper;
use findery_core::{Error, Result};

use findery_store::files::registry::Registry;

use gates::ProjectGate;
use jobs::watch::WatchHandle;

/// The engine: every operation a transport exposes is one of its methods.
pub struct Indexer {
    pub cfg: Config,
    pub chunkers: findery_chunk::Registry,
    pub jobs: JobRegistry,
    /// The intent library, layered per project.
    intent_cache: search::intents::IntentCache,
    /// One embedder per model profile, and which profile is the default.
    pool: Arc<EmbedderPool>,
    /// Every bucket family the daemon has open, by name. Rebuilt from the disk
    /// at boot, then maintained only by the daemon's own writes.
    families: RwLock<BTreeMap<String, Arc<BucketFamily>>>,
    /// Which family each ATTACHED project lives in. A detached project has no
    /// home: that is what keeps it out of every listing and every lease.
    homes: RwLock<HashMap<ProjectId, String>>,
    /// The door of each project every operation passes, and a drain closes.
    gates: Mutex<HashMap<ProjectId, Arc<ProjectGate>>>,
    /// Live watchers, one per project. Dropping a handle stops its task.
    watches: Mutex<HashMap<ProjectId, WatchHandle>>,
    /// Buckets on disk that could not be opened, at boot or when attached
    /// again, as reasons. The daemon keeps serving the others and says so in
    /// [`Indexer::health`].
    broken: Mutex<Vec<String>>,
    /// What boot did on its own that a user should hear about.
    notices: Vec<String>,
    mapper: PathMapper,
}

impl Indexer {
    /// Boots the engine over the data directory `cfg` names.
    ///
    /// The order is the point, each step a precondition of the next:
    ///
    /// 1. the vocabulary migration (`catalog.db` → `bucket.db`);
    /// 2. `buckets.toml`;
    /// 3. the removals a previous run left unfinished — before anything opens a
    ///    file a removal still has to delete;
    /// 4. legacy adoption (`bucket.db` → its recipe's member), on the blocking
    ///    pool, before anything opens a handle that would hold the file;
    /// 5. a scan of every family, peeking at the recipe each file records;
    /// 6. the embedder pool — the default profile built now, at the width its
    ///    buckets remember when its runtime is down;
    /// 7. the families opened, skipping the ones whose projects are all
    ///    detached, and each project's home picked.
    pub async fn new(cfg: Config) -> Result<Arc<Self>> {
        Self::with_builder(cfg, models::default_builder()).await
    }

    /// [`Indexer::new`] with the embedder builder swapped, for tests.
    pub(crate) async fn with_builder(cfg: Config, build: models::BuildFn) -> Result<Arc<Self>> {
        cfg.validate()?;
        let intent_cache = search::intents::IntentCache::new(&cfg)?;
        findery_store::migrate_legacy_layout(&cfg.store)?;
        let (registry, mut broken) = Registry::load(&cfg.store);
        let (unfinished, mut notices) = lifecycle::tombstones::complete(&cfg, &registry).await;
        // Finishing a removal rewrote the file.
        let (registry, _) = Registry::load(&cfg.store);
        notices.extend(adopt(&cfg.store, &registry).await?);

        let scans =
            storage::boot::scan_all(&cfg.store, &registry, &unfinished.families, &mut broken).await;
        let remembered = storage::boot::remembered_dims(&scans);
        let pool = Arc::new(EmbedderPool::boot(&cfg, &remembered, build).await?);
        let default = pool.default_alias();
        tracing::info!(profile = %default, recipe = ?pool.recipe_of(&default).ok().map(|r| r.to_string()), "default model");

        let opened = storage::boot::open_all(&cfg.store, &pool, &registry, scans).await;
        broken.extend(opened.broken);
        notices.extend(opened.notices);
        for reason in &broken {
            tracing::warn!("{reason}");
        }
        Ok(Arc::new(Self {
            chunkers: findery_chunk::Registry::new(),
            jobs: JobRegistry::new(cfg.server.job_history, cfg.server.job_concurrency),
            intent_cache,
            pool,
            families: RwLock::new(opened.families),
            homes: RwLock::new(opened.homes),
            gates: Mutex::new(HashMap::new()),
            watches: Mutex::new(HashMap::new()),
            broken: Mutex::new(broken),
            notices,
            mapper: cfg.path_mapper(),
            cfg,
        }))
    }

    /// The embedder pool: profiles, the default one, and what each serves.
    pub fn pool(&self) -> &EmbedderPool {
        &self.pool
    }

    pub fn mapper(&self) -> &PathMapper {
        &self.mapper
    }

    // -- indexing -----------------------------------------------------------

    /// Synchronizes a project against the filesystem, under its model. The
    /// first write under a model creates that model's member of the bucket.
    /// `progress` is written as the pass advances; a caller with nobody
    /// watching passes [`ProgressHandle::detached`].
    ///
    /// Two passes over one project never write side by side: the second waits
    /// for the first's turn to end, without holding the project meanwhile.
    pub async fn sync(
        &self,
        id: &ProjectId,
        mode: SyncMode,
        progress: &ProgressHandle,
    ) -> Result<SyncReport> {
        let _turn = self.gate(id).take_turn().await;
        let lease = self.lease(id, Access::Write).await?;
        sync::run(self, &lease, mode, progress).await
    }

    // -- querying -----------------------------------------------------------

    /// A search: every leg of the plan the request resolves to — one for a
    /// plain query, several for a workflow intent — run concurrently. In hybrid
    /// mode a vector list that cannot be had degrades to keyword results and a
    /// warning; see [`search`].
    pub async fn search(&self, req: &SearchRequest) -> Result<findery_core::api::SearchResponse> {
        search::search(self, req).await
    }

    /// Releases every file: stops the watchers and closes every member. The
    /// daemon calls it on its way out, so the journals are folded back and the
    /// OS lock goes with the process rather than after it.
    pub async fn close(&self) -> Result<()> {
        self.watches.lock().clear();
        let families: Vec<_> = std::mem::take(&mut *self.families.write())
            .into_values()
            .collect();
        storage::families::every(families, |family| async move { family.close().await }).await?;
        Ok(())
    }
}

/// Adopts every legacy base file onto its recipe's member, on the blocking
/// pool, and returns what a user should hear about it.
async fn adopt(
    cfg: &findery_core::config::StoreConfig,
    registry: &Registry,
) -> Result<Vec<String>> {
    // Owned copies: the blocking task outlives neither, but must own them.
    let (store, snapshot) = (cfg.clone(), registry.clone());
    let adopted =
        storage::blocking::blocking(move || Ok(findery_store::adopt_all(&store, &snapshot)))
            .await?;
    let notices = adopted
        .iter()
        .map(|notice| notice.describe())
        .collect::<Vec<_>>();
    for notice in &notices {
        tracing::info!("{notice}");
    }
    Ok(notices)
}

/// Normalizes a caller-supplied relative path and refuses traversal.
///
/// INTENT: this string arrives from an MCP tool call. `../../etc/passwd` must
/// die here, before it becomes a `root.join(..)`.
pub(crate) fn sanitize_rel(raw: &str) -> Result<String> {
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
mod tests;
