//! Chunk rows and the full-text index that shadows them.
//!
//! INTENT: `chunks` and `chunks_fts` are written in the same transaction, on
//! purpose. A keyword index that lags the chunk table returns hits pointing at
//! text that no longer exists, which is worse than no keyword leg at all.

use super::Bucket;
use crate::schema::sql;
use findery_core::Result;
use findery_core::ids::{ChunkId, ContentHash, ProjectId};
use findery_core::model::{Chunk, ChunkKind, Lang, ScoredChunk};
use rusqlite::{Connection, Row, params};

/// Ids per `IN (...)` batch. SQLite's default variable limit is 999.
const ID_BATCH: usize = 400;

impl Bucket {
    /// Replaces every chunk of one file, returning the ids that disappeared.
    ///
    /// The caller uses that list to evict the matching vectors: the bucket
    /// never reaches into the vector store itself.
    pub async fn replace_file_chunks(
        &self,
        project: &ProjectId,
        path: &str,
        chunks: Vec<Chunk>,
    ) -> Result<Vec<ChunkId>> {
        let (pid, path) = (project.to_string(), path.to_string());
        self.with_conn(move |c| {
            let tx = c.unchecked_transaction().map_err(sql)?;
            let previous = chunk_ids_of_file(&tx, &pid, &path)?;
            delete_chunks(&tx, &pid, &previous)?;
            for chunk in &chunks {
                insert_chunk(&tx, &pid, chunk)?;
            }
            tx.commit().map_err(sql)?;

            let kept: std::collections::HashSet<&str> =
                chunks.iter().map(|c| c.id.as_str()).collect();
            Ok(previous
                .into_iter()
                .filter(|id| !kept.contains(id.as_str()))
                .collect())
        })
        .await
    }

    /// Removes every chunk of one file, returning their ids.
    pub async fn delete_file_chunks(
        &self,
        project: &ProjectId,
        path: &str,
    ) -> Result<Vec<ChunkId>> {
        let (pid, path) = (project.to_string(), path.to_string());
        self.with_conn(move |c| {
            let tx = c.unchecked_transaction().map_err(sql)?;
            let ids = chunk_ids_of_file(&tx, &pid, &path)?;
            delete_chunks(&tx, &pid, &ids)?;
            tx.commit().map_err(sql)?;
            Ok(ids)
        })
        .await
    }

    /// Loads chunks by id, in the order requested; unknown ids are skipped.
    pub async fn get_chunks(&self, project: &ProjectId, ids: &[ChunkId]) -> Result<Vec<Chunk>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let pid = project.to_string();
        let wanted: Vec<String> = ids.iter().map(|i| i.to_string()).collect();
        self.with_conn(move |c| {
            let mut found = std::collections::HashMap::new();
            for batch in wanted.chunks(ID_BATCH) {
                let holes = placeholders(batch.len());
                let query = format!("{CHUNK_SELECT} WHERE project_id = ?1 AND id IN ({holes})");
                let mut stmt = c.prepare(&query).map_err(sql)?;
                let mut binds: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(batch.len() + 1);
                binds.push(&pid);
                for id in batch {
                    binds.push(id);
                }
                let mut rows = stmt.query(binds.as_slice()).map_err(sql)?;
                while let Some(row) = rows.next().map_err(sql)? {
                    let chunk = read_chunk(row)?;
                    found.insert(chunk.id.to_string(), chunk);
                }
            }
            Ok(wanted.iter().filter_map(|id| found.remove(id)).collect())
        })
        .await
    }

    /// Every chunk of a project, streamed in batches. Used by the exporter and
    /// by a cold rebuild of the in-memory vector matrix.
    pub async fn all_chunks(&self, project: &ProjectId) -> Result<Vec<Chunk>> {
        let pid = project.to_string();
        self.with_conn(move |c| {
            let query = format!("{CHUNK_SELECT} WHERE project_id = ?1 ORDER BY path, ordinal");
            let mut stmt = c.prepare(&query).map_err(sql)?;
            let mut rows = stmt.query([&pid]).map_err(sql)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(sql)? {
                out.push(read_chunk(row)?);
            }
            Ok(out)
        })
        .await
    }

    /// BM25 keyword search over the chunk text, symbols and paths.
    ///
    /// Scores are returned positive and descending: `bm25()` is negative and
    /// ascending, which would silently invert any fusion built on top.
    pub async fn keyword_search(
        &self,
        project: &ProjectId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ScoredChunk>> {
        let Some(match_expr) = fts_query(query) else {
            return Ok(Vec::new());
        };
        let pid = project.to_string();
        self.with_conn(move |c| {
            let mut stmt = c
                .prepare(
                    "SELECT id, bm25(chunks_fts, 1.0, 4.0, 2.0) AS rank
                     FROM chunks_fts
                     WHERE chunks_fts MATCH ?1 AND project_id = ?2
                     ORDER BY rank
                     LIMIT ?3",
                )
                .map_err(sql)?;
            let mut rows = stmt
                .query(params![match_expr, pid, limit as i64])
                .map_err(sql)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(sql)? {
                let id: String = row.get(0).map_err(sql)?;
                let rank: f64 = row.get(1).map_err(sql)?;
                out.push(ScoredChunk {
                    id: ChunkId::from_hex(id),
                    score: -rank as f32,
                });
            }
            Ok(out)
        })
        .await
    }
}

// ---------------------------------------------------------------------------

const CHUNK_SELECT: &str = "SELECT id, project_id, path, lang, ordinal, kind, symbol, breadcrumb, \
                            start_line, end_line, content_hash, text FROM chunks";

fn chunk_ids_of_file(c: &Connection, project: &str, path: &str) -> Result<Vec<ChunkId>> {
    let mut stmt = c
        .prepare("SELECT id FROM chunks WHERE project_id = ?1 AND path = ?2")
        .map_err(sql)?;
    let mut rows = stmt.query(params![project, path]).map_err(sql)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(sql)? {
        let id: String = row.get(0).map_err(sql)?;
        out.push(ChunkId::from_hex(id));
    }
    Ok(out)
}

fn delete_chunks(c: &Connection, project: &str, ids: &[ChunkId]) -> Result<()> {
    for batch in ids.chunks(ID_BATCH) {
        let holes = placeholders(batch.len());
        let owned: Vec<String> = batch.iter().map(|i| i.to_string()).collect();
        let mut binds: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(owned.len() + 1);
        binds.push(&project);
        for id in &owned {
            binds.push(id);
        }
        c.execute(
            &format!("DELETE FROM chunks WHERE project_id = ?1 AND id IN ({holes})"),
            binds.as_slice(),
        )
        .map_err(sql)?;
        c.execute(
            &format!("DELETE FROM chunks_fts WHERE project_id = ?1 AND id IN ({holes})"),
            binds.as_slice(),
        )
        .map_err(sql)?;
    }
    Ok(())
}

fn insert_chunk(c: &Connection, project: &str, chunk: &Chunk) -> Result<()> {
    c.execute(
        "INSERT INTO chunks
             (id, project_id, path, lang, ordinal, kind, symbol, breadcrumb,
              start_line, end_line, content_hash, text)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(id) DO UPDATE SET
             path = excluded.path, lang = excluded.lang, ordinal = excluded.ordinal,
             kind = excluded.kind, symbol = excluded.symbol, breadcrumb = excluded.breadcrumb,
             start_line = excluded.start_line, end_line = excluded.end_line,
             content_hash = excluded.content_hash, text = excluded.text",
        params![
            chunk.id.as_str(),
            project,
            chunk.path,
            chunk.lang.as_str(),
            chunk.ordinal as i64,
            chunk.kind.as_str(),
            chunk.symbol,
            chunk.breadcrumb,
            chunk.start_line as i64,
            chunk.end_line as i64,
            chunk.content_hash.as_str(),
            chunk.text,
        ],
    )
    .map_err(sql)?;
    c.execute(
        "INSERT INTO chunks_fts (id, project_id, text, symbol, path) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            chunk.id.as_str(),
            project,
            chunk.text,
            chunk
                .breadcrumb
                .clone()
                .or_else(|| chunk.symbol.clone())
                .unwrap_or_default(),
            chunk.path,
        ],
    )
    .map_err(sql)?;
    Ok(())
}

fn read_chunk(row: &Row<'_>) -> Result<Chunk> {
    let id: String = row.get(0).map_err(sql)?;
    let project: String = row.get(1).map_err(sql)?;
    let lang: String = row.get(3).map_err(sql)?;
    let kind: String = row.get(5).map_err(sql)?;
    let ordinal: i64 = row.get(4).map_err(sql)?;
    let start: i64 = row.get(8).map_err(sql)?;
    let end: i64 = row.get(9).map_err(sql)?;
    let hash: String = row.get(10).map_err(sql)?;
    Ok(Chunk {
        id: ChunkId::from_hex(id),
        project: ProjectId::new(project)?,
        path: row.get(2).map_err(sql)?,
        lang: Lang::parse(&lang).unwrap_or(Lang::Other),
        ordinal: ordinal.max(0) as u32,
        kind: ChunkKind::parse(&kind).unwrap_or(ChunkKind::Text),
        symbol: row.get(6).map_err(sql)?,
        breadcrumb: row.get(7).map_err(sql)?,
        start_line: start.max(0) as u32,
        end_line: end.max(0) as u32,
        content_hash: ContentHash::from_hex(hash),
        text: row.get(11).map_err(sql)?,
    })
}

fn placeholders(n: usize) -> String {
    (0..n)
        .map(|i| format!("?{}", i + 2))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Turns free-form text into a safe FTS5 MATCH expression.
///
/// INTENT: user text reaches this straight from an MCP tool call. Passing it
/// raw to FTS5 turns `foo:bar` or a stray `"` into a syntax error at best. We
/// keep identifier-ish tokens, quote each one, and OR them for recall — the
/// fusion step upstream is what turns recall back into precision.
pub fn fts_query(raw: &str) -> Option<String> {
    let tokens: Vec<String> = raw
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|t| t.len() >= 2)
        .take(24)
        .map(|t| format!("\"{}\"", t.to_lowercase()))
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" OR "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use findery_core::model::{Origin, Project, ProjectSpec};

    fn chunk(id_seed: u32, path: &str, text: &str) -> Chunk {
        let project = ProjectId::new("demo").expect("id");
        Chunk {
            id: ChunkId::derive(&project, path, id_seed),
            project,
            path: path.into(),
            lang: Lang::Rust,
            ordinal: id_seed,
            kind: ChunkKind::Function,
            symbol: Some("retry_with_backoff".into()),
            breadcrumb: Some("Net > retry_with_backoff".into()),
            start_line: 1,
            end_line: 9,
            content_hash: ContentHash::of(text.as_bytes()),
            text: text.into(),
        }
    }

    async fn bucket() -> (Bucket, ProjectId) {
        let cat = Bucket::open_in_memory().expect("bucket");
        let id = ProjectId::new("demo").expect("id");
        cat.put_project(&Project {
            id: id.clone(),
            name: "demo".into(),
            bucket: cat.name().to_string(),
            spec: ProjectSpec::default(),
            host_root: "C:/demo".into(),
            created_at: 1,
            updated_at: 1,
        })
        .await
        .expect("project");
        let _ = Origin::File;
        (cat, id)
    }

    #[tokio::test]
    async fn replacing_chunks_reports_exactly_what_disappeared() {
        let (cat, id) = bucket().await;
        let a = chunk(0, "src/net.rs", "fn one() {}");
        let b = chunk(1, "src/net.rs", "fn two() {}");
        let removed = cat
            .replace_file_chunks(&id, "src/net.rs", vec![a.clone(), b.clone()])
            .await
            .expect("v1");
        assert!(removed.is_empty());

        // Second pass keeps ordinal 0 and drops ordinal 1.
        let removed = cat
            .replace_file_chunks(&id, "src/net.rs", vec![a.clone()])
            .await
            .expect("v2");
        assert_eq!(removed, vec![b.id.clone()]);
        assert_eq!(cat.all_chunks(&id).await.expect("all").len(), 1);
    }

    #[tokio::test]
    async fn chunks_come_back_in_the_order_requested() {
        let (cat, id) = bucket().await;
        let a = chunk(0, "src/a.rs", "fn one() {}");
        let b = chunk(1, "src/a.rs", "fn two() {}");
        cat.replace_file_chunks(&id, "src/a.rs", vec![a.clone(), b.clone()])
            .await
            .expect("write");

        let got = cat
            .get_chunks(
                &id,
                &[b.id.clone(), a.id.clone(), ChunkId::from_hex("deadbeef")],
            )
            .await
            .expect("read");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, b.id);
        assert_eq!(got[1].id, a.id);
    }

    #[tokio::test]
    async fn keyword_search_finds_symbols_and_survives_hostile_input() {
        let (cat, id) = bucket().await;
        let c = chunk(0, "src/net.rs", "fn retry_with_backoff(attempts: u32) {}");
        cat.replace_file_chunks(&id, "src/net.rs", vec![c.clone()])
            .await
            .expect("write");

        let hits = cat
            .keyword_search(&id, "backoff retry", 10)
            .await
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, c.id);
        assert!(
            hits[0].score > 0.0,
            "scores must be positive and descending"
        );

        // Raw FTS5 syntax must not blow up the query.
        let hits = cat
            .keyword_search(&id, "NEAR( \"unclosed AND *", 10)
            .await
            .expect("search");
        assert!(hits.len() <= 1);
        assert!(
            cat.keyword_search(&id, "%%%", 10)
                .await
                .expect("search")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn deleted_chunks_leave_no_ghost_in_the_keyword_index() {
        let (cat, id) = bucket().await;
        let c = chunk(0, "src/net.rs", "fn retry_with_backoff() {}");
        cat.replace_file_chunks(&id, "src/net.rs", vec![c])
            .await
            .expect("write");
        cat.delete_file_chunks(&id, "src/net.rs")
            .await
            .expect("delete");
        assert!(
            cat.keyword_search(&id, "backoff", 10)
                .await
                .expect("search")
                .is_empty()
        );
    }

    #[test]
    fn fts_query_keeps_only_usable_tokens() {
        assert_eq!(
            fts_query("retry_with_backoff"),
            Some("\"retry_with_backoff\"".into())
        );
        assert_eq!(fts_query("a ? !"), None);
        assert_eq!(fts_query("Foo::bar"), Some("\"foo\" OR \"bar\"".into()));
    }
}
