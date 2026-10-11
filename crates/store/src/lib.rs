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

pub mod bucket;
pub mod export;
pub mod qdrant;
pub mod schema;
pub mod sqlite_store;

use findery_core::Result;
use findery_core::config::{StoreBackend, StoreConfig, bucket_path, buckets_dir};
use findery_core::model::{DEFAULT_BUCKET, is_bucket_name};
use findery_core::traits::VectorStore;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use bucket::Bucket;
pub use export::{ExportFormat, ExportReport, export_project};
pub use qdrant::QdrantStore;
pub use sqlite_store::SqliteVectorStore;

/// Moves an install created before the vocabulary change onto the current names.
///
/// INTENT: the file names moved with the vocabulary, and an install that
/// predates the change still has `catalog.db` and `catalogs/<name>/`. The rename
/// is done once, here, before anything opens a handle — the only moment it is
/// safe on every platform. Best-effort by contract: a rename that fails leaves
/// the old path in place and reports itself, because a bucket nobody can find is
/// worse than one whose file kept an old name.
///
/// Returns the non-fatal failures, as messages (CONV-022).
pub fn migrate_legacy_layout(cfg: &StoreConfig) -> Vec<String> {
    let mut warnings = Vec::new();
    let data_dir = PathBuf::from(cfg.data_dir.trim_end_matches('/'));

    let legacy_db = data_dir.join("catalog.db");
    let current_db = data_dir.join("bucket.db");
    if legacy_db.is_file()
        && !current_db.exists()
        && let Err(e) = rename_file_set(&legacy_db, &current_db)
    {
        warnings.push(format!(
            "cannot move {} to {}: {e}",
            legacy_db.display(),
            current_db.display()
        ));
    }

    let legacy_dir = data_dir.join("catalogs");
    let current_dir = PathBuf::from(buckets_dir(&cfg.data_dir));
    let Ok(entries) = std::fs::read_dir(&legacy_dir) else {
        return warnings;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let from = entry.path();
        if !from.is_dir() {
            continue;
        }
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let to = current_dir.join(&name);
        if to.exists() {
            continue;
        }
        if let Err(e) = std::fs::create_dir_all(&current_dir)
            .and_then(|()| std::fs::rename(&from, &to))
            .and_then(|()| rename_file_set(&to.join("catalog.db"), &to.join("bucket.db")))
        {
            warnings.push(format!(
                "cannot move bucket `{name}` from {} to {}: {e}",
                from.display(),
                to.display()
            ));
        }
    }
    warnings
}

/// Renames a SQLite database together with the journal files named after it.
///
/// INTENT: SQLite derives `-wal` and `-shm` from the database file name, so
/// renaming the database alone would detach a write-ahead log that may still
/// hold committed pages.
fn rename_file_set(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)?;
    for suffix in ["-wal", "-shm"] {
        let source = with_suffix(from, suffix);
        if source.is_file() {
            std::fs::rename(&source, with_suffix(to, suffix))?;
        }
    }
    Ok(())
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Opens one named bucket declared by the configuration.
pub fn open_bucket(cfg: &StoreConfig, name: &str) -> Result<Bucket> {
    Bucket::open_file(Path::new(&bucket_path(&cfg.data_dir, name)), name)
}

/// True when a bucket file already exists for `name`.
///
/// INTENT: asked before opening, because opening *creates*. A `bucket rm` on a
/// name that was never there must be a refusal, not a fresh empty database.
pub fn bucket_exists(cfg: &StoreConfig, name: &str) -> bool {
    Path::new(&bucket_path(&cfg.data_dir, name)).is_file()
}

/// Every bucket that exists on disk, default first then names in order.
///
/// Read from the filesystem rather than from a registry: a bucket *is* its
/// file, so a registry is one more thing that can disagree with the disk.
pub fn list_buckets(cfg: &StoreConfig) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if bucket_exists(cfg, DEFAULT_BUCKET) {
        out.push(DEFAULT_BUCKET.to_string());
    }
    let dir = PathBuf::from(buckets_dir(&cfg.data_dir));
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // No `buckets/` directory is the ordinary single-bucket install.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    let mut named: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("bucket.db").is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| is_bucket_name(n) && n != DEFAULT_BUCKET)
        .collect();
    named.sort();
    out.extend(named);
    Ok(out)
}

/// The recipe a bucket remembers, without keeping it open.
///
/// INTENT: read before the embedder is built, so a daemon whose embedding
/// runtime is down still starts at the width its vectors were written at —
/// which is what keeps `findery doctor` answerable. Never called for a bucket
/// that does not exist: opening creates.
pub async fn stored_recipe(cfg: &StoreConfig, name: &str) -> Result<Option<String>> {
    if !bucket_exists(cfg, name) {
        return Ok(None);
    }
    open_bucket(cfg, name)?.get_meta("embedding_recipe").await
}

/// Deletes a bucket and everything in it.
///
/// The default bucket is refused: removing it is `findery rm` on each of its
/// projects, not a rename of the whole install.
pub fn delete_bucket(cfg: &StoreConfig, name: &str) -> Result<bool> {
    if name == DEFAULT_BUCKET {
        return Err(findery_core::Error::Config(format!(
            "`{DEFAULT_BUCKET}` is the bucket every install has; delete its projects instead"
        )));
    }
    if !is_bucket_name(name) {
        return Err(findery_core::Error::Config(format!(
            "`{name}` is not a bucket name"
        )));
    }
    let dir = PathBuf::from(buckets_dir(&cfg.data_dir)).join(name);
    if !dir.is_dir() {
        return Ok(false);
    }
    // A bucket this process still holds open cannot be removed on Windows, so
    // the caller drops its handle first — which is why removal goes through the
    // daemon, the one process that knows every handle there is.
    std::fs::remove_dir_all(&dir).map_err(|e| {
        findery_core::Error::Config(format!(
            "cannot delete bucket `{name}` at {}: {e}",
            dir.display()
        ))
    })?;
    Ok(true)
}

/// Builds the configured vector backend on top of an open bucket.
pub fn build_vector_store(cfg: &StoreConfig, bucket: Bucket) -> Result<Arc<dyn VectorStore>> {
    match cfg.backend {
        StoreBackend::Sqlite => Ok(Arc::new(SqliteVectorStore::new(bucket))),
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
        let bucket = open_bucket(&cfg, DEFAULT_BUCKET).expect("bucket");
        let store = build_vector_store(&cfg, bucket).expect("store");
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
        let cfg = StoreConfig {
            data_dir: dir.path().display().to_string(),
            ..Default::default()
        };
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
        let cfg = StoreConfig {
            data_dir: dir.path().display().to_string(),
            ..StoreConfig::default()
        };
        {
            let default = open_bucket(&cfg, DEFAULT_BUCKET).expect("default");
            assert_eq!(default.path(), dir.path().join("bucket.db"));
            let work = open_bucket(&cfg, "work").expect("named");
            assert_eq!(work.path(), dir.path().join("buckets/work/bucket.db"));
            assert_eq!(work.name(), "work");
        }

        assert_eq!(
            list_buckets(&cfg).expect("list"),
            vec![DEFAULT_BUCKET.to_string(), "work".to_string()]
        );
        // The handle is closed, which is the state removal requires.
        assert!(delete_bucket(&cfg, "work").expect("delete"));
        assert!(!bucket_exists(&cfg, "work"));
        assert_eq!(
            list_buckets(&cfg).expect("list"),
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

    /// An install created before the vocabulary change is moved onto the current
    /// names, once, and its content comes with it.
    #[tokio::test]
    async fn an_existing_install_is_moved_onto_the_current_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = StoreConfig {
            data_dir: dir.path().display().to_string(),
            ..StoreConfig::default()
        };
        {
            let legacy = Bucket::open_file(&dir.path().join("catalog.db"), DEFAULT_BUCKET)
                .expect("legacy file");
            legacy.set_meta("k", "v").await.expect("write");
        }

        assert!(migrate_legacy_layout(&cfg).is_empty());
        assert!(!dir.path().join("catalog.db").exists());
        let moved = open_bucket(&cfg, DEFAULT_BUCKET).expect("moved");
        assert_eq!(moved.get_meta("k").await.expect("read"), Some("v".into()));
    }

    /// A migration that ran halfway leaves both names on disk. The current one
    /// wins, and nothing is overwritten.
    #[tokio::test]
    async fn a_half_finished_migration_never_overwrites_the_current_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = StoreConfig {
            data_dir: dir.path().display().to_string(),
            ..StoreConfig::default()
        };
        {
            let legacy = Bucket::open_file(&dir.path().join("catalog.db"), DEFAULT_BUCKET)
                .expect("legacy file");
            legacy.set_meta("k", "old").await.expect("write");
            let current = open_bucket(&cfg, DEFAULT_BUCKET).expect("current file");
            current.set_meta("k", "current").await.expect("write");
        }

        assert!(migrate_legacy_layout(&cfg).is_empty());
        let current = open_bucket(&cfg, DEFAULT_BUCKET).expect("reopen");
        assert_eq!(
            current.get_meta("k").await.expect("read"),
            Some("current".into())
        );
    }

    /// A named bucket leaves `catalogs/` for `buckets/`, and is listed where the
    /// rest of the daemon expects to find it.
    #[tokio::test]
    async fn a_named_bucket_moves_out_of_the_legacy_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = StoreConfig {
            data_dir: dir.path().display().to_string(),
            ..StoreConfig::default()
        };
        let legacy = dir.path().join("catalogs/work");
        std::fs::create_dir_all(&legacy).expect("legacy dir");
        Bucket::open_file(&legacy.join("catalog.db"), "work").expect("legacy bucket");

        assert!(migrate_legacy_layout(&cfg).is_empty());
        assert_eq!(list_buckets(&cfg).expect("list"), vec!["work".to_string()]);
        assert!(open_bucket(&cfg, "work").is_ok());
    }

    #[test]
    fn selecting_qdrant_builds_a_client_without_contacting_it() {
        let cfg = StoreConfig {
            backend: StoreBackend::Qdrant,
            ..StoreConfig::default()
        };
        let bucket = Bucket::open_in_memory().expect("bucket");
        let store = build_vector_store(&cfg, bucket).expect("store");
        assert_eq!(store.backend(), "qdrant");
    }
}
