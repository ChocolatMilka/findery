//! The vocabulary migration: an install created before `catalog.db` became
//! `bucket.db` (and `catalogs/` became `buckets/`), moved onto the current
//! names.
//!
//! INTENT: split from `adopt.rs` (CONV-029), the younger generation of the
//! same idea. Like adoption it is "complete this rename if it is still owed",
//! idempotent and resumable, and it refuses to rename a database whose journal
//! still holds committed pages. Unlike adoption, a move it cannot make stops
//! the boot: nothing downstream ever looks at the old name again. Its tests sit
//! in `adopt.rs`, beside the journal fixture they share.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use findery_core::config::{StoreConfig, buckets_dir};
use findery_core::{Error, Result};

use crate::files::journal::{
    JOURNAL_SUFFIXES, checkpoint_truncate, journal_holds_pages, with_suffix,
};

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

/// Empties the write-ahead log by letting SQLite recover it.
///
/// A database with a live journal cannot be renamed safely: the rename is not
/// atomic across the file set, and a failure between the two moves detaches
/// committed pages from the database that owns them. SQLite is the one thing
/// that can put those pages back, so it is asked to — and its answer is
/// *checked*: a checkpoint a reader blocked is a successful statement whose
/// row says `busy`, and moving on from it moves a journal that still owns
/// pages.
fn consolidate(db: &Path) -> std::io::Result<()> {
    let conn = Connection::open(db).map_err(std::io::Error::other)?;
    let row = checkpoint_truncate(&conn).map_err(std::io::Error::other)?;
    if !row.complete() {
        return Err(std::io::Error::other(row.describe(db)));
    }
    conn.close().map_err(|(_, e)| std::io::Error::other(e))
}

/// The refusal a half-migrated install gets. It names both paths and the manual
/// move, because the cause is usually transient — a held file, a read-only
/// volume — and the user's next move is to fix it and retry.
fn stranded(from: &Path, to: &Path, cause: std::io::Error) -> Error {
    Error::Store(format!(
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
    for suffix in JOURNAL_SUFFIXES {
        let source = with_suffix(from, suffix);
        if source.is_file() {
            std::fs::rename(&source, with_suffix(to, suffix))?;
        }
    }
    Ok(())
}
