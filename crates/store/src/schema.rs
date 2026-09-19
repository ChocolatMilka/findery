//! Catalog schema and forward-only migrations.
//!
//! INTENT: the catalog is the durable truth about what is indexed. The vector
//! backend is swappable and rebuildable; this file is not. Migrations are
//! append-only and numbered, and `user_version` is the single source of truth
//! for where a database stands.

use findery_core::{Error, Result};
use rusqlite::Connection;

/// Schema version this binary writes. Bump it and append a migration.
pub const SCHEMA_VERSION: i64 = 1;

/// Statements applied to reach version 1 from an empty database.
const V1: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS projects (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    spec_json    TEXT NOT NULL,
    host_root    TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    last_sync_at INTEGER
);

CREATE TABLE IF NOT EXISTS files (
    project_id   TEXT NOT NULL,
    path         TEXT NOT NULL,
    lang         TEXT NOT NULL,
    origin       TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    size         INTEGER NOT NULL,
    mtime_ns     INTEGER NOT NULL,
    chunk_count  INTEGER NOT NULL,
    indexed_at   INTEGER NOT NULL,
    PRIMARY KEY (project_id, path)
);

CREATE TABLE IF NOT EXISTS chunks (
    id           TEXT PRIMARY KEY,
    project_id   TEXT NOT NULL,
    path         TEXT NOT NULL,
    lang         TEXT NOT NULL,
    ordinal      INTEGER NOT NULL,
    kind         TEXT NOT NULL,
    symbol       TEXT,
    breadcrumb   TEXT,
    start_line   INTEGER NOT NULL,
    end_line     INTEGER NOT NULL,
    content_hash TEXT NOT NULL,
    text         TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS chunks_by_file    ON chunks (project_id, path);
CREATE INDEX IF NOT EXISTS chunks_by_project ON chunks (project_id);

-- Content-addressed vector cache, shared by every project on this host.
-- `recipe` is `<model>:<dim>`: changing either one starts a fresh namespace
-- instead of silently reusing vectors of another shape.
CREATE TABLE IF NOT EXISTS embeddings (
    recipe       TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    dim          INTEGER NOT NULL,
    vec          BLOB NOT NULL,
    created_at   INTEGER NOT NULL,
    PRIMARY KEY (recipe, content_hash)
);

-- Live vectors, used only by the `sqlite` vector backend.
CREATE TABLE IF NOT EXISTS vectors (
    project_id TEXT NOT NULL,
    chunk_id   TEXT NOT NULL,
    dim        INTEGER NOT NULL,
    vec        BLOB NOT NULL,
    path       TEXT NOT NULL,
    lang       TEXT NOT NULL,
    kind       TEXT NOT NULL,
    symbol     TEXT,
    PRIMARY KEY (project_id, chunk_id)
);

CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5 (
    id         UNINDEXED,
    project_id UNINDEXED,
    text,
    symbol,
    path,
    tokenize = 'unicode61 remove_diacritics 2'
);
"#;

/// Opens the connection's pragmas and migrates it up to [`SCHEMA_VERSION`].
pub fn migrate(conn: &Connection) -> Result<()> {
    // WAL keeps readers from blocking the sync writer; NORMAL is the right
    // durability point for an index that can always be rebuilt from source.
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(sql)?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(sql)?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(sql)?;
    conn.busy_timeout(std::time::Duration::from_secs(10))
        .map_err(sql)?;

    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(sql)?;
    if version > SCHEMA_VERSION {
        return Err(Error::Store(format!(
            "catalog schema is version {version}, this binary understands {SCHEMA_VERSION}; \
             upgrade findery or point FINDERY_DATA_DIR elsewhere"
        )));
    }
    if version < 1 {
        conn.execute_batch(V1).map_err(sql)?;
    }
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(sql)?;
    Ok(())
}

/// Maps a rusqlite failure into the workspace error type.
pub fn sql(e: rusqlite::Error) -> Error {
    Error::Store(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrating_twice_is_a_no_op() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        migrate(&conn).expect("first migration");
        migrate(&conn).expect("second migration");
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("version");
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn a_future_schema_is_refused_rather_than_corrupted() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .expect("bump");
        let err = migrate(&conn).expect_err("must refuse");
        assert_eq!(err.code(), "store");
    }

    #[test]
    fn full_text_search_is_available() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        migrate(&conn).expect("migration");
        conn.execute(
            "INSERT INTO chunks_fts (id, project_id, text, symbol, path) VALUES (?, ?, ?, ?, ?)",
            rusqlite::params!["c1", "p", "fn retry_with_backoff()", "retry", "src/net.rs"],
        )
        .expect("insert");
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'retry'",
                [],
                |r| r.get(0),
            )
            .expect("match");
        assert_eq!(n, 1);
    }
}
