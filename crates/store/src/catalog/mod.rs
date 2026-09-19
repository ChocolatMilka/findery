//! The catalog: the durable record of what is indexed.
//!
//! One SQLite file holds projects, files, chunks, the content-addressed vector
//! cache and the full-text index. The vector backend is swappable and always
//! rebuildable from here — this file is the one thing worth backing up.
//!
//! INTENT: rusqlite is synchronous, so every public method hops onto
//! `spawn_blocking` through [`Catalog::with_conn`]. A single mutex serializes
//! access, which is the right trade for a single-writer daemon: queries are
//! indexed lookups, and vector search never touches this connection at all.

mod cache;
mod chunks;
mod projects;

use findery_core::{Error, Result};
use parking_lot::Mutex;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::schema;

/// Handle on the catalog database.
#[derive(Clone)]
pub struct Catalog {
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
}

impl Catalog {
    /// Opens (creating if needed) `dir/catalog.db` and migrates it.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("catalog.db");
        let conn = Connection::open(&path).map_err(schema::sql)?;
        schema::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path,
        })
    }

    /// Opens an in-memory catalog. Tests only — nothing survives the process.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(schema::sql)?;
        schema::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path: PathBuf::from(":memory:"),
        })
    }

    /// Path of the database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Runs a closure against the connection on the blocking pool.
    pub async fn with_conn<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock();
            f(&guard)
        })
        .await
        .map_err(|e| Error::Internal(format!("catalog task: {e}")))?
    }

    /// Bytes the database occupies on disk, WAL included.
    pub async fn disk_bytes(&self) -> u64 {
        let base = self.path.clone();
        let mut total = 0u64;
        for suffix in ["", "-wal", "-shm"] {
            let p = if suffix.is_empty() {
                base.clone()
            } else {
                PathBuf::from(format!("{}{suffix}", base.display()))
            };
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

    /// Refuses to run against a catalog built with a different embedding
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
    Ok(blob
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn meta_round_trips() {
        let cat = Catalog::open_in_memory().expect("catalog");
        assert_eq!(cat.get_meta("nope").await.expect("read"), None);
        cat.set_meta("k", "v").await.expect("write");
        cat.set_meta("k", "v2").await.expect("overwrite");
        assert_eq!(cat.get_meta("k").await.expect("read"), Some("v2".into()));
    }

    #[tokio::test]
    async fn a_recipe_change_is_reported_not_silently_accepted() {
        let cat = Catalog::open_in_memory().expect("catalog");
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
}
