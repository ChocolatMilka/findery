//! # findery-store — layer L1
//!
//! Durable state. Two things live here and they are deliberately separate:
//!
//! * the **catalog** ([`Catalog`]) — one SQLite file holding projects, files,
//!   chunks, the content-addressed vector cache and the full-text index. Always
//!   present, whatever the vector backend. This is the file worth backing up.
//! * the **vector store** ([`findery_core::traits::VectorStore`]) — where vectors
//!   are written and searched. [`SqliteVectorStore`] keeps them next to the
//!   catalog; [`QdrantStore`] hands them to a Qdrant cluster. Both are
//!   rebuildable from the catalog, which is why swapping one for the other is a
//!   configuration change plus a re-sync.

pub mod catalog;
pub mod export;
pub mod qdrant;
pub mod schema;
pub mod sqlite_store;

use findery_core::Result;
use findery_core::config::{StoreBackend, StoreConfig, catalog_path, catalogs_dir};
use findery_core::model::{DEFAULT_CATALOG, is_catalog_name};
use findery_core::traits::VectorStore;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use catalog::Catalog;
pub use export::{ExportFormat, ExportReport, export_project};
pub use qdrant::QdrantStore;
pub use sqlite_store::SqliteVectorStore;

/// Opens one named catalog declared by the configuration.
pub fn open_catalog(cfg: &StoreConfig, name: &str) -> Result<Catalog> {
    Catalog::open_file(Path::new(&catalog_path(&cfg.data_dir, name)), name)
}

/// True when a catalog file already exists for `name`.
///
/// INTENT: asked before opening, because opening *creates*. A `catalog rm` on a
/// name that was never there must be a refusal, not a fresh empty database.
pub fn catalog_exists(cfg: &StoreConfig, name: &str) -> bool {
    Path::new(&catalog_path(&cfg.data_dir, name)).is_file()
}

/// Every catalog that exists on disk, default first then names in order.
///
/// Read from the filesystem rather than from a registry: a catalog *is* its
/// file, so a registry is one more thing that can disagree with the disk.
pub fn list_catalogs(cfg: &StoreConfig) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if catalog_exists(cfg, DEFAULT_CATALOG) {
        out.push(DEFAULT_CATALOG.to_string());
    }
    let dir = PathBuf::from(catalogs_dir(&cfg.data_dir));
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // No `catalogs/` directory is the ordinary single-catalog install.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    let mut named: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("catalog.db").is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| is_catalog_name(n) && n != DEFAULT_CATALOG)
        .collect();
    named.sort();
    out.extend(named);
    Ok(out)
}

/// The recipe a catalog remembers, without keeping it open.
///
/// INTENT: read before the embedder is built, so a daemon whose embedding
/// runtime is down still starts at the width its vectors were written at —
/// which is what keeps `findery doctor` answerable. Never called for a catalog
/// that does not exist: opening creates.
pub async fn stored_recipe(cfg: &StoreConfig, name: &str) -> Result<Option<String>> {
    if !catalog_exists(cfg, name) {
        return Ok(None);
    }
    open_catalog(cfg, name)?.get_meta("embedding_recipe").await
}

/// Deletes a catalog and everything in it.
///
/// The default catalog is refused: removing it is `findery rm` on each of its
/// projects, not a rename of the whole install.
pub fn delete_catalog(cfg: &StoreConfig, name: &str) -> Result<bool> {
    if name == DEFAULT_CATALOG {
        return Err(findery_core::Error::Config(format!(
            "`{DEFAULT_CATALOG}` is the catalog every install has; delete its projects instead"
        )));
    }
    if !is_catalog_name(name) {
        return Err(findery_core::Error::Config(format!(
            "`{name}` is not a catalog name"
        )));
    }
    let dir = PathBuf::from(catalogs_dir(&cfg.data_dir)).join(name);
    if !dir.is_dir() {
        return Ok(false);
    }
    // A catalog this process still holds open cannot be removed on Windows, so
    // the caller drops its handle first — which is why removal goes through the
    // daemon, the one process that knows every handle there is.
    std::fs::remove_dir_all(&dir).map_err(|e| {
        findery_core::Error::Config(format!(
            "cannot delete catalog `{name}` at {}: {e}",
            dir.display()
        ))
    })?;
    Ok(true)
}

/// Builds the configured vector backend on top of an open catalog.
pub fn build_vector_store(cfg: &StoreConfig, catalog: Catalog) -> Result<Arc<dyn VectorStore>> {
    match cfg.backend {
        StoreBackend::Sqlite => Ok(Arc::new(SqliteVectorStore::new(catalog))),
        StoreBackend::Qdrant => Ok(Arc::new(QdrantStore::new(cfg.qdrant.clone())?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use findery_core::ids::ProjectId;
    use findery_core::model::SearchFilter;
    use findery_core::traits::{VectorPayload, VectorRecord};

    #[tokio::test]
    async fn the_sqlite_backend_needs_nothing_but_a_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = StoreConfig {
            backend: StoreBackend::Sqlite,
            data_dir: dir.path().display().to_string(),
            ..StoreConfig::default()
        };
        let catalog = open_catalog(&cfg, DEFAULT_CATALOG).expect("catalog");
        let store = build_vector_store(&cfg, catalog).expect("store");
        assert_eq!(store.backend(), "sqlite");

        let p = ProjectId::new("demo").expect("id");
        store
            .upsert(
                &p,
                &[VectorRecord {
                    id: findery_core::ids::ChunkId::derive(&p, "a.rs", 0),
                    vector: vec![1.0, 0.0],
                    payload: VectorPayload {
                        path: "a.rs".into(),
                        lang: findery_core::model::Lang::Rust,
                        kind: findery_core::model::ChunkKind::File,
                        symbol: None,
                    },
                }],
            )
            .await
            .expect("upsert");
        let hits = store
            .search(&p, &[1.0, 0.0], 5, &SearchFilter::default())
            .await
            .expect("search");
        assert_eq!(hits.len(), 1);
    }

    #[tokio::test]
    async fn the_catalog_survives_being_reopened() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = StoreConfig {
            data_dir: dir.path().display().to_string(),
            ..Default::default()
        };
        {
            let catalog = open_catalog(&cfg, DEFAULT_CATALOG).expect("catalog");
            catalog.set_meta("k", "v").await.expect("write");
        }
        let catalog = open_catalog(&cfg, DEFAULT_CATALOG).expect("reopen");
        assert_eq!(catalog.get_meta("k").await.expect("read"), Some("v".into()));
    }

    /// The default catalog keeps the flat path it has always had, so an
    /// existing install is not moved by this feature; a named one lives under
    /// `catalogs/`, where its name is a directory and nothing else may be.
    #[tokio::test]
    async fn the_default_catalog_stays_flat_and_named_ones_get_a_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = StoreConfig {
            data_dir: dir.path().display().to_string(),
            ..StoreConfig::default()
        };
        {
            let default = open_catalog(&cfg, DEFAULT_CATALOG).expect("default");
            assert_eq!(default.path(), dir.path().join("catalog.db"));
            let work = open_catalog(&cfg, "work").expect("named");
            assert_eq!(work.path(), dir.path().join("catalogs/work/catalog.db"));
            assert_eq!(work.name(), "work");
        }

        assert_eq!(
            list_catalogs(&cfg).expect("list"),
            vec![DEFAULT_CATALOG.to_string(), "work".to_string()]
        );
        // The handle is closed, which is the state removal requires.
        assert!(delete_catalog(&cfg, "work").expect("delete"));
        assert!(!catalog_exists(&cfg, "work"));
        assert_eq!(
            list_catalogs(&cfg).expect("list"),
            vec!["default".to_string()]
        );
    }

    /// Removing the default catalog would be a rename of the install; the
    /// refusal is the feature.
    #[test]
    fn the_default_catalog_cannot_be_deleted_and_a_bad_name_is_refused() {
        let cfg = StoreConfig::default();
        assert!(delete_catalog(&cfg, DEFAULT_CATALOG).is_err());
        assert!(delete_catalog(&cfg, "../etc").is_err());
        assert!(!delete_catalog(&cfg, "never-created").expect("absent is not an error"));
    }

    #[test]
    fn selecting_qdrant_builds_a_client_without_contacting_it() {
        let cfg = StoreConfig {
            backend: StoreBackend::Qdrant,
            ..StoreConfig::default()
        };
        let catalog = Catalog::open_in_memory().expect("catalog");
        let store = build_vector_store(&cfg, catalog).expect("store");
        assert_eq!(store.backend(), "qdrant");
    }
}
