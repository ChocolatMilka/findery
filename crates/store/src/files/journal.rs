//! The SQLite file set: a database, the `-wal` and `-shm` files named after it,
//! and the checkpoint that folds the first back into the database.
//!
//! INTENT: three callers move or close a database — legacy adoption, the
//! vocabulary migration and [`crate::Bucket::close`] — and each one has to
//! answer the same two questions: *is every committed page in the main file
//! yet?* and *did that file fail to move because something holds it?* One
//! answer for each, here, so a checkpoint whose `busy` column is ignored (the
//! bug this module exists to retire) cannot come back through a fourth caller.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

/// The files SQLite derives from a database's own name.
pub(crate) const JOURNAL_SUFFIXES: [&str; 2] = ["-wal", "-shm"];

/// `path` with `suffix` appended to its file name — `bucket.db` → `bucket.db-wal`.
pub(crate) fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// True when the journal beside `db` still holds committed pages.
pub(crate) fn journal_holds_pages(db: &Path) -> bool {
    std::fs::metadata(with_suffix(db, "-wal")).is_ok_and(|m| m.len() > 0)
}

/// The row `PRAGMA wal_checkpoint(TRUNCATE)` answers with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CheckpointRow {
    /// 1 when a reader or writer kept the checkpoint from completing.
    pub busy: i64,
    /// Frames in the log, or -1 when the database is not in WAL mode.
    pub log: i64,
    /// Frames copied back into the database, or -1 likewise.
    pub checkpointed: i64,
}

impl CheckpointRow {
    /// Every committed page is in the main file. A database outside WAL mode
    /// answers `(0, -1, -1)`, which is complete by definition: it has no log.
    pub fn complete(&self) -> bool {
        self.busy == 0 && self.log == self.checkpointed
    }

    /// What an incomplete checkpoint left behind, for a report.
    pub fn describe(&self, db: &Path) -> String {
        format!(
            "the journal of {} could not be folded back (busy {}, {} of {} frames copied): \
             another connection is reading or writing it",
            db.display(),
            self.busy,
            self.checkpointed.max(0),
            self.log.max(0)
        )
    }
}

/// Folds the write-ahead log back into the database and truncates it.
///
/// The row is returned, never discarded: SQLite reports a checkpoint blocked
/// by a reader as a *successful* statement with `busy = 1`, so a caller that
/// only looks at the `Result` believes pages are home when they are not.
pub(crate) fn checkpoint_truncate(conn: &Connection) -> rusqlite::Result<CheckpointRow> {
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
        Ok(CheckpointRow {
            busy: r.get(0)?,
            log: r.get(1)?,
            checkpointed: r.get(2)?,
        })
    })
}

/// True when a filesystem call failed because another handle holds the file,
/// as opposed to the file being absent or the path being wrong.
///
/// Windows reports a file open elsewhere as a sharing violation (os error 32)
/// or a lock violation (33), and an open SQLite file being renamed or deleted
/// frequently as access denied (5). Unix reports nothing for an open file, so
/// there only a read-only volume or a permission lands here — both of which
/// the user fixes the same way: release or unlock, then retry.
pub(crate) fn is_held(e: &std::io::Error) -> bool {
    // The raw codes are Windows' own: on Unix 32 is EPIPE, which no file call
    // returns and which would mean something else entirely.
    (cfg!(windows) && matches!(e.raw_os_error(), Some(32 | 33)))
        || matches!(
            e.kind(),
            std::io::ErrorKind::PermissionDenied
                | std::io::ErrorKind::ResourceBusy
                | std::io::ErrorKind::ReadOnlyFilesystem
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_names_are_derived_from_the_database_name() {
        let db = Path::new("dir/bucket.db");
        assert_eq!(with_suffix(db, "-wal"), PathBuf::from("dir/bucket.db-wal"));
        assert_eq!(with_suffix(db, "-shm"), PathBuf::from("dir/bucket.db-shm"));
    }

    #[test]
    fn a_checkpoint_blocked_by_a_reader_is_reported_incomplete() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("b.db");
        let writer = Connection::open(&path).expect("open");
        writer
            .pragma_update(None, "journal_mode", "WAL")
            .expect("wal");
        writer
            .execute_batch("CREATE TABLE t (x); INSERT INTO t VALUES (1);")
            .expect("write");

        let reader = Connection::open(&path).expect("open reader");
        reader
            .execute_batch("BEGIN; SELECT count(*) FROM t;")
            .expect("hold a snapshot");
        writer
            .execute("INSERT INTO t VALUES (2)", [])
            .expect("frames the reader does not see");

        let row = checkpoint_truncate(&writer).expect("the statement itself succeeds");
        assert!(!row.complete(), "{row:?}");
        assert!(row.describe(&path).contains("b.db"));

        reader.execute_batch("COMMIT").expect("release");
        let row = checkpoint_truncate(&writer).expect("checkpoint");
        assert!(row.complete(), "{row:?}");
    }

    #[test]
    fn a_database_outside_wal_mode_checkpoints_complete() {
        let conn = Connection::open_in_memory().expect("open");
        let row = checkpoint_truncate(&conn).expect("checkpoint");
        assert!(row.complete(), "{row:?}");
    }

    #[test]
    fn a_missing_file_is_not_a_held_file() {
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(!is_held(&missing));
        assert!(is_held(&std::io::Error::from_raw_os_error(32)) || !cfg!(windows));
        assert!(is_held(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
    }
}
