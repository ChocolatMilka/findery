//! The bucket: the durable record of what is indexed.
//!
//! One SQLite file holds projects, files, chunks, the content-addressed vector
//! cache and the full-text index. The vector backend is swappable and always
//! rebuildable from here — this file is the one thing worth backing up.
//!
//! INTENT: rusqlite is synchronous, so every public method hops onto
//! `spawn_blocking` through [`Bucket::with_conn`]. A single mutex serializes
//! access, which is the right trade for a single-writer daemon: queries are
//! indexed lookups, and vector search never touches this connection at all.
//!
//! The connection sits in an `Option` so that [`Bucket::close`] can end it for
//! every clone at once. Without that, the OS lock on the file lasted as long as
//! the last `Arc` — a search in flight, a finished job still holding its handle
//! — and on Windows a file whose lock outlives its removal cannot be removed.

mod cache;
mod chunks;
mod projects;
pub(crate) mod schema;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::{Connection, OpenFlags};

use findery_core::{Error, Result};

use crate::files::journal;

/// The `meta` key holding the Qdrant collection namespace of this file.
const VECTOR_NAMESPACE_KEY: &str = "vector_namespace";

/// Handle on the bucket database.
#[derive(Clone)]
pub struct Bucket {
    /// `None` once [`Bucket::close`] ran: every clone sees it at once.
    conn: Arc<Mutex<Option<Connection>>>,
    path: PathBuf,
    /// Shared, not copied: every query's closure carries it for its message.
    name: Arc<str>,
}

/// What [`Bucket::close`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CloseReport {
    /// False when an earlier close had already released the file.
    pub was_open: bool,
    /// Set when the final checkpoint could not fold the whole journal back.
    /// The connection is closed regardless; this says the `-wal` may still
    /// hold pages, because another connection — usually another process — is
    /// reading or writing the same file.
    pub incomplete_checkpoint: Option<String>,
}

impl Bucket {
    /// Opens (creating if needed) `dir/bucket.db` and migrates it.
    pub fn open(dir: &Path) -> Result<Self> {
        Self::open_file(&dir.join("bucket.db"), findery_core::model::DEFAULT_BUCKET)
    }

    /// Opens one bucket file, creating it and its directory if needed.
    ///
    /// INTENT: the caller owns the path — `findery_core::config::bucket_path`
    /// decides where a bucket lives — so the layout is one definition shared by
    /// the daemon that writes it and the CLI that reports it.
    pub fn open_file(path: &Path, name: &str) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path).map_err(schema::sql)?;
        Self::wrap(conn, path, name)
    }

    /// Opens a bucket file that must already exist, and migrates it.
    ///
    /// INTENT: a read path must never create a file. `open_file` creates,
    /// which is right for a write that names a new bucket and wrong for
    /// everything else: a lookup that creates leaves an empty database beside
    /// the real one, and the next scan counts it as a member.
    pub fn open_existing(path: &Path, name: &str) -> Result<Self> {
        if !path.is_file() {
            return Err(Error::NotFound(format!(
                "bucket `{name}` has no file at {}",
                path.display()
            )));
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(schema::sql)?;
        Self::wrap(conn, path, name)
    }

    /// Opens an in-memory bucket. Tests only — nothing survives the process.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(schema::sql)?;
        Self::wrap(
            conn,
            Path::new(":memory:"),
            findery_core::model::DEFAULT_BUCKET,
        )
    }

    fn wrap(conn: Connection, path: &Path, name: &str) -> Result<Self> {
        schema::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(Some(conn))),
            path: path.to_path_buf(),
            name: Arc::from(name),
        })
    }

    /// Path of the database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Name this bucket answers to.
    ///
    /// Stamped on every [`Project`](findery_core::model::Project) read out of
    /// it, so the project's bucket is a fact about where the row lives rather
    /// than a column that could disagree with it.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Runs a closure against the connection on the blocking pool.
    ///
    /// The one path every query takes, so a closed bucket answers the same
    /// `NotFound` from every method of every clone.
    pub async fn with_conn<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let (conn, name) = (self.conn.clone(), self.name.clone());
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock();
            match guard.as_ref() {
                Some(conn) => f(conn),
                None => Err(Error::NotFound(format!("bucket `{name}` was closed"))),
            }
        })
        .await
        .map_err(|e| Error::Internal(format!("bucket task: {e}")))?
    }

    /// Releases the file for good, for this handle and every clone of it.
    ///
    /// Waits for a transaction in flight (it holds the same lock), folds the
    /// journal back into the database, then closes the connection — after
    /// which the OS holds no lock on the file whatever number of clones are
    /// still alive, and each of them answers `NotFound`. Idempotent: a second
    /// close reports `was_open: false` and does nothing.
    ///
    /// An incomplete checkpoint does not stop the close: the file is released
    /// either way, and the report says the `-wal` may still hold pages.
    pub async fn close(&self) -> Result<CloseReport> {
        let conn = self.conn.clone();
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            // Taken under the lock, so a query that starts after this line
            // already sees the bucket closed rather than racing the close.
            let Some(conn) = conn.lock().take() else {
                return Ok(CloseReport::default());
            };
            let incomplete_checkpoint = match journal::checkpoint_truncate(&conn) {
                Ok(row) if row.complete() => None,
                Ok(row) => Some(row.describe(&path)),
                Err(e) => Some(format!("checkpoint of {} failed: {e}", path.display())),
            };
            if let Some(note) = &incomplete_checkpoint {
                tracing::warn!(path = %path.display(), "{note}");
            }
            conn.close()
                .map_err(|(_, e)| Error::Store(format!("cannot close {}: {e}", path.display())))?;
            Ok(CloseReport {
                was_open: true,
                incomplete_checkpoint,
            })
        })
        .await
        .map_err(|e| Error::Internal(format!("bucket task: {e}")))?
    }

    /// True once [`Bucket::close`] ran on this handle or any clone of it.
    pub fn is_closed(&self) -> bool {
        self.conn.lock().is_none()
    }

    /// Bytes the database occupies on disk, WAL included.
    pub async fn disk_bytes(&self) -> u64 {
        let base = self.path.clone();
        let mut total = 0u64;
        for suffix in ["", "-wal", "-shm"] {
            let p = journal::with_suffix(&base, suffix);
            if let Ok(meta) = tokio::fs::metadata(&p).await {
                total += meta.len();
            }
        }
        total
    }

    /// Reads a `meta` key.
    pub async fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let key = key.to_string();
        self.with_conn(move |c| {
            let mut stmt = c
                .prepare("SELECT value FROM meta WHERE key = ?1")
                .map_err(schema::sql)?;
            let mut rows = stmt.query([&key]).map_err(schema::sql)?;
            match rows.next().map_err(schema::sql)? {
                Some(row) => Ok(Some(row.get::<_, String>(0).map_err(schema::sql)?)),
                None => Ok(None),
            }
        })
        .await
    }

    /// Writes a `meta` key.
    pub async fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let (key, value) = (key.to_string(), value.to_string());
        self.with_conn(move |c| {
            c.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![key, value],
            )
            .map_err(schema::sql)?;
            Ok(())
        })
        .await
    }

    /// Refuses to run against a bucket built with a different embedding
    /// recipe, which would mix incompatible vector spaces in one index.
    ///
    /// Returns the previously recorded recipe when it differs.
    pub async fn claim_recipe(&self, recipe: &str) -> Result<Option<String>> {
        match self.get_meta("embedding_recipe").await? {
            Some(known) if known == recipe => Ok(None),
            Some(known) => Ok(Some(known)),
            None => {
                self.set_meta("embedding_recipe", recipe).await?;
                Ok(None)
            }
        }
    }

    /// The Qdrant collection namespace of this file: `""` when none was set.
    ///
    /// INTENT: absent means empty so that a file written before namespaces
    /// existed keeps the collections it already has (`<prefix><project>`).
    pub async fn vector_namespace(&self) -> Result<String> {
        Ok(self
            .get_meta(VECTOR_NAMESPACE_KEY)
            .await?
            .unwrap_or_default())
    }

    /// Records the Qdrant collection namespace of this file, refusing one that
    /// could not appear in a collection name.
    pub async fn set_vector_namespace(&self, namespace: &str) -> Result<()> {
        crate::vectors::qdrant::check_namespace(namespace)?;
        self.set_meta(VECTOR_NAMESPACE_KEY, namespace).await
    }
}

/// The recipe a bucket file records, read without creating or migrating it.
/// No statement here writes; the only write SQLite itself may make is folding
/// back a journal a crashed writer left, which any open of the file does.
///
/// A missing file is `NotFound`; a file with no `meta` table (never migrated,
/// or not ours) has no recipe and answers `Ok(None)`. Blocking — up to `busy`
/// when a writer holds the file — so async callers run it on the blocking
/// pool.
///
/// INTENT: opened read-write *without* `CREATE`, with `query_only` on, rather
/// than read-only. A read-only connection to a WAL database creates its
/// `-wal`/`-shm` and cannot delete them when it closes, so every peek would
/// leave journals beside a closed file — and a journal beside a closed file is
/// precisely how adoption recognizes a file another process holds. Read-only
/// remains the fallback for a file that cannot be opened for writing.
pub fn read_recipe_no_create(path: &Path, busy: Duration) -> Result<Option<String>> {
    peek(path, busy, read_recipe)
}

/// The project ids a bucket file holds, read the way
/// [`read_recipe_no_create`] reads its recipe. A file with no `projects` table
/// holds none.
pub fn read_project_ids_no_create(path: &Path, busy: Duration) -> Result<Vec<String>> {
    peek(path, busy, |conn| {
        if !has_table(conn, "projects")? {
            return Ok(Vec::new());
        }
        let mut stmt = conn
            .prepare("SELECT id FROM projects ORDER BY id")
            .map_err(schema::sql)?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(schema::sql)?;
        ids.collect::<std::result::Result<_, _>>()
            .map_err(schema::sql)
    })
}

/// Runs one read on a file that must exist, opened without `CREATE`, with
/// `query_only` on, and closed before returning.
fn peek<T>(path: &Path, busy: Duration, read: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    if !path.is_file() {
        return Err(Error::NotFound(format!(
            "no bucket file at {}",
            path.display()
        )));
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .or_else(|_| {
        Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
    })
    .map_err(schema::sql)?;
    conn.busy_timeout(busy).map_err(schema::sql)?;
    conn.pragma_update(None, "query_only", true)
        .map_err(schema::sql)?;
    let value = read(&conn)?;
    conn.close()
        .map_err(|(_, e)| Error::Store(format!("cannot close {}: {e}", path.display())))?;
    Ok(value)
}

fn has_table(conn: &Connection, name: &str) -> Result<bool> {
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [name],
            |r| r.get(0),
        )
        .map_err(schema::sql)?;
    Ok(n > 0)
}

/// `meta.embedding_recipe`, or `None` when the file has no `meta` table.
pub(crate) fn read_recipe(conn: &Connection) -> Result<Option<String>> {
    if !has_table(conn, "meta")? {
        return Ok(None);
    }
    let mut stmt = conn
        .prepare("SELECT value FROM meta WHERE key = 'embedding_recipe'")
        .map_err(schema::sql)?;
    let mut rows = stmt.query([]).map_err(schema::sql)?;
    match rows.next().map_err(schema::sql)? {
        Some(row) => Ok(Some(row.get::<_, String>(0).map_err(schema::sql)?)),
        None => Ok(None),
    }
}

/// Serializes a vector as little-endian `f32`.
pub fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// Parses a little-endian `f32` blob back into a vector.
pub fn blob_to_vec(blob: &[u8]) -> Result<Vec<f32>> {
    if !blob.len().is_multiple_of(4) {
        return Err(Error::Store(format!(
            "vector blob of {} bytes is not f32-aligned",
            blob.len()
        )));
    }
    // `as_chunks` hands back the whole 4-byte groups and whatever tail is left
    // over; the alignment check above is what makes that tail empty.
    let (words, _) = blob.as_chunks::<4>();
    Ok(words.iter().copied().map(f32::from_le_bytes).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUSY: Duration = Duration::from_secs(2);

    #[tokio::test]
    async fn meta_round_trips() {
        let cat = Bucket::open_in_memory().expect("bucket");
        assert_eq!(cat.get_meta("nope").await.expect("read"), None);
        cat.set_meta("k", "v").await.expect("write");
        cat.set_meta("k", "v2").await.expect("overwrite");
        assert_eq!(cat.get_meta("k").await.expect("read"), Some("v2".into()));
    }

    #[tokio::test]
    async fn a_recipe_change_is_reported_not_silently_accepted() {
        let cat = Bucket::open_in_memory().expect("bucket");
        assert_eq!(cat.claim_recipe("qwen3:1024").await.expect("claim"), None);
        assert_eq!(
            cat.claim_recipe("qwen3:1024").await.expect("re-claim"),
            None
        );
        assert_eq!(
            cat.claim_recipe("openai:3072").await.expect("claim"),
            Some("qwen3:1024".to_string())
        );
    }

    #[test]
    fn vector_blobs_round_trip() {
        let v = vec![1.0f32, -0.5, 0.25];
        let blob = vec_to_blob(&v);
        assert_eq!(blob_to_vec(&blob).expect("parse"), v);
        assert!(blob_to_vec(&[0u8; 3]).is_err());
    }

    /// The lock on the file is the close's to release, not the last clone's:
    /// a stray `Arc` held by a search or a finished job must neither keep the
    /// file locked nor keep reading a file the daemon has let go of.
    #[tokio::test]
    async fn a_closed_bucket_releases_its_file_while_a_clone_is_still_alive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bucket.db");
        let bucket = Bucket::open_file(&path, "work").expect("open");
        bucket.set_meta("k", "v").await.expect("write");
        let stray = bucket.clone();

        let report = bucket.close().await.expect("close");
        assert!(report.was_open);
        assert_eq!(report.incomplete_checkpoint, None);
        assert!(stray.is_closed());

        let err = stray
            .get_meta("k")
            .await
            .expect_err("closed for every clone");
        assert_eq!(err.code(), "not_found");
        assert!(
            err.to_string().contains("bucket `work` was closed"),
            "{err}"
        );

        // The point of the close: the file set goes while `stray` is alive.
        crate::files::layout::remove_file_set(&path, "findery bucket rm work").expect("removable");
        assert!(!path.exists());
        assert!(!journal::with_suffix(&path, "-wal").exists());
        drop(stray);
    }

    #[tokio::test]
    async fn closing_twice_is_a_no_op() {
        let bucket = Bucket::open_in_memory().expect("bucket");
        assert!(bucket.close().await.expect("first").was_open);
        assert_eq!(
            bucket.close().await.expect("second"),
            CloseReport::default()
        );
    }

    #[test]
    fn opening_a_missing_file_without_create_is_not_found_and_creates_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bucket.db");
        let err = match Bucket::open_existing(&path, "work") {
            Ok(_) => panic!("a missing file must not open"),
            Err(e) => e,
        };
        assert_eq!(err.code(), "not_found");
        assert!(!path.exists(), "nothing was created");
    }

    #[tokio::test]
    async fn an_existing_file_opens_without_create_and_serves() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bucket.db");
        {
            let b = Bucket::open_file(&path, "work").expect("create");
            b.set_meta("k", "v").await.expect("write");
            b.close().await.expect("close");
        }
        let b = Bucket::open_existing(&path, "work").expect("reopen");
        assert_eq!(b.get_meta("k").await.expect("read"), Some("v".into()));
    }

    #[tokio::test]
    async fn reading_a_recipe_never_creates_or_migrates_a_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("missing.db");
        let err = read_recipe_no_create(&missing, BUSY).expect_err("missing");
        assert_eq!(err.code(), "not_found");
        assert!(!missing.exists(), "the read created nothing");

        // A SQLite file that was never migrated stays unmigrated.
        let bare = dir.path().join("bare.db");
        {
            let conn = Connection::open(&bare).expect("create");
            conn.execute_batch("CREATE TABLE other (x)")
                .expect("schema");
        }
        assert_eq!(read_recipe_no_create(&bare, BUSY).expect("read"), None);
        let tables: i64 = Connection::open(&bare)
            .expect("open")
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'meta'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(tables, 0, "the read did not migrate the file");

        let real = dir.path().join("bucket.db");
        let b = Bucket::open_file(&real, "default").expect("create");
        b.claim_recipe("mock:8").await.expect("claim");
        // Read while the daemon's own connection is still open on it.
        assert_eq!(
            read_recipe_no_create(&real, BUSY).expect("read"),
            Some("mock:8".into())
        );
    }

    /// A closed file is peeked at without leaving journals beside it: a stray
    /// `-shm` reads as "another connection holds it" to adoption.
    #[tokio::test]
    async fn a_closed_file_is_peeked_at_without_leaving_journals() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bucket.db");
        let b = Bucket::open_file(&path, "work").expect("create");
        b.claim_recipe("mock:8").await.expect("claim");
        b.close().await.expect("close");

        assert_eq!(
            read_recipe_no_create(&path, BUSY).expect("read"),
            Some("mock:8".into())
        );
        assert!(
            read_project_ids_no_create(&path, BUSY)
                .expect("ids")
                .is_empty()
        );
        for suffix in ["-wal", "-shm"] {
            assert!(
                !crate::files::journal::with_suffix(&path, suffix).exists(),
                "{suffix}"
            );
        }
    }

    #[test]
    fn project_ids_are_read_without_creating_and_a_bare_file_holds_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("missing.db");
        assert_eq!(
            read_project_ids_no_create(&missing, BUSY).map_err(|e| e.code()),
            Err("not_found")
        );
        assert!(!missing.exists());

        let real = dir.path().join("bucket.db");
        drop(Bucket::open_file(&real, "work").expect("create"));
        Connection::open(&real)
            .expect("open")
            .execute_batch(
                "INSERT INTO projects (id, name, spec_json, host_root, created_at, updated_at)
                 VALUES ('b', 'b', '{}', '/b', 0, 0), ('a', 'a', '{}', '/a', 0, 0)",
            )
            .expect("rows");
        assert_eq!(
            read_project_ids_no_create(&real, BUSY).expect("ids"),
            vec!["a".to_string(), "b".to_string()]
        );

        let bare = dir.path().join("bare.db");
        Connection::open(&bare)
            .expect("create")
            .execute_batch("CREATE TABLE other (x)")
            .expect("schema");
        assert!(
            read_project_ids_no_create(&bare, BUSY)
                .expect("ids")
                .is_empty()
        );
    }

    #[test]
    fn a_file_that_is_not_a_database_is_an_error_not_an_empty_recipe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let junk = dir.path().join("bucket.db");
        std::fs::write(
            &junk,
            b"definitely not sqlite, and long enough to have a header",
        )
        .expect("write");
        assert!(read_recipe_no_create(&junk, BUSY).is_err());
    }

    #[tokio::test]
    async fn the_vector_namespace_is_empty_until_set_and_refuses_bad_names() {
        let b = Bucket::open_in_memory().expect("bucket");
        assert_eq!(b.vector_namespace().await.expect("read"), "");
        b.set_vector_namespace("zvpmtsdf_").await.expect("set");
        assert_eq!(b.vector_namespace().await.expect("read"), "zvpmtsdf_");
        let err = b
            .set_vector_namespace("../x")
            .await
            .expect_err("not a name");
        assert_eq!(err.code(), "invalid");
        assert_eq!(b.vector_namespace().await.expect("read"), "zvpmtsdf_");
    }
}
