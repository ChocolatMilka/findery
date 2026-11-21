//! The write path: turning documents into chunks, vectors and bucket rows.
//!
//! INTENT: split from `sync/mod.rs`, which decides WHAT changed by walking a
//! directory. This module decides nothing — it takes documents from whoever has
//! them, whether the daemon read them off disk or a client pushed them over
//! HTTP, and writes them the same way. One write path, so the two feeding sides
//! cannot drift.

use findery_core::Result;
use findery_core::ids::ContentHash;
use findery_core::model::{FileRecord, Lang, Origin, Project, SyncReport};
use findery_core::support::clock::{now_millis, now_unix};

use super::Indexer;
use super::flush::{Batch, Pending, flush, materialize};
use crate::BucketHandle;
use crate::gates::Lease;
use crate::jobs::progress::ProgressHandle;

/// Indexes one document supplied by the caller, bypassing the filesystem.
pub async fn index_inline(
    idx: &Indexer,
    lease: &Lease,
    rel_path: &str,
    text: &str,
) -> Result<SyncReport> {
    ingest(
        idx,
        lease,
        vec![(rel_path.to_string(), text.to_string())],
        Origin::Inline,
        &ProgressHandle::detached(),
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
    lease: &Lease,
    documents: Vec<(String, String)>,
) -> Result<SyncReport> {
    // Nobody polls a pushed batch: the client that walked the project draws its
    // own bar. The engine stays written once rather than forked in two.
    ingest(
        idx,
        lease,
        documents,
        Origin::Pushed,
        &ProgressHandle::detached(),
    )
    .await
}

/// Shared body of inline and pushed ingestion.
async fn ingest(
    idx: &Indexer,
    lease: &Lease,
    documents: Vec<(String, String)>,
    origin: Origin,
    progress: &ProgressHandle,
) -> Result<SyncReport> {
    let member = lease.writer()?;
    let project = &lease.project;
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
    // A file holding another model's vectors must not receive this one's: the
    // two spaces would share a store and neither would be searchable.
    member.ensure_serves(&lease.recipe)?;
    member
        .store
        .ensure_project(&project.id, lease.recipe.dim())
        .await?;

    let known = member.bucket.file_map(&project.id).await?;
    let mut batch = Batch::new(&idx.cfg.sync);

    for (rel_path, text) in documents {
        lease.check_cancelled()?;
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
        let file = Pending {
            record,
            chunks,
            is_new: !known.contains_key(&rel_path),
        };
        if batch.push(file) {
            flush(lease, batch.take(), &mut report, progress).await?;
        }
    }
    flush(lease, batch.take(), &mut report, progress).await?;
    member.bucket.touch_sync(&project.id).await?;
    report.duration_ms = now_millis().saturating_sub(started);
    Ok(report)
}

/// Removes pushed files the client no longer has from one member.
///
/// INTENT: only `Origin::Pushed` rows are touched. A client must never be able
/// to delete what the daemon walked, nor a hand-added inline document — each
/// side prunes what it fed, and nothing else.
pub async fn prune(
    member: &BucketHandle,
    project: &Project,
    keep: &std::collections::HashSet<String>,
) -> Result<u64> {
    let known = member.bucket.file_map(&project.id).await?;
    let mut removed = 0u64;
    for (path, record) in &known {
        if record.origin != Origin::Pushed || keep.contains(path.as_str()) {
            continue;
        }
        forget_file(member, project, path).await?;
        removed += 1;
    }
    if removed > 0 {
        member.bucket.touch_sync(&project.id).await?;
    }
    Ok(removed)
}

/// Removes one document from one member: chunks, vectors and file record.
pub async fn forget_file(member: &BucketHandle, project: &Project, rel_path: &str) -> Result<u64> {
    let removed = member
        .bucket
        .delete_file_chunks(&project.id, rel_path)
        .await?;
    if !removed.is_empty() {
        member.store.delete(&project.id, &removed).await?;
    }
    member
        .bucket
        .delete_file_record(&project.id, rel_path)
        .await?;
    Ok(removed.len() as u64)
}
