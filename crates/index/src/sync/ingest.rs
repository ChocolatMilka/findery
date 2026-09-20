//! The write path: turning documents into chunks, vectors and catalog rows.
//!
//! INTENT: split from `sync/mod.rs`, which decides WHAT changed by walking a
//! directory. This module decides nothing — it takes documents from whoever has
//! them, whether the daemon read them off disk or a client pushed them over
//! HTTP, and writes them the same way. One write path, so the two feeding sides
//! cannot drift.

use super::Indexer;
use crate::CatalogHandle;
use findery_core::clock::{now_millis, now_unix};
use findery_core::ids::{ChunkId, ContentHash};
use findery_core::model::{Chunk, FileRecord, Lang, Origin, Project, RawChunk, SyncReport};
use findery_core::traits::{VectorPayload, VectorRecord};
use findery_core::{Error, Result};

/// Files buffered before a flush. Bounds peak memory on a first full index.
pub(super) const FLUSH_FILES: usize = 96;
/// Chunks buffered before a flush, whichever limit trips first.
pub(super) const FLUSH_CHUNKS: usize = 512;

/// A file whose chunks are computed but not yet embedded or written.
pub(super) struct Pending {
    pub(super) record: FileRecord,
    pub(super) chunks: Vec<Chunk>,
    pub(super) is_new: bool,
}

/// Indexes one document supplied by the caller, bypassing the filesystem.
pub async fn index_inline(
    idx: &Indexer,
    cat: &CatalogHandle,
    project: &Project,
    rel_path: &str,
    text: &str,
) -> Result<SyncReport> {
    ingest(
        idx,
        cat,
        project,
        vec![(rel_path.to_string(), text.to_string())],
        Origin::Inline,
    )
    .await
}

/// Indexes a batch of documents pushed by a client that walked the project
/// itself.
///
/// INTENT: one call per document would spend the whole first index in HTTP round
/// trips. Batching also lets the embedder fill its own batches, which is where
/// the throughput actually comes from.
pub async fn push_documents(
    idx: &Indexer,
    cat: &CatalogHandle,
    project: &Project,
    documents: Vec<(String, String)>,
) -> Result<SyncReport> {
    ingest(idx, cat, project, documents, Origin::Pushed).await
}

/// Shared body of inline and pushed ingestion.
async fn ingest(
    idx: &Indexer,
    cat: &CatalogHandle,
    project: &Project,
    documents: Vec<(String, String)>,
    origin: Origin,
) -> Result<SyncReport> {
    let started = now_millis();
    let mut report = SyncReport {
        project: project.id.to_string(),
        mode: if origin == Origin::Pushed {
            "push"
        } else {
            "inline"
        }
        .to_string(),
        files_scanned: documents.len() as u64,
        ..SyncReport::default()
    };
    if documents.is_empty() {
        return Ok(report);
    }
    // A catalog built by another model must not receive vectors of this one:
    // the two spaces would share a store and neither would be searchable.
    cat.ensure_vectors_usable()?;
    cat.store
        .ensure_project(&project.id, idx.embedder.dim())
        .await?;

    let known = cat.catalog.file_map(&project.id).await?;
    let mut pending = Vec::with_capacity(documents.len());
    let mut buffered = 0usize;

    for (rel_path, text) in documents {
        let lang = Lang::from_path(&rel_path);
        let hash = ContentHash::of(text.as_bytes());

        // The caller may push a file it already sent unchanged; skipping here
        // costs one hash comparison and saves a full re-chunk.
        if let Some(previous) = known.get(&rel_path)
            && previous.content_hash == hash
        {
            report.files_unchanged += 1;
            continue;
        }

        let raw = idx.chunkers.chunk(&rel_path, lang, &text, &idx.cfg.chunk)?;
        let chunks = materialize(project, &rel_path, lang, raw);
        let record = FileRecord {
            path: rel_path.clone(),
            lang,
            origin,
            content_hash: hash,
            size: text.len() as u64,
            mtime_ns: 0,
            chunk_count: chunks.len() as u32,
            indexed_at: now_unix(),
        };
        buffered += chunks.len();
        pending.push(Pending {
            record,
            chunks,
            is_new: !known.contains_key(&rel_path),
        });

        if pending.len() >= FLUSH_FILES || buffered >= FLUSH_CHUNKS {
            flush(idx, cat, project, std::mem::take(&mut pending), &mut report).await?;
            buffered = 0;
        }
    }
    if !pending.is_empty() {
        flush(idx, cat, project, pending, &mut report).await?;
    }
    cat.catalog.touch_sync(&project.id).await?;
    report.duration_ms = now_millis().saturating_sub(started);
    Ok(report)
}

/// Removes pushed files the client no longer has.
///
/// INTENT: only `Origin::Pushed` rows are touched. A client must never be able
/// to delete what the daemon walked, nor a hand-added inline document — each
/// side prunes what it fed, and nothing else.
pub async fn prune(
    cat: &CatalogHandle,
    project: &Project,
    keep: &std::collections::HashSet<String>,
) -> Result<u64> {
    let known = cat.catalog.file_map(&project.id).await?;
    let mut removed = 0u64;
    for (path, record) in &known {
        if record.origin != Origin::Pushed || keep.contains(path.as_str()) {
            continue;
        }
        forget_file(cat, project, path).await?;
        removed += 1;
    }
    if removed > 0 {
        cat.catalog.touch_sync(&project.id).await?;
    }
    Ok(removed)
}

/// Removes one document: chunks, vectors and file record.
pub async fn forget_file(cat: &CatalogHandle, project: &Project, rel_path: &str) -> Result<u64> {
    let removed = cat
        .catalog
        .delete_file_chunks(&project.id, rel_path)
        .await?;
    if !removed.is_empty() {
        cat.store.delete(&project.id, &removed).await?;
    }
    cat.catalog
        .delete_file_record(&project.id, rel_path)
        .await?;
    Ok(removed.len() as u64)
}

// ---------------------------------------------------------------------------

/// Embeds and writes a batch of files.
pub(super) async fn flush(
    idx: &Indexer,
    cat: &CatalogHandle,
    project: &Project,
    pending: Vec<Pending>,
    report: &mut SyncReport,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    // Only a catalog whose stored recipe is this one reaches here, so the cache
    // key names the recipe the vectors already in it were built with.
    let recipe = cat.recipe.clone();

    // One cache probe for the whole batch, deduplicated: two identical chunks
    // in two files cost one lookup and, on a miss, one embedding.
    let mut wanted: Vec<ContentHash> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for file in &pending {
        for chunk in &file.chunks {
            if seen.insert(chunk.content_hash.to_string()) {
                wanted.push(chunk.content_hash.clone());
            }
        }
    }
    let mut vectors = cat.catalog.cached_vectors(&recipe, wanted.clone()).await?;
    report.embeddings_cached += vectors.len() as u64;

    let misses: Vec<&Chunk> = {
        let mut out = Vec::new();
        let mut taken = std::collections::HashSet::new();
        for file in &pending {
            for chunk in &file.chunks {
                if !vectors.contains_key(chunk.content_hash.as_str())
                    && taken.insert(chunk.content_hash.to_string())
                {
                    out.push(chunk);
                }
            }
        }
        out
    };

    if !misses.is_empty() {
        let inputs: Vec<String> = misses.iter().map(|c| c.embed_input()).collect();
        let computed = idx.embedder.embed_documents(&inputs).await?;
        if computed.len() != misses.len() {
            return Err(Error::Embed(format!(
                "embedder returned {} vectors for {} inputs",
                computed.len(),
                misses.len()
            )));
        }
        report.embeddings_computed += computed.len() as u64;
        let mut to_cache = Vec::with_capacity(computed.len());
        for (chunk, vector) in misses.iter().zip(computed) {
            vectors.insert(chunk.content_hash.to_string(), vector.clone());
            to_cache.push((chunk.content_hash.clone(), vector));
        }
        cat.catalog.put_cached_vectors(&recipe, to_cache).await?;
    }

    for file in pending {
        let path = file.record.path.clone();
        let mut records = Vec::with_capacity(file.chunks.len());
        for chunk in &file.chunks {
            let Some(vector) = vectors.get(chunk.content_hash.as_str()) else {
                return Err(Error::Internal(format!(
                    "chunk {} lost its vector",
                    chunk.id
                )));
            };
            records.push(VectorRecord {
                id: chunk.id.clone(),
                vector: vector.clone(),
                payload: VectorPayload {
                    path: chunk.path.clone(),
                    lang: chunk.lang,
                    kind: chunk.kind,
                    symbol: chunk.symbol.clone(),
                },
            });
        }

        let orphans = cat
            .catalog
            .replace_file_chunks(&project.id, &path, file.chunks)
            .await?;
        if !orphans.is_empty() {
            cat.store.delete(&project.id, &orphans).await?;
            report.chunks_deleted += orphans.len() as u64;
        }
        cat.store.upsert(&project.id, &records).await?;
        cat.catalog.put_file(&project.id, &file.record).await?;

        report.chunks_written += records.len() as u64;
        if file.is_new {
            report.files_added += 1;
        } else {
            report.files_updated += 1;
        }
    }
    Ok(())
}

/// Stamps project identity, ordinals and content hashes onto raw chunks.
pub(super) fn materialize(
    project: &Project,
    rel_path: &str,
    lang: Lang,
    raw: Vec<RawChunk>,
) -> Vec<Chunk> {
    raw.into_iter()
        .enumerate()
        .map(|(ordinal, r)| {
            let ordinal = ordinal as u32;
            let mut chunk = Chunk {
                id: ChunkId::derive(&project.id, rel_path, ordinal),
                project: project.id.clone(),
                path: rel_path.to_string(),
                lang,
                ordinal,
                kind: r.kind,
                symbol: r.symbol,
                breadcrumb: r.breadcrumb,
                start_line: r.start_line,
                end_line: r.end_line,
                // Placeholder: the real hash covers the text we actually embed,
                // header included, so a path change invalidates the vector.
                content_hash: ContentHash::from_hex(String::new()),
                text: r.text,
            };
            chunk.content_hash = ContentHash::of(chunk.embed_input().as_bytes());
            chunk
        })
        .collect()
}
