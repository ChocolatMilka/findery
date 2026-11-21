//! # findery-store — layer L1
//!
//! Durable state. Two things live here and they are deliberately separate:
//!
//! * the **bucket** ([`Bucket`]) — one SQLite file holding projects, files,
//!   chunks, the content-addressed vector cache and the full-text index. Always
//!   present, whatever the vector backend. This is the file worth backing up.
//! * the **vector store** ([`findery_core::traits::VectorStore`]) — where vectors
//!   are written and searched. [`SqliteVectorStore`] keeps them next to the
//!   bucket; [`QdrantStore`] hands them to a Qdrant cluster. Both are
//!   rebuildable from the bucket, which is why swapping one for the other is a
//!   configuration change plus a re-sync.
//!
//! Which files make up a bucket, and how files written under an older layout
//! move onto the current one, is [`files`]'s; where vectors go is [`vectors`]'s. This module keeps the
//! factories the engine opens buckets through.

pub mod bucket;
pub mod export;
pub mod files;
pub mod vectors;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use findery_core::config::{StoreBackend, StoreConfig, buckets_dir};
use findery_core::model::{DEFAULT_BUCKET, is_bucket_name};
use findery_core::traits::VectorStore;
use findery_core::{Error, Result};

#[cfg(test)]
pub(crate) use test_support::open_bucket;

pub use bucket::{Bucket, CloseReport, read_project_ids_no_create, read_recipe_no_create};
pub use export::{ExportFormat, ExportReport, export_project};
pub use files::adopt::{AdoptNotice, AdoptOutcome, adopt_all, adopt_legacy};
pub use files::layout::{
    FamilyPath, Member, family_absent, family_exists, list_families, remove_file_set, scan,
};
pub use files::migrate::migrate_legacy_layout;
pub use vectors::qdrant::QdrantStore;
pub use vectors::sqlite::SqliteVectorStore;

/// Opens a bucket at an explicit file, wherever that file is.
///
/// The parent directory is created here: a bucket that does not exist yet is the
/// ordinary state of a fresh `init`, not an error to route around.
pub fn open_bucket_file(path: &str, name: &str) -> Result<Bucket> {
    let file = Path::new(path);
    if let Some(parent) = file.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| {
            Error::Store(format!(
                "cannot create the directory for bucket `{name}` at {}: {e}",
                parent.display()
            ))
        })?;
    }
    Bucket::open_file(file, name)
}

/// Refuses a bucket that may not be removed. Every caller asks this *first*:
/// removal drops vectors and forgets homes long before it unlinks a file, so a
/// guard at the end of the path has already cost the user their index.
///
/// Both refusals are `Invalid`: the request names something that may not be
/// removed, which is the caller's to change — nothing is misconfigured.
pub fn ensure_removable(name: &str) -> Result<()> {
    if name == DEFAULT_BUCKET {
        return Err(Error::Invalid(format!(
            "`{DEFAULT_BUCKET}` is the bucket every install has; delete its projects instead"
        )));
    }
    if !is_bucket_name(name) {
        return Err(Error::Invalid(format!("`{name}` is not a bucket name")));
    }
    Ok(())
}

/// Deletes the directory of a bucket at its derived path, and everything in it.
///
/// The default bucket is refused: removing it is `findery rm` on each of its
/// projects, not a rename of the whole install. A file another process holds
/// is [`Error::busy_removal`], naming the command to run again once it is
/// released.
pub fn delete_bucket(cfg: &StoreConfig, name: &str) -> Result<bool> {
    ensure_removable(name)?;
    let dir = PathBuf::from(buckets_dir(&cfg.data_dir)).join(name);
    if !dir.is_dir() {
        return Ok(false);
    }
    // A bucket still open cannot be removed on Windows, so the caller closes
    // its handle first — which is why removal goes through the daemon, the one
    // process that knows every handle there is.
    std::fs::remove_dir_all(&dir).map_err(|e| {
        if files::journal::is_held(&e) {
            Error::busy_removal(
                &dir.display().to_string(),
                &e,
                &format!("findery bucket rm {name}"),
            )
        } else {
            Error::Store(format!(
                "cannot delete bucket `{name}` at {}: {e}",
                dir.display()
            ))
        }
    })?;
    Ok(true)
}

/// Builds the configured vector backend on top of an open bucket.
///
/// `namespace` is the bucket file's `meta.vector_namespace`
/// ([`Bucket::vector_namespace`]): Qdrant puts it between the collection
/// prefix and the project id; the SQLite backend has no use for it, since its
/// vectors already live in the member file of their recipe.
pub fn build_vector_store(
    cfg: &StoreConfig,
    bucket: Bucket,
    namespace: &str,
) -> Result<Arc<dyn VectorStore>> {
    match cfg.backend {
        StoreBackend::Sqlite => Ok(Arc::new(SqliteVectorStore::new(bucket))),
        StoreBackend::Qdrant => Ok(Arc::new(QdrantStore::new(cfg.qdrant.clone(), namespace)?)),
    }
}

#[cfg(test)]
mod tests {
    use findery_core::ids::ProjectId;
    use findery_core::model::SearchFilter;
    use findery_core::traits::{VectorPayload, VectorRecord};

    use super::*;

    fn config_in(dir: &Path) -> StoreConfig {
        StoreConfig {
            data_dir: dir.display().to_string(),
            ..StoreConfig::default()
        }
    }

    fn data_dir_holds(dir: &Path, rel: &str) -> bool {
        dir.join(rel).exists()
    }

    #[tokio::test]
    async fn the_sqlite_backend_needs_nothing_but_a_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = StoreConfig {
            backend: StoreBackend::Sqlite,
            ..config_in(dir.path())
        };
        let bucket = open_bucket(&cfg, DEFAULT_BUCKET).expect("bucket");
        let store = build_vector_store(&cfg, bucket, "").expect("store");
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
    async fn the_bucket_survives_being_reopened() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_in(dir.path());
        {
            let bucket = open_bucket(&cfg, DEFAULT_BUCKET).expect("bucket");
            bucket.set_meta("k", "v").await.expect("write");
        }
        let bucket = open_bucket(&cfg, DEFAULT_BUCKET).expect("reopen");
        assert_eq!(bucket.get_meta("k").await.expect("read"), Some("v".into()));
    }

    /// The default bucket keeps its flat path; a named one lives under
    /// `buckets/`, where its name is a directory and nothing else may be.
    #[tokio::test]
    async fn the_default_bucket_stays_flat_and_named_ones_get_a_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_in(dir.path());
        {
            let default = open_bucket(&cfg, DEFAULT_BUCKET).expect("default");
            assert_eq!(default.path(), dir.path().join("bucket.db"));
            let work = open_bucket(&cfg, "work").expect("named");
            assert_eq!(work.path(), dir.path().join("buckets/work/bucket.db"));
            assert_eq!(work.name(), "work");
        }

        assert_eq!(
            list_families(&cfg).expect("list"),
            vec![DEFAULT_BUCKET.to_string(), "work".to_string()]
        );
        // The handle is closed, which is the state removal requires.
        assert!(delete_bucket(&cfg, "work").expect("delete"));
        assert!(!data_dir_holds(dir.path(), "buckets/work"));
        assert_eq!(
            list_families(&cfg).expect("list"),
            vec!["default".to_string()]
        );
    }

    /// Removing the default bucket would be a rename of the install; the
    /// refusal is the feature.
    #[test]
    fn the_default_bucket_cannot_be_deleted_and_a_bad_name_is_refused() {
        let cfg = StoreConfig::default();
        assert!(delete_bucket(&cfg, DEFAULT_BUCKET).is_err());
        assert!(delete_bucket(&cfg, "../etc").is_err());
        assert!(!delete_bucket(&cfg, "never-created").expect("absent is not an error"));
    }

    /// The default bucket is refused *before* anything is touched: the removal
    /// path drops vectors and forgets homes long before it unlinks a file, so a
    /// guard at the end would already have cost the user their index.
    #[test]
    fn removing_the_default_bucket_is_refused_by_the_first_check() {
        assert_eq!(
            ensure_removable(DEFAULT_BUCKET).map_err(|e| e.code()),
            Err("invalid")
        );
        assert_eq!(
            ensure_removable("../etc").map_err(|e| e.code()),
            Err("invalid")
        );
        assert!(ensure_removable("work").is_ok());
    }

    #[test]
    fn selecting_qdrant_builds_a_client_without_contacting_it() {
        let cfg = StoreConfig {
            backend: StoreBackend::Qdrant,
            ..StoreConfig::default()
        };
        let bucket = Bucket::open_in_memory().expect("bucket");
        let store = build_vector_store(&cfg, bucket.clone(), "").expect("store");
        assert_eq!(store.backend(), "qdrant");
        assert!(build_vector_store(&cfg, bucket, "Bad Name").is_err());
    }
}

#[cfg(test)]
mod test_support {
    use findery_core::config::bucket_path;

    use super::*;

    /// Opens (creating) the bucket a name derives, the way tests set one up.
    pub(crate) fn open_bucket(cfg: &StoreConfig, name: &str) -> Result<Bucket> {
        Bucket::open_file(Path::new(&bucket_path(&cfg.data_dir, name)), name)
    }
}
