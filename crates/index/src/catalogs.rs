//! The catalog registry: which catalogs the daemon has open, which one each
//! project calls home, and what each can serve.
//!
//! INTENT: a catalog is a file, so the daemon's map of them is rebuilt from the
//! filesystem at boot and then maintained only by the daemon's own writes. One
//! writer means the map cannot drift from the disk, and nothing reads a
//! project's catalog out of a column — a project's home IS the catalog holding
//! its row.
//!
//! Entry/exit: [`Indexer::new`] fills the registry; every later mutation goes
//! through [`Indexer::register`] and [`Indexer::forget_catalog`]. A catalog that
//! disagrees with the daemon about its embedding recipe is opened anyway and
//! reported through [`CatalogHandle::recipe_conflict`] — see there for why.

use crate::Indexer;
use findery_core::api::{CatalogInfo, CatalogRemoved};
use findery_core::config::{Config, StoreConfig};
use findery_core::ids::ProjectId;
use findery_core::model::{DEFAULT_CATALOG, is_catalog_name};
use findery_core::traits::VectorStore;
use findery_core::{Error, Result};
use findery_store::Catalog;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// One catalog the daemon has open.
pub struct CatalogHandle {
    /// The name it answers to, and the directory it lives in.
    pub name: String,
    pub catalog: Catalog,
    pub store: Arc<dyn VectorStore>,
    /// The recipe the vectors already in this catalog were built with. A
    /// catalog with no vectors has this stamped to what the daemon serves.
    pub stored_recipe: String,
    /// The recipe this daemon serves.
    pub recipe: String,
}

impl CatalogHandle {
    /// Opens one catalog, creating it if it does not exist, and claims `recipe`
    /// for it.
    pub async fn open(cfg: &Config, name: &str, recipe: &str, dim: usize) -> Result<Self> {
        if !is_catalog_name(name) {
            return Err(Error::Config(format!("`{name}` is not a catalog name")));
        }
        let catalog = findery_store::open_catalog(&cfg.store, name)?;
        // `claim_recipe` stamps a catalog that has none and reports the one it
        // had when they differ, so a `None` means the stored recipe is ours.
        let conflict = catalog.claim_recipe(recipe).await?;
        let stored_recipe = conflict.clone().unwrap_or_else(|| recipe.to_string());
        let store = findery_store::build_vector_store(&cfg.store, catalog.clone())?;
        let _ = dim;
        Ok(Self {
            name: name.to_string(),
            catalog,
            store,
            stored_recipe,
            recipe: recipe.to_string(),
        })
    }

    /// True when this catalog's vectors belong to another embedding space.
    pub fn conflicted(&self) -> bool {
        self.stored_recipe != self.recipe
    }

    /// The refusal a vector-touching operation gets, or `None` when this catalog
    /// and the daemon agree.
    ///
    /// INTENT: the conflict is refused at the operation, never at boot. The
    /// daemon that owns the catalog stays up to report it and to delete it — a
    /// conflict you can read about and repair, not one that locks the door you
    /// would repair it through. Everything that touches no vector (keyword
    /// search, project listing, deletion) keeps working meanwhile.
    pub fn recipe_conflict(&self) -> Option<String> {
        self.conflicted().then(|| {
            format!(
                "catalog `{}` holds vectors built with `{}`, but this daemon serves `{}`; \
                 re-index it — `findery catalog rm {}` then `findery init --catalog {}` — or \
                 start the daemon with the embedder settings it was built with",
                self.name, self.stored_recipe, self.recipe, self.name, self.name
            )
        })
    }

    /// Refuses when this catalog cannot accept or serve vectors.
    pub fn ensure_vectors_usable(&self) -> Result<()> {
        match self.recipe_conflict() {
            Some(message) => Err(Error::Config(message)),
            None => Ok(()),
        }
    }

    /// Everything `GET /v1/catalogs` reports about it.
    pub async fn info(&self, is_default: bool) -> Result<CatalogInfo> {
        let (projects, chunks) = self.catalog.totals().await?;
        Ok(CatalogInfo {
            name: self.name.clone(),
            path: self.catalog.path().display().to_string(),
            recipe: Some(self.stored_recipe.clone()),
            conflict: self.recipe_conflict(),
            projects,
            chunks,
            disk_bytes: self.catalog.disk_bytes().await,
            is_default,
        })
    }
}

/// The width a previously built index remembers, when any catalog has one for
/// the model this daemon is configured to serve.
///
/// INTENT: read before the embedder is built, so a daemon whose runtime is down
/// starts at the width its vectors were written at instead of refusing to boot.
pub async fn remembered_dim(cfg: &StoreConfig, model: &str) -> Result<Option<usize>> {
    for name in findery_store::list_catalogs(cfg)? {
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

/// Opens every catalog on disk and rebuilds the project → catalog map.
///
/// A project id registered in two catalogs resolves to the first in order
/// (default, then names alphabetically). The daemon's own writes cannot create
/// that state — registering into a second catalog moves the project — so it
/// takes a hand-edited database to reach, and the rule is stated rather than
/// discovered.
pub(crate) async fn open_all(
    cfg: &Config,
    recipe: &str,
    dim: usize,
) -> Result<(
    BTreeMap<String, Arc<CatalogHandle>>,
    HashMap<ProjectId, String>,
)> {
    let mut catalogs = BTreeMap::new();
    let mut homes = HashMap::new();
    let mut names = findery_store::list_catalogs(&cfg.store)?;
    // The default catalog always exists, so a fresh install has somewhere to put
    // its first project without a create step.
    if !names.iter().any(|n| n == DEFAULT_CATALOG) {
        names.insert(0, DEFAULT_CATALOG.to_string());
    }
    for name in names {
        let handle = Arc::new(CatalogHandle::open(cfg, &name, recipe, dim).await?);
        for project in handle.catalog.list_projects().await? {
            homes.insert(project.id, name.clone());
        }
        catalogs.insert(name, handle);
    }
    Ok((catalogs, homes))
}

impl Indexer {
    /// Every open catalog, default first then names in order.
    pub fn handles(&self) -> Vec<Arc<CatalogHandle>> {
        self.catalogs.read().values().cloned().collect()
    }

    /// One open catalog by name.
    pub fn handle(&self, name: &str) -> Result<Arc<CatalogHandle>> {
        self.catalogs
            .read()
            .get(name)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("catalog `{name}`")))
    }

    /// The catalog a project lives in.
    pub fn home_of(&self, id: &ProjectId) -> Result<String> {
        self.homes
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("project `{id}`")))
    }

    /// The open catalog a project lives in, ready to serve.
    pub fn catalog_for(&self, id: &ProjectId) -> Result<Arc<CatalogHandle>> {
        self.handle(&self.home_of(id)?)
    }

    /// Opens a catalog, creating it when it does not exist.
    ///
    /// Creating here rather than behind a separate command is what lets
    /// `findery init --catalog work` be the whole gesture: naming a catalog you
    /// have never used is a decision, not an error.
    pub async fn ensure_catalog(&self, name: &str) -> Result<Arc<CatalogHandle>> {
        if !is_catalog_name(name) {
            return Err(Error::Config(format!("`{name}` is not a catalog name")));
        }
        // Bound before the `if`: the guard must not be alive across the `await`
        // below, which is what keeps this future `Send`.
        let existing = self.catalogs.read().get(name).cloned();
        if let Some(existing) = existing {
            return Ok(existing);
        }
        let handle = Arc::new(
            CatalogHandle::open(&self.cfg, name, &self.recipe(), self.embedder.dim()).await?,
        );
        self.catalogs
            .write()
            .insert(name.to_string(), handle.clone());
        Ok(handle)
    }

    /// Remembers which catalog holds a project.
    pub(crate) fn remember_home(&self, id: &ProjectId, catalog: &str) {
        self.homes.write().insert(id.clone(), catalog.to_string());
    }

    /// Forgets a project's home.
    pub(crate) fn forget_home(&self, id: &ProjectId) {
        self.homes.write().remove(id);
    }

    /// What the daemon serves, in the shape the API reports it.
    pub async fn list_catalogs(&self) -> Result<Vec<CatalogInfo>> {
        let mut out = Vec::new();
        for handle in self.handles() {
            out.push(
                handle
                    .info(handle.name == self.cfg.store.default_catalog)
                    .await?,
            );
        }
        Ok(out)
    }

    /// Creates a catalog, or reports the one that is already there.
    pub async fn create_catalog(&self, name: &str) -> Result<CatalogInfo> {
        let handle = self.ensure_catalog(name).await?;
        handle.info(name == self.cfg.store.default_catalog).await
    }

    /// Deletes a catalog, its projects and their vectors.
    ///
    /// INTENT: it goes through the daemon rather than through the CLI's own
    /// filesystem call, because the daemon is the only process that knows every
    /// handle on the file — and on Windows a catalog still open cannot be
    /// removed at all. Nothing here needs the embedder: this is the door the
    /// recipe conflict used to lock.
    pub async fn forget_catalog(&self, name: &str) -> Result<CatalogRemoved> {
        let handle = self.handle(name)?;
        let projects = handle.catalog.list_projects().await?;
        for project in &projects {
            // Vectors first: a catalog row with no vectors is recoverable, an
            // orphaned collection is not.
            handle.store.drop_project(&project.id).await?;
            self.forget_home(&project.id);
        }
        // Dropped before the file is removed: the map is the last handle the
        // daemon holds, and the request's own `Arc` is dropped with `handle`.
        self.catalogs.write().remove(name);
        drop(handle);
        let removed = findery_store::delete_catalog(&self.cfg.store, name)?;
        Ok(CatalogRemoved {
            removed,
            projects_removed: projects.len() as u64,
            projects: projects.iter().map(|p| p.id.to_string()).collect(),
        })
    }
}
