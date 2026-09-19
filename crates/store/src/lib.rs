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
use findery_core::config::{StoreBackend, StoreConfig};
use findery_core::traits::VectorStore;
use std::path::Path;
use std::sync::Arc;

pub use catalog::Catalog;
pub use export::{ExportFormat, ExportReport, export_project};
pub use qdrant::QdrantStore;
pub use sqlite_store::SqliteVectorStore;

/// Opens the catalog declared by the configuration.
pub fn open_catalog(cfg: &StoreConfig) -> Result<Catalog> {
    Catalog::open(Path::new(&cfg.data_dir))
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
        let catalog = open_catalog(&cfg).expect("catalog");
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
            let catalog = open_catalog(&cfg).expect("catalog");
            catalog.set_meta("k", "v").await.expect("write");
        }
        let catalog = open_catalog(&cfg).expect("reopen");
        assert_eq!(catalog.get_meta("k").await.expect("read"), Some("v".into()));
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
