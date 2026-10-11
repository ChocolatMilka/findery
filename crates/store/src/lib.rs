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
pub mod registry;
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
/// A move that cannot be made is an error, not a warning: opening *creates*, so
/// the alternative is a fresh empty database beside the real index with the user
/// told nothing.
///
/// Idempotent, and resumable: every step is "complete this rename if it is still
/// owed", so a run interrupted between the directory and the file finishes on the
/// next one instead of leaving a bucket that no scan can see.
pub fn migrate_legacy_layout(cfg: &StoreConfig) -> Result<()> {
    let data_dir = PathBuf::from(cfg.data_dir.trim_end_matches('/'));
    // The default bucket is flat: <data_dir>/catalog.db -> <data_dir>/bucket.db
    settle(&data_dir.join("catalog.db"), &data_dir.join("bucket.db"))?;

    // Named buckets are a directory each. Moving the directory is one step and
    // renaming the file inside it is another, so both are attempted separately.
    let legacy_dir = data_dir.join("catalogs");
    let current_dir = PathBuf::from(buckets_dir(&cfg.data_dir));
    if let Ok(entries) = std::fs::read_dir(&legacy_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let (from, to) = (entry.path(), current_dir.join(&name));
            if !from.is_dir() {
                continue;
            }
            std::fs::create_dir_all(&current_dir)
                .and_then(|()| std::fs::rename(&from, &to))
                .map_err(|e| stranded(&from, &to, e))?;
        }
    }

    // Whatever is still named for the old vocabulary, wherever it sits — which
    // includes the directories just moved above, and any left half-finished by
    // an earlier run whose `catalogs/` entry is long gone.
    for entry in std::fs::read_dir(&current_dir)
        .into_iter()
        .flatten()
        .flatten()
    {
        let dir = entry.path();
        settle(&dir.join("catalog.db"), &dir.join("bucket.db"))?;
    }
    Ok(())
}

/// Completes one rename, doing nothing when there is nothing to do.
fn settle(from: &Path, to: &Path) -> Result<()> {
    // `is_file`, not `exists`: a *directory* named for either side is not a
    // database, and treating one as migrated would skip the real index.
    if !from.is_file() || to.is_file() {
        return Ok(());
    }
    // Only what has to be opened is opened. A database with no journal moves as
    // a file, and asking SQLite to open it first would impose its opinion on a
    // file that is not a database — turning "move this" into "this is corrupt".
    if journal_holds_pages(from) {
        consolidate(from).map_err(|e| stranded(from, to, e))?;
    }
    rename_file_set(from, to).map_err(|e| stranded(from, to, e))
}

/// True when the journal beside `db` still holds committed pages.
fn journal_holds_pages(db: &Path) -> bool {
    std::fs::metadata(with_suffix(db, "-wal")).is_ok_and(|m| m.len() > 0)
}

/// Empties the write-ahead log by letting SQLite recover it.
///
/// A database with a live journal cannot be renamed safely: the rename is not
/// atomic across the file set, and a failure between the two moves detaches
/// committed pages from the database that owns them. SQLite is the one thing
/// that can put those pages back, so it is asked to — after which there is no
/// journal left to lose. Closing the last connection would do it eventually;
/// the checkpoint says so deliberately.
fn consolidate(db: &Path) -> std::io::Result<()> {
    let conn = rusqlite::Connection::open(db).map_err(std::io::Error::other)?;
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .map_err(std::io::Error::other)?;
    Ok(())
}

/// The refusal a half-migrated install gets. It names both paths and the manual
/// move, because the cause is usually transient — a held file, a read-only
/// volume — and the user's next move is to fix it and retry.
fn stranded(from: &Path, to: &Path, cause: std::io::Error) -> findery_core::Error {
    findery_core::Error::Store(format!(
        "cannot move {} to {}: {cause}\n  \
         this install predates the current file names, and starting on a fresh \
         database beside them would hide the index you already have; move it by \
         hand (`mv {} {}`) and start again",
        from.display(),
        to.display(),
        from.display(),
        to.display()
    ))
}

/// Renames a SQLite database together with the journal files named after it —
/// SQLite derives `-wal` and `-shm` from the database's own name.
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
            findery_core::Error::Store(format!(
                "cannot create the directory for bucket `{name}` at {}: {e}",
                parent.display()
            ))
        })?;
    }
    Bucket::open_file(file, name)
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

/// Refuses a bucket that may not be removed. Every caller asks this *first*:
/// removal drops vectors and forgets homes long before it unlinks a file, so a
/// guard at the end of the path has already cost the user their index.
pub fn ensure_removable(name: &str) -> Result<()> {
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
    Ok(())
}

/// Deletes a bucket and everything in it.
///
/// The default bucket is refused: removing it is `findery rm` on each of its
/// projects, not a rename of the whole install.
pub fn delete_bucket(cfg: &StoreConfig, name: &str) -> Result<bool> {
    ensure_removable(name)?;
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

/// Deletes a bucket that lives at an explicit path, journals included — a `-wal`
/// left beside nothing is a file the user cannot explain or safely delete.
pub fn delete_bucket_file(path: &str) -> Result<bool> {
    let file = Path::new(path);
    if !file.is_file() {
        return Ok(false);
    }
    remove_file_set(file)?;
    // The directory is left alone even when it is now empty. It is the user's:
    // `bucket.path` may be `D:/team/findery.db`, and removing `D:/team` because
    // the bucket in it was deleted is deleting something the daemon was never
    // given.
    Ok(true)
}

fn remove_file_set(file: &Path) -> Result<()> {
    std::fs::remove_file(file).map_err(|e| {
        findery_core::Error::Store(format!("cannot delete {}: {e}", file.display()))
    })?;
    for suffix in ["-wal", "-shm"] {
        let journal = with_suffix(file, suffix);
        if journal.is_file() {
            std::fs::remove_file(&journal).map_err(|e| {
                findery_core::Error::Store(format!("cannot delete {}: {e}", journal.display()))
            })?;
        }
    }
    Ok(())
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

        migrate_legacy_layout(&cfg).expect("migrates");
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

        migrate_legacy_layout(&cfg).expect("migrates");
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

        migrate_legacy_layout(&cfg).expect("migrates");
        assert_eq!(list_buckets(&cfg).expect("list"), vec!["work".to_string()]);
        assert!(open_bucket(&cfg, "work").is_ok());
    }

    /// The failure this whole path exists to prevent. A move that cannot be made
    /// must stop the boot: nothing downstream ever looks at the old name again,
    /// and opening *creates* — so the alternative is a fresh empty database
    /// beside the real index, with the real index orphaned and the user told
    /// nothing.
    #[test]
    fn a_legacy_file_that_cannot_be_moved_stops_the_boot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = StoreConfig {
            data_dir: dir.path().display().to_string(),
            ..StoreConfig::default()
        };
        Bucket::open_file(&dir.path().join("catalog.db"), DEFAULT_BUCKET).expect("legacy file");
        // A non-empty directory sits where the migrated file has to land, so the
        // rename cannot succeed.
        let blocked = dir.path().join("bucket.db");
        std::fs::create_dir_all(&blocked).expect("mkdir");
        std::fs::write(blocked.join("keep"), b"x").expect("write");

        let err = migrate_legacy_layout(&cfg).expect_err("must refuse");
        let text = err.to_string();
        assert!(text.contains("catalog.db"), "{text}");
        assert!(text.contains("mv "), "the refusal names the repair: {text}");
        assert!(
            dir.path().join("catalog.db").is_file(),
            "the index is still where it was"
        );
    }

    /// A database whose journal still holds committed pages is moved by first
    /// letting SQLite put those pages back. Refusing instead leaves the user to
    /// resolve it by hand, and a real install has exactly this shape: a journal
    /// from the last time the old daemon was killed.
    #[test]
    fn a_journal_holding_pages_is_consolidated_before_the_move() {
        // A live database, written and left with its journal — the shape a
        // process that was killed leaves behind. It is built here and *copied*
        // because the point of the case is a file set nothing holds open.
        let source = tempfile::tempdir().expect("tempdir");
        let conn = rusqlite::Connection::open(source.path().join("catalog.db")).expect("open");
        conn.pragma_update(None, "journal_mode", "WAL")
            .expect("wal");
        conn.execute("CREATE TABLE projects (id TEXT)", [])
            .expect("schema");
        conn.execute("INSERT INTO projects VALUES ('axle')", [])
            .expect("row");

        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = StoreConfig {
            data_dir: dir.path().display().to_string(),
            ..StoreConfig::default()
        };
        // Copied while the writer is alive, and closed only afterwards: a clean
        // close checkpoints the journal away, which is the one thing this case
        // is about.
        for suffix in ["", "-wal", "-shm"] {
            let from = source.path().join(format!("catalog.db{suffix}"));
            if from.is_file() {
                std::fs::copy(&from, dir.path().join(format!("catalog.db{suffix}"))).expect("copy");
            }
        }
        drop(conn);
        assert!(
            journal_holds_pages(&dir.path().join("catalog.db")),
            "the fixture needs committed pages in the journal"
        );

        migrate_legacy_layout(&cfg).expect("migrates");

        assert!(dir.path().join("bucket.db").is_file());
        assert!(!dir.path().join("catalog.db").exists());
        let rows: i64 = rusqlite::Connection::open(dir.path().join("bucket.db"))
            .expect("reopen")
            .query_row("SELECT count(*) FROM projects", [], |r| r.get(0))
            .expect("count");
        assert_eq!(rows, 1, "the committed row came with it");
    }

    /// An empty journal is what a cleanly closed database leaves, and it must
    /// not block an ordinary install from moving.
    #[test]
    fn an_empty_journal_does_not_block_the_move() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = StoreConfig {
            data_dir: dir.path().display().to_string(),
            ..StoreConfig::default()
        };
        std::fs::write(dir.path().join("catalog.db"), b"db").expect("write");
        std::fs::write(dir.path().join("catalog.db-wal"), b"").expect("write");

        migrate_legacy_layout(&cfg).expect("migrates");
        assert!(dir.path().join("bucket.db").is_file());
        assert!(dir.path().join("bucket.db-wal").is_file());
        assert!(!dir.path().join("catalog.db-wal").exists());
    }

    /// A run interrupted between moving a bucket's directory and renaming the
    /// file inside it leaves a bucket no scan can see — `list_buckets` looks for
    /// `bucket.db`. The next run has to finish it, and `catalogs/` is already
    /// empty by then, so the recovery cannot depend on finding the old entry.
    #[test]
    fn a_bucket_left_half_migrated_is_finished_on_the_next_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = StoreConfig {
            data_dir: dir.path().display().to_string(),
            ..StoreConfig::default()
        };
        let stranded = dir.path().join("buckets").join("discord");
        std::fs::create_dir_all(&stranded).expect("mkdir");
        Bucket::open_file(&stranded.join("catalog.db"), "discord").expect("legacy bucket");

        migrate_legacy_layout(&cfg).expect("migrates");

        assert!(stranded.join("bucket.db").is_file());
        assert!(!stranded.join("catalog.db").exists());
        assert_eq!(
            list_buckets(&cfg).expect("list"),
            vec!["discord".to_string()],
            "the bucket is visible again"
        );
    }

    /// Running the migration over an install that is already current changes
    /// nothing: it runs on every boot.
    #[test]
    fn a_migrated_install_is_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = StoreConfig {
            data_dir: dir.path().display().to_string(),
            ..StoreConfig::default()
        };
        let bucket = open_bucket(&cfg, DEFAULT_BUCKET).expect("bucket");
        drop(bucket);

        migrate_legacy_layout(&cfg).expect("first");
        let before = std::fs::read(dir.path().join("bucket.db")).expect("read");
        migrate_legacy_layout(&cfg).expect("second");
        assert_eq!(
            std::fs::read(dir.path().join("bucket.db")).expect("read"),
            before,
            "an already-migrated install is untouched"
        );
    }

    /// The default bucket is refused *before* anything is touched: the removal
    /// path drops vectors and forgets homes long before it unlinks a file, so a
    /// guard at the end would already have cost the user their index.
    #[test]
    fn removing_the_default_bucket_is_refused_by_the_first_check() {
        assert!(ensure_removable(DEFAULT_BUCKET).is_err());
        assert!(ensure_removable("../etc").is_err());
        assert!(ensure_removable("work").is_ok());
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
