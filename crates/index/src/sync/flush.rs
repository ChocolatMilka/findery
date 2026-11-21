//! Turning a batch of chunked files into vectors and rows: the one place
//! where the time of a sync goes.
//!
//! INTENT: the misses are embedded in the runtime's own batch size and counted
//! as each batch lands, so a flush of 500 chunks moves the bar sixteen times
//! instead of once at the end.

use std::collections::{HashMap, HashSet};

use futures::{StreamExt, stream};

use findery_core::config::SyncConfig;
use findery_core::ids::{ChunkId, ContentHash};
use findery_core::model::{Chunk, FileRecord, Lang, Project, RawChunk, SyncReport};
use findery_core::traits::{VectorPayload, VectorRecord};
use findery_core::{Error, Result};

use crate::gates::Lease;
use crate::jobs::progress::ProgressHandle;

/// A file whose chunks are computed but not yet embedded or written.
pub(super) struct Pending {
    pub(super) record: FileRecord,
    pub(super) chunks: Vec<Chunk>,
    pub(super) is_new: bool,
}

/// The files waiting for one flush, bounded by `[sync]` so a first index of a
/// large tree never holds more than one flush's chunks at once.
pub(super) struct Batch {
    files: Vec<Pending>,
    chunks: usize,
    max_files: usize,
    max_chunks: usize,
}

impl Batch {
    pub(super) fn new(limits: &SyncConfig) -> Self {
        Self {
            files: Vec::new(),
            chunks: 0,
            max_files: limits.flush_files,
            max_chunks: limits.flush_chunks,
        }
    }

    /// Adds a file; `true` once the batch is worth a flush.
    pub(super) fn push(&mut self, file: Pending) -> bool {
        self.chunks += file.chunks.len();
        self.files.push(file);
        self.files.len() >= self.max_files || self.chunks >= self.max_chunks
    }

    /// Empties the batch into a flush.
    pub(super) fn take(&mut self) -> Vec<Pending> {
        self.chunks = 0;
        std::mem::take(&mut self.files)
    }
}

/// Embeds and writes a batch of files into the lease's member.
pub(super) async fn flush(
    lease: &Lease,
    pending: Vec<Pending>,
    report: &mut SyncReport,
    progress: &ProgressHandle,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    // Checked before the batch, not only between files: a flush is where the
    // time goes, and a drain should not wait out one it could have prevented.
    lease.check_cancelled()?;
    let member = lease.writer()?;
    // Only a member that holds exactly this recipe reaches here, so the cache
    // key names the recipe the vectors already in it were built with.
    let recipe = lease.recipe.as_str();

    let mut vectors = member
        .bucket
        .cached_vectors(recipe, distinct_hashes(&pending))
        .await?;
    let misses = misses(&pending, &vectors);
    report.embeddings_cached += vectors.len() as u64;
    progress.write(|p| {
        p.embeddings_cached += vectors.len() as u64;
        p.expect_embeddings(misses.len() as u64);
    });

    if !misses.is_empty() {
        let computed = embed(lease, &misses, progress).await?;
        report.embeddings_computed += computed.len() as u64;
        let fresh: Vec<(ContentHash, Vec<f32>)> = misses
            .iter()
            .map(|c| c.content_hash.clone())
            .zip(computed)
            .collect();
        // Copied: the cache write takes its vectors, the rows below need them.
        for (hash, vector) in &fresh {
            vectors.insert(hash.to_string(), vector.clone());
        }
        member.bucket.put_cached_vectors(recipe, fresh).await?;
    }

    for file in pending {
        write_file(lease, file, &vectors, report).await?;
        progress.write(|p| p.chunks_done = report.chunks_written);
    }
    Ok(())
}

/// Every chunk hash of the batch, once: two identical chunks in two files
/// cost one lookup and, on a miss, one embedding.
fn distinct_hashes(pending: &[Pending]) -> Vec<ContentHash> {
    let mut seen = HashSet::new();
    pending
        .iter()
        .flat_map(|f| &f.chunks)
        .filter(|c| seen.insert(c.content_hash.as_str()))
        .map(|c| c.content_hash.clone())
        .collect()
}

/// The chunks the cache did not have, one per distinct hash.
fn misses<'a>(pending: &'a [Pending], cached: &HashMap<String, Vec<f32>>) -> Vec<&'a Chunk> {
    let mut taken = HashSet::new();
    pending
        .iter()
        .flat_map(|f| &f.chunks)
        .filter(|c| !cached.contains_key(c.content_hash.as_str()))
        .filter(|c| taken.insert(c.content_hash.as_str()))
        .collect()
}

/// Embeds `misses` in slices of the runtime's batch size, `concurrency` at a
/// time, in order, counting each slice as it lands. The embedder's own
/// semaphore still bounds what reaches the runtime.
///
/// A drain is not checked between slices: a flush that started finishes, so
/// the vectors it paid for are written rather than thrown away.
async fn embed(
    lease: &Lease,
    misses: &[&Chunk],
    progress: &ProgressHandle,
) -> Result<Vec<Vec<f32>>> {
    // Asked only now: a batch whose every vector is cached writes with the
    // runtime down, and a lazy profile is not built for nothing.
    let embedder = lease.embedder().await?;
    let (batch_size, concurrency) = lease.batching()?;
    let inputs: Vec<String> = misses.iter().map(|c| c.embed_input()).collect();
    // Collected rather than mapped lazily: a stream over a `map` closure
    // borrows through a higher-ranked lifetime the spawned job's `Send` bound
    // cannot prove.
    let mut slices = Vec::new();
    for slice in inputs.chunks(batch_size) {
        let embedder = &embedder;
        slices.push(async move {
            let vectors = embedder.embed_documents(slice).await?;
            if vectors.len() != slice.len() {
                return Err(Error::Embed(format!(
                    "embedder returned {} vectors for {} inputs",
                    vectors.len(),
                    slice.len()
                )));
            }
            progress.write(|p| p.observe(vectors.len() as u64));
            Ok(vectors)
        });
    }

    let mut landed = stream::iter(slices).buffered(concurrency);
    let mut out = Vec::with_capacity(inputs.len());
    while let Some(vectors) = landed.next().await {
        out.extend(vectors?);
    }
    Ok(out)
}

/// Writes one file: its chunks, their vectors, its record.
///
/// The bucket decides what disappears, the store follows: a bucket row with no
/// vector self-heals on the next pass, an orphan vector does not.
async fn write_file(
    lease: &Lease,
    file: Pending,
    vectors: &HashMap<String, Vec<f32>>,
    report: &mut SyncReport,
) -> Result<()> {
    let member = lease.writer()?;
    let project = &lease.project.id;
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

    let orphans = member
        .bucket
        .replace_file_chunks(project, &file.record.path, file.chunks)
        .await?;
    if !orphans.is_empty() {
        member.store.delete(project, &orphans).await?;
        report.chunks_deleted += orphans.len() as u64;
    }
    member.store.upsert(project, &records).await?;
    member.bucket.put_file(project, &file.record).await?;

    report.chunks_written += records.len() as u64;
    if file.is_new {
        report.files_added += 1;
    } else {
        report.files_updated += 1;
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
