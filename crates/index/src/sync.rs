//! The incremental synchronization engine.
//!
//! Three cheap gates stand between a file on disk and the GPU, in this order:
//!
//! 1. `(size, mtime)` match — the file is untouched, nothing is read.
//! 2. content hash match — the file was rewritten with identical bytes
//!    (a formatter run, a branch switch and back), nothing is chunked.
//! 3. chunk hash found in the content-addressed cache — the fragment already
//!    has a vector under this recipe, nothing is embedded.
//!
//! INTENT: gate 3 is what makes moving a function between files, renaming a
//! module or re-cloning the repository free. Vectors are keyed by content and
//! by recipe, never by location.

use crate::Indexer;
use findery_core::clock::{now_millis, now_unix};
use findery_core::ids::{ChunkId, ContentHash};
use findery_core::model::{
    Chunk, FileRecord, Lang, Origin, Project, RawChunk, SyncMode, SyncReport,
};
use findery_core::traits::{VectorPayload, VectorRecord};
use findery_core::{Error, Result};
use std::collections::HashMap;

/// Files buffered before a flush. Bounds peak memory on a first full index.
const FLUSH_FILES: usize = 96;
/// Chunks buffered before a flush, whichever limit trips first.
const FLUSH_CHUNKS: usize = 512;

/// A file whose chunks are computed but not yet embedded or written.
struct Pending {
    record: FileRecord,
    chunks: Vec<Chunk>,
    is_new: bool,
}

/// Runs a synchronization pass and returns exactly what it did.
pub async fn run(idx: &Indexer, project: &Project, mode: SyncMode) -> Result<SyncReport> {
    let started = now_millis();
    let mut report = SyncReport {
        project: project.id.to_string(),
        mode: mode.as_str().to_string(),
        ..SyncReport::default()
    };

    idx.store
        .ensure_project(&project.id, idx.embedder.dim())
        .await?;
    let known = idx.catalog.file_map(&project.id).await?;

    let (candidates, warnings) = collect_candidates(project, &mode).await?;
    report.warnings.extend(warnings);
    report.files_scanned = candidates.len() as u64;

    let full = matches!(mode, SyncMode::Full);
    let mut pending: Vec<Pending> = Vec::new();
    let mut buffered_chunks = 0usize;
    let mut seen: Vec<String> = Vec::with_capacity(candidates.len());

    for candidate in candidates {
        seen.push(candidate.rel_path.clone());
        let previous = known.get(&candidate.rel_path);

        // Gate 1: metadata unchanged.
        if !full
            && let Some(prev) = previous
            && prev.size == candidate.size
            && prev.mtime_ns == candidate.mtime_ns
            && candidate.mtime_ns != 0
        {
            report.files_unchanged += 1;
            continue;
        }

        let bytes = match tokio::fs::read(&candidate.abs_path).await {
            Ok(b) => b,
            Err(e) => {
                report.warnings.push(format!("{}: {e}", candidate.rel_path));
                report.files_skipped += 1;
                continue;
            }
        };
        let Ok(text) = String::from_utf8(bytes) else {
            report
                .warnings
                .push(format!("{}: not valid UTF-8", candidate.rel_path));
            report.files_skipped += 1;
            continue;
        };
        let hash = ContentHash::of(text.as_bytes());

        // Gate 2: bytes unchanged. Refresh the metadata so gate 1 catches it
        // next time instead of re-reading the file forever.
        if !full
            && let Some(prev) = previous
            && prev.content_hash == hash
        {
            let mut record = prev.clone();
            record.mtime_ns = candidate.mtime_ns;
            record.size = candidate.size;
            idx.catalog.put_file(&project.id, &record).await?;
            report.files_unchanged += 1;
            continue;
        }

        let raw = idx
            .chunkers
            .chunk(&candidate.rel_path, candidate.lang, &text, &idx.cfg.chunk)?;
        let chunks = materialize(project, &candidate.rel_path, candidate.lang, raw);
        let record = FileRecord {
            path: candidate.rel_path.clone(),
            lang: candidate.lang,
            origin: Origin::File,
            content_hash: hash,
            size: candidate.size,
            mtime_ns: candidate.mtime_ns,
            chunk_count: chunks.len() as u32,
            indexed_at: now_unix(),
        };
        buffered_chunks += chunks.len();
        pending.push(Pending {
            record,
            chunks,
            is_new: previous.is_none(),
        });

        if pending.len() >= FLUSH_FILES || buffered_chunks >= FLUSH_CHUNKS {
            flush(idx, project, std::mem::take(&mut pending), &mut report).await?;
            buffered_chunks = 0;
        }
    }
    if !pending.is_empty() {
        flush(idx, project, pending, &mut report).await?;
    }

    // A targeted sync never deletes: the caller asked about specific paths and
    // said nothing about the rest of the project.
    if !matches!(mode, SyncMode::Paths(_)) {
        let seen: std::collections::HashSet<&str> = seen.iter().map(String::as_str).collect();
        for (path, record) in &known {
            if record.origin == Origin::Inline || seen.contains(path.as_str()) {
                continue;
            }
            let removed = forget_file(idx, project, path).await?;
            report.chunks_deleted += removed;
            report.files_removed += 1;
        }
    }

    idx.catalog.touch_sync(&project.id).await?;
    report.duration_ms = now_millis().saturating_sub(started);
    Ok(report)
}

/// Indexes a document supplied inline, bypassing the filesystem entirely.
pub async fn index_inline(
    idx: &Indexer,
    project: &Project,
    rel_path: &str,
    text: &str,
) -> Result<SyncReport> {
    let started = now_millis();
    let mut report = SyncReport {
        project: project.id.to_string(),
        mode: "inline".into(),
        files_scanned: 1,
        ..SyncReport::default()
    };
    idx.store
        .ensure_project(&project.id, idx.embedder.dim())
        .await?;

    let lang = Lang::from_path(rel_path);
    let raw = idx.chunkers.chunk(rel_path, lang, text, &idx.cfg.chunk)?;
    let chunks = materialize(project, rel_path, lang, raw);
    let existed = idx
        .catalog
        .file_map(&project.id)
        .await?
        .contains_key(rel_path);
    let record = FileRecord {
        path: rel_path.to_string(),
        lang,
        origin: Origin::Inline,
        content_hash: ContentHash::of(text.as_bytes()),
        size: text.len() as u64,
        mtime_ns: 0,
        chunk_count: chunks.len() as u32,
        indexed_at: now_unix(),
    };
    flush(
        idx,
        project,
        vec![Pending {
            record,
            chunks,
            is_new: !existed,
        }],
        &mut report,
    )
    .await?;
    report.duration_ms = now_millis().saturating_sub(started);
    Ok(report)
}

/// Removes one document: chunks, vectors and file record.
pub async fn forget_file(idx: &Indexer, project: &Project, rel_path: &str) -> Result<u64> {
    let removed = idx
        .catalog
        .delete_file_chunks(&project.id, rel_path)
        .await?;
    if !removed.is_empty() {
        idx.store.delete(&project.id, &removed).await?;
    }
    idx.catalog
        .delete_file_record(&project.id, rel_path)
        .await?;
    Ok(removed.len() as u64)
}

// ---------------------------------------------------------------------------

/// Embeds and writes a batch of files.
async fn flush(
    idx: &Indexer,
    project: &Project,
    pending: Vec<Pending>,
    report: &mut SyncReport,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let recipe = idx.recipe();

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
    let mut vectors = idx.catalog.cached_vectors(&recipe, wanted.clone()).await?;
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
        idx.catalog.put_cached_vectors(&recipe, to_cache).await?;
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

        let orphans = idx
            .catalog
            .replace_file_chunks(&project.id, &path, file.chunks)
            .await?;
        if !orphans.is_empty() {
            idx.store.delete(&project.id, &orphans).await?;
            report.chunks_deleted += orphans.len() as u64;
        }
        idx.store.upsert(&project.id, &records).await?;
        idx.catalog.put_file(&project.id, &file.record).await?;

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
fn materialize(project: &Project, rel_path: &str, lang: Lang, raw: Vec<RawChunk>) -> Vec<Chunk> {
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

/// Turns a sync mode into the list of files to consider.
async fn collect_candidates(
    project: &Project,
    mode: &SyncMode,
) -> Result<(Vec<crate::walker::Candidate>, Vec<String>)> {
    let root = std::path::PathBuf::from(&project.spec.root);
    match mode {
        SyncMode::Paths(paths) => {
            let mut out = Vec::new();
            let mut warnings = Vec::new();
            for rel in paths {
                let rel = rel.replace('\\', "/");
                if rel.contains("..") {
                    warnings.push(format!("{rel}: rejected, escapes the project root"));
                    continue;
                }
                let abs = root.join(&rel);
                match tokio::fs::metadata(&abs).await {
                    Ok(meta) if meta.is_file() => out.push(crate::walker::Candidate {
                        lang: Lang::from_path(&rel),
                        rel_path: rel,
                        abs_path: abs,
                        size: meta.len(),
                        mtime_ns: crate::walker::mtime_ns(&meta),
                    }),
                    Ok(_) => warnings.push(format!("{rel}: not a file")),
                    Err(e) => warnings.push(format!("{rel}: {e}")),
                }
            }
            Ok((out, warnings))
        }
        _ => {
            let spec = project.spec.clone();
            let outcome = tokio::task::spawn_blocking(move || crate::walker::walk(&root, &spec))
                .await
                .map_err(|e| Error::Internal(format!("walk task: {e}")))??;
            Ok((outcome.files, outcome.warnings))
        }
    }
}

/// Convenience used by tests and by `search`: a map of chunk id to chunk.
pub fn index_by_id(chunks: Vec<Chunk>) -> HashMap<String, Chunk> {
    chunks.into_iter().map(|c| (c.id.to_string(), c)).collect()
}
