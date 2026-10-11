//! The bucket registry: which buckets the daemon has open, which one each
//! project calls home, and what each can serve.
//!
//! INTENT: a bucket is a file, so the daemon's map of them is rebuilt from the
//! filesystem at boot and then maintained only by the daemon's own writes. One
//! writer means the map cannot drift from the disk, and nothing reads a
//! project's bucket out of a column — a project's home IS the bucket holding
//! its row.
//!
//! Entry/exit: [`Indexer::new`] fills the registry; every later mutation goes
//! through [`Indexer::register`] and [`Indexer::forget_bucket`]. A bucket that
//! disagrees with the daemon about its embedding recipe is opened anyway and
//! reported through [`BucketHandle::recipe_conflict`] — see there for why.

use crate::Indexer;
use findery_core::api::{BucketInfo, BucketRemoved};
use findery_core::config::{Config, StoreConfig};
use findery_core::ids::ProjectId;
use findery_core::model::{DEFAULT_BUCKET, is_bucket_name};
use findery_core::traits::VectorStore;
use findery_core::{Error, Result};
use findery_store::Bucket;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// One bucket the daemon has open.
pub struct BucketHandle {
    /// The name it answers to, and the directory it lives in.
    pub name: String,
    pub bucket: Bucket,
    pub store: Arc<dyn VectorStore>,
    /// The recipe the vectors already in this bucket were built with. A
    /// bucket with no vectors has this stamped to what the daemon serves.
    pub stored_recipe: String,
    /// The recipe this daemon serves.
    pub recipe: String,
}

impl BucketHandle {
    /// Opens one bucket, creating it if it does not exist, and claims `recipe`
    /// for it.
    pub async fn open(cfg: &Config, name: &str, recipe: &str, dim: usize) -> Result<Self> {
        if !is_bucket_name(name) {
            return Err(Error::Config(format!("`{name}` is not a bucket name")));
        }
        let bucket = findery_store::open_bucket(&cfg.store, name)?;
        // `claim_recipe` stamps a bucket that has none and reports the one it
        // had when they differ, so a `None` means the stored recipe is ours.
        let conflict = bucket.claim_recipe(recipe).await?;
        let stored_recipe = conflict.clone().unwrap_or_else(|| recipe.to_string());
        let store = findery_store::build_vector_store(&cfg.store, bucket.clone())?;
        let _ = dim;
        Ok(Self {
            name: name.to_string(),
            bucket,
            store,
            stored_recipe,
            recipe: recipe.to_string(),
        })
    }

    /// True when this bucket's vectors belong to another embedding space.
    pub fn conflicted(&self) -> bool {
        self.stored_recipe != self.recipe
    }

    /// The refusal a vector-touching operation gets, or `None` when this bucket
    /// and the daemon agree.
    ///
    /// INTENT: the conflict is refused at the operation, never at boot. The
    /// daemon that owns the bucket stays up to report it and to delete it — a
    /// conflict you can read about and repair, not one that locks the door you
    /// would repair it through. Everything that touches no vector (keyword
    /// search, project listing, deletion) keeps working meanwhile.
    pub fn recipe_conflict(&self) -> Option<String> {
        self.conflicted().then(|| {
            format!(
                "bucket `{}` holds vectors built with `{}`, but this daemon serves `{}`; \
                 re-index it — `findery bucket rm {}` then `findery init --bucket {}` — or \
                 start the daemon with the embedder settings it was built with",
                self.name, self.stored_recipe, self.recipe, self.name, self.name
            )
        })
    }

    /// Refuses when this bucket cannot accept or serve vectors.
    pub fn ensure_vectors_usable(&self) -> Result<()> {
        match self.recipe_conflict() {
            Some(message) => Err(Error::Config(message)),
            None => Ok(()),
        }
    }

    /// Everything `GET /v1/buckets` reports about it.
    pub async fn info(&self, is_default: bool) -> Result<BucketInfo> {
        let (projects, chunks) = self.bucket.totals().await?;
        Ok(BucketInfo {
            name: self.name.clone(),
            path: self.bucket.path().display().to_string(),
            recipe: Some(self.stored_recipe.clone()),
            conflict: self.recipe_conflict(),
            projects,
            chunks,
            disk_bytes: self.bucket.disk_bytes().await,
            is_default,
        })
    }
}

/// The width a previously built index remembers, when any bucket has one for
/// the model this daemon is configured to serve.
///
/// INTENT: read before the embedder is built, so a daemon whose runtime is down
/// starts at the width its vectors were written at instead of refusing to boot.
pub async fn remembered_dim(cfg: &StoreConfig, model: &str) -> Result<Option<usize>> {
    for name in findery_store::list_buckets(cfg)? {
        let Some(recipe) = findery_store::stored_recipe(cfg, &name).await? else {
            continue;
        };
        let Some((stored_model, dim)) = recipe.rsplit_once(':') else {
            continue;
        };
        if stored_model == model
            && let Ok(width) = dim.parse::<usize>()
        {
            return Ok(Some(width));
        }
    }
    Ok(None)
}

/// Opens every bucket on disk and rebuilds the project → bucket map.
///
/// A project id registered in two buckets resolves to the first in order
/// (default, then names alphabetically). The daemon's own writes cannot create
/// that state — registering into a second bucket moves the project — so it
/// takes a hand-edited database to reach, and the rule is stated rather than
/// discovered.
pub(crate) async fn open_all(
    cfg: &Config,
    recipe: &str,
    dim: usize,
) -> Result<(
    BTreeMap<String, Arc<BucketHandle>>,
    HashMap<ProjectId, String>,
)> {
    let mut buckets = BTreeMap::new();
    let mut homes = HashMap::new();
    let mut names = findery_store::list_buckets(&cfg.store)?;
    // The default bucket always exists, so a fresh install has somewhere to put
    // its first project without a create step.
    if !names.iter().any(|n| n == DEFAULT_BUCKET) {
        names.insert(0, DEFAULT_BUCKET.to_string());
    }
    for name in names {
        let handle = Arc::new(BucketHandle::open(cfg, &name, recipe, dim).await?);
        for project in handle.bucket.list_projects().await? {
            homes.insert(project.id, name.clone());
        }
        buckets.insert(name, handle);
    }
    Ok((buckets, homes))
}

impl Indexer {
    /// Every open bucket, default first then names in order.
    pub fn handles(&self) -> Vec<Arc<BucketHandle>> {
        self.buckets.read().values().cloned().collect()
    }

    /// One open bucket by name.
    pub fn handle(&self, name: &str) -> Result<Arc<BucketHandle>> {
        self.buckets
            .read()
            .get(name)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("bucket `{name}`")))
    }

    /// The bucket a project lives in.
    pub fn home_of(&self, id: &ProjectId) -> Result<String> {
        self.homes
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("project `{id}`")))
    }

    /// The open bucket a project lives in, ready to serve.
    pub fn bucket_for(&self, id: &ProjectId) -> Result<Arc<BucketHandle>> {
        self.handle(&self.home_of(id)?)
    }

    /// Opens a bucket, creating it when it does not exist.
    ///
    /// Creating here rather than behind a separate command is what lets
    /// `findery init --bucket work` be the whole gesture: naming a bucket you
    /// have never used is a decision, not an error.
    pub async fn ensure_bucket(&self, name: &str) -> Result<Arc<BucketHandle>> {
        if !is_bucket_name(name) {
            return Err(Error::Config(format!("`{name}` is not a bucket name")));
        }
        // Bound before the `if`: the guard must not be alive across the `await`
        // below, which is what keeps this future `Send`.
        let existing = self.buckets.read().get(name).cloned();
        if let Some(existing) = existing {
            return Ok(existing);
        }
        let handle = Arc::new(
            BucketHandle::open(&self.cfg, name, &self.recipe(), self.embedder.dim()).await?,
        );
        self.buckets
            .write()
            .insert(name.to_string(), handle.clone());
        Ok(handle)
    }

    /// Remembers which bucket holds a project.
    pub(crate) fn remember_home(&self, id: &ProjectId, bucket: &str) {
        self.homes.write().insert(id.clone(), bucket.to_string());
    }

    /// Forgets a project's home.
    pub(crate) fn forget_home(&self, id: &ProjectId) {
        self.homes.write().remove(id);
    }

    /// What the daemon serves, in the shape the API reports it.
    pub async fn list_buckets(&self) -> Result<Vec<BucketInfo>> {
        let mut out = Vec::new();
        for handle in self.handles() {
            out.push(
                handle
                    .info(handle.name == self.cfg.store.default_bucket)
                    .await?,
            );
        }
        Ok(out)
    }

    /// Creates a bucket, or reports the one that is already there.
    pub async fn create_bucket(&self, name: &str) -> Result<BucketInfo> {
        let handle = self.ensure_bucket(name).await?;
        handle.info(name == self.cfg.store.default_bucket).await
    }

    /// Deletes a bucket, its projects and their vectors.
    ///
    /// INTENT: it goes through the daemon rather than through the CLI's own
    /// filesystem call, because the daemon is the only process that knows every
    /// handle on the file — and on Windows a bucket still open cannot be
    /// removed at all. Nothing here needs the embedder: this is the door the
    /// recipe conflict used to lock.
    pub async fn forget_bucket(&self, name: &str) -> Result<BucketRemoved> {
        let handle = self.handle(name)?;
        let projects = handle.bucket.list_projects().await?;
        for project in &projects {
            // Vectors first: a bucket row with no vectors is recoverable, an
            // orphaned collection is not.
            handle.store.drop_project(&project.id).await?;
            self.forget_home(&project.id);
        }
        // Dropped before the file is removed: the map is the last handle the
        // daemon holds, and the request's own `Arc` is dropped with `handle`.
        self.buckets.write().remove(name);
        drop(handle);
        let removed = findery_store::delete_bucket(&self.cfg.store, name)?;
        Ok(BucketRemoved {
            removed,
            projects_removed: projects.len() as u64,
            projects: projects.iter().map(|p| p.id.to_string()).collect(),
        })
    }
}
