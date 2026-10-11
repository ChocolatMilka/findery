//! The content-addressed vector cache, and the `vectors` table backing the
//! SQLite vector store.
//!
//! INTENT: `embeddings` is keyed by `(recipe, content_hash)` and by nothing
//! else. Re-run a sync, re-clone the repository, reorder a file — the embedded
//! text is identical, so the vector is reused and the GPU stays idle. This
//! single table is what makes a re-sync of an unchanged repository free.
//!
//! The key does NOT survive a change of LOCATION: `content_hash` covers the
//! text that is embedded, which opens on `path: …`. Moving a function to
//! another file re-embeds it, and on purpose — the path is signal.

use super::{Bucket, blob_to_vec, vec_to_blob};
use crate::schema::sql;
use findery_core::Result;
use findery_core::clock::now_unix;
use findery_core::ids::{ChunkId, ContentHash, ProjectId};
use findery_core::model::{ChunkKind, Lang};
use findery_core::traits::{VectorPayload, VectorRecord};
use rusqlite::params;
use std::collections::HashMap;

/// Hashes per `IN (...)` batch.
const HASH_BATCH: usize = 400;

impl Bucket {
    /// Looks up already-computed vectors for the given content hashes.
    pub async fn cached_vectors(
        &self,
        recipe: &str,
        hashes: Vec<ContentHash>,
    ) -> Result<HashMap<String, Vec<f32>>> {
        if hashes.is_empty() {
            return Ok(HashMap::new());
        }
        let recipe = recipe.to_string();
        self.with_conn(move |c| {
            let mut out = HashMap::with_capacity(hashes.len());
            for batch in hashes.chunks(HASH_BATCH) {
                let owned: Vec<String> = batch.iter().map(|h| h.to_string()).collect();
                let holes = (0..owned.len())
                    .map(|i| format!("?{}", i + 2))
                    .collect::<Vec<_>>()
                    .join(", ");
                let query = format!(
                    "SELECT content_hash, vec FROM embeddings
                     WHERE recipe = ?1 AND content_hash IN ({holes})"
                );
                let mut stmt = c.prepare(&query).map_err(sql)?;
                let mut binds: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(owned.len() + 1);
                binds.push(&recipe);
                for h in &owned {
                    binds.push(h);
                }
                let mut rows = stmt.query(binds.as_slice()).map_err(sql)?;
                while let Some(row) = rows.next().map_err(sql)? {
                    let hash: String = row.get(0).map_err(sql)?;
                    let blob: Vec<u8> = row.get(1).map_err(sql)?;
                    out.insert(hash, blob_to_vec(&blob)?);
                }
            }
            Ok(out)
        })
        .await
    }

    /// Stores freshly computed vectors under the current recipe.
    pub async fn put_cached_vectors(
        &self,
        recipe: &str,
        items: Vec<(ContentHash, Vec<f32>)>,
    ) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let recipe = recipe.to_string();
        let now = now_unix();
        self.with_conn(move |c| {
            let tx = c.unchecked_transaction().map_err(sql)?;
            {
                let mut stmt = tx
                    .prepare(
                        "INSERT INTO embeddings (recipe, content_hash, dim, vec, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT(recipe, content_hash) DO NOTHING",
                    )
                    .map_err(sql)?;
                for (hash, vector) in &items {
                    stmt.execute(params![
                        recipe,
                        hash.as_str(),
                        vector.len() as i64,
                        vec_to_blob(vector),
                        now
                    ])
                    .map_err(sql)?;
                }
            }
            tx.commit().map_err(sql)?;
            Ok(())
        })
        .await
    }

    /// Drops cached vectors no live chunk refers to any more.
    ///
    /// Not run automatically: the cache is exactly what makes a moved file free
    /// to re-index, so collecting it eagerly would defeat its purpose.
    pub async fn gc_embeddings(&self) -> Result<u64> {
        self.with_conn(move |c| {
            let n = c
                .execute(
                    "DELETE FROM embeddings
                     WHERE content_hash NOT IN (SELECT DISTINCT content_hash FROM chunks)",
                    [],
                )
                .map_err(sql)?;
            Ok(n as u64)
        })
        .await
    }

    /// Number of rows in the cache, and the bytes they hold.
    pub async fn cache_stats(&self) -> Result<(u64, u64)> {
        self.with_conn(move |c| {
            let rows: i64 = c
                .query_row("SELECT count(*) FROM embeddings", [], |r| r.get(0))
                .map_err(sql)?;
            let bytes: i64 = c
                .query_row(
                    "SELECT coalesce(sum(length(vec)), 0) FROM embeddings",
                    [],
                    |r| r.get(0),
                )
                .map_err(sql)?;
            Ok((rows.max(0) as u64, bytes.max(0) as u64))
        })
        .await
    }

    // -----------------------------------------------------------------------
    // `vectors` table — live vectors of the SQLite vector backend
    // -----------------------------------------------------------------------

    /// Loads every live vector of a project, for a cold matrix rebuild.
    pub async fn load_vectors(
        &self,
        project: &ProjectId,
    ) -> Result<Vec<(ChunkId, VectorPayload, Vec<f32>)>> {
        let pid = project.to_string();
        self.with_conn(move |c| {
            let mut stmt = c
                .prepare(
                    "SELECT chunk_id, vec, path, lang, kind, symbol
                     FROM vectors WHERE project_id = ?1",
                )
                .map_err(sql)?;
            let mut rows = stmt.query([&pid]).map_err(sql)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(sql)? {
                let id: String = row.get(0).map_err(sql)?;
                let blob: Vec<u8> = row.get(1).map_err(sql)?;
                let lang: String = row.get(3).map_err(sql)?;
                let kind: String = row.get(4).map_err(sql)?;
                let payload = VectorPayload {
                    path: row.get(2).map_err(sql)?,
                    lang: Lang::parse(&lang).unwrap_or(Lang::Other),
                    kind: ChunkKind::parse(&kind).unwrap_or(ChunkKind::Text),
                    symbol: row.get(5).map_err(sql)?,
                };
                out.push((ChunkId::from_hex(id), payload, blob_to_vec(&blob)?));
            }
            Ok(out)
        })
        .await
    }

    /// Writes vectors for the SQLite backend.
    pub async fn put_vectors(&self, project: &ProjectId, records: Vec<VectorRecord>) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let pid = project.to_string();
        self.with_conn(move |c| {
            let tx = c.unchecked_transaction().map_err(sql)?;
            {
                let mut stmt = tx
                    .prepare(
                        "INSERT INTO vectors
                             (project_id, chunk_id, dim, vec, path, lang, kind, symbol)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                         ON CONFLICT(project_id, chunk_id) DO UPDATE SET
                             dim = excluded.dim, vec = excluded.vec, path = excluded.path,
                             lang = excluded.lang, kind = excluded.kind, symbol = excluded.symbol",
                    )
                    .map_err(sql)?;
                for r in &records {
                    stmt.execute(params![
                        pid,
                        r.id.as_str(),
                        r.vector.len() as i64,
                        vec_to_blob(&r.vector),
                        r.payload.path,
                        r.payload.lang.as_str(),
                        r.payload.kind.as_str(),
                        r.payload.symbol,
                    ])
                    .map_err(sql)?;
                }
            }
            tx.commit().map_err(sql)?;
            Ok(())
        })
        .await
    }

    /// Evicts vectors by chunk id.
    pub async fn delete_vectors(&self, project: &ProjectId, ids: Vec<ChunkId>) -> Result<u64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let pid = project.to_string();
        self.with_conn(move |c| {
            let tx = c.unchecked_transaction().map_err(sql)?;
            let mut removed = 0u64;
            for batch in ids.chunks(HASH_BATCH) {
                let owned: Vec<String> = batch.iter().map(|i| i.to_string()).collect();
                let holes = (0..owned.len())
                    .map(|i| format!("?{}", i + 2))
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut binds: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(owned.len() + 1);
                binds.push(&pid);
                for id in &owned {
                    binds.push(id);
                }
                removed += tx
                    .execute(
                        &format!(
                            "DELETE FROM vectors WHERE project_id = ?1 AND chunk_id IN ({holes})"
                        ),
                        binds.as_slice(),
                    )
                    .map_err(sql)? as u64;
            }
            tx.commit().map_err(sql)?;
            Ok(removed)
        })
        .await
    }

    /// Drops every vector of a project.
    pub async fn clear_vectors(&self, project: &ProjectId) -> Result<()> {
        let pid = project.to_string();
        self.with_conn(move |c| {
            c.execute("DELETE FROM vectors WHERE project_id = ?1", [&pid])
                .map_err(sql)?;
            Ok(())
        })
        .await
    }

    /// Live vector count of a project.
    pub async fn count_vectors(&self, project: &ProjectId) -> Result<u64> {
        let pid = project.to_string();
        self.with_conn(move |c| {
            let n: i64 = c
                .query_row(
                    "SELECT count(*) FROM vectors WHERE project_id = ?1",
                    [&pid],
                    |r| r.get(0),
                )
                .map_err(sql)?;
            Ok(n.max(0) as u64)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(path: &str) -> VectorPayload {
        VectorPayload {
            path: path.into(),
            lang: Lang::Rust,
            kind: ChunkKind::Function,
            symbol: Some("f".into()),
        }
    }

    #[tokio::test]
    async fn the_cache_is_keyed_by_recipe_and_content_only() {
        let cat = Bucket::open_in_memory().expect("bucket");
        let h = ContentHash::of(b"fn main() {}");
        cat.put_cached_vectors("qwen3:4", vec![(h.clone(), vec![1.0, 0.0, 0.0, 0.0])])
            .await
            .expect("put");

        let hit = cat
            .cached_vectors("qwen3:4", vec![h.clone()])
            .await
            .expect("get");
        assert_eq!(hit.get(h.as_str()), Some(&vec![1.0, 0.0, 0.0, 0.0]));

        // A different recipe is a different namespace, never a stale reuse.
        let miss = cat.cached_vectors("openai:4", vec![h]).await.expect("get");
        assert!(miss.is_empty());
    }

    #[tokio::test]
    async fn gc_only_removes_unreferenced_vectors() {
        let cat = Bucket::open_in_memory().expect("bucket");
        let h = ContentHash::of(b"orphan");
        cat.put_cached_vectors("r", vec![(h, vec![0.0; 4])])
            .await
            .expect("put");
        assert_eq!(cat.cache_stats().await.expect("stats").0, 1);
        assert_eq!(cat.gc_embeddings().await.expect("gc"), 1);
        assert_eq!(cat.cache_stats().await.expect("stats").0, 0);
    }

    #[tokio::test]
    async fn live_vectors_upsert_delete_and_survive_a_reload() {
        let cat = Bucket::open_in_memory().expect("bucket");
        let p = ProjectId::new("demo").expect("id");
        let id = ChunkId::derive(&p, "a.rs", 0);

        cat.put_vectors(
            &p,
            vec![VectorRecord {
                id: id.clone(),
                vector: vec![1.0, 0.0],
                payload: payload("a.rs"),
            }],
        )
        .await
        .expect("put");
        cat.put_vectors(
            &p,
            vec![VectorRecord {
                id: id.clone(),
                vector: vec![0.0, 1.0],
                payload: payload("b.rs"),
            }],
        )
        .await
        .expect("upsert");

        let loaded = cat.load_vectors(&p).await.expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].1.path, "b.rs");
        assert_eq!(loaded[0].2, vec![0.0, 1.0]);

        assert_eq!(cat.delete_vectors(&p, vec![id]).await.expect("delete"), 1);
        assert_eq!(cat.count_vectors(&p).await.expect("count"), 0);
    }
}
