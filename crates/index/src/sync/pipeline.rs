//! The work of a pass, as two halves that overlap: a scanner that reads,
//! hashes and chunks, and a writer that embeds and writes.
//!
//! INTENT: embedding a batch is network time, reading the next files is disk
//! and CPU time; done in turn, each waits for the other. The scanner runs
//! ahead by `sync.scan_ahead_flushes` (a bounded channel) and the writer stays the
//! single writer of the member (CONV-024). Both are futures of the same task,
//! joined, so no task outlives the pass:
//!
//! - a drain stops the scanner at its next file; the writer finishes the flush
//!   in flight — the vectors it paid for are kept — and refuses the next one;
//! - a failed writer drops the channel, and the scanner stops at its next send.

use std::collections::HashMap;

use tokio::sync::mpsc;

use findery_core::Result;
use findery_core::ids::ContentHash;
use findery_core::model::{FileRecord, Origin, SyncReport};
use findery_core::support::clock::now_unix;

use super::flush::{Batch, Pending, flush, materialize};
use crate::Indexer;
use crate::gates::Lease;
use crate::jobs::progress::ProgressHandle;
use crate::walker::Candidate;

/// What the scanner decided about one pending file.
enum Outcome {
    /// Gate 2: same bytes. Only the metadata moves, so gate 1 bites next time.
    Same(FileRecord),
    /// Changed: chunks waiting for their vectors.
    Changed(Pending),
    /// Unreadable: the warning that says so (CONV-022).
    Skipped(String),
}

/// Reads, embeds and writes every pending file, counting each one in
/// `progress` once its outcome is decided.
pub(super) async fn run(
    idx: &Indexer,
    lease: &Lease,
    pending: Vec<Candidate>,
    known: &HashMap<String, FileRecord>,
    full: bool,
    report: &mut SyncReport,
    progress: &ProgressHandle,
) -> Result<()> {
    let (tx, rx) = mpsc::channel(idx.cfg.sync.scan_ahead_files());
    let scanner = scan(idx, lease, pending, known, full, tx);
    let writer = write(idx, lease, rx, report, progress);
    let (scanned, written) = tokio::join!(scanner, writer);
    written.and(scanned)
}

/// The scanner: one [`Outcome`] per file, in walk order.
async fn scan(
    idx: &Indexer,
    lease: &Lease,
    pending: Vec<Candidate>,
    known: &HashMap<String, FileRecord>,
    full: bool,
    tx: mpsc::Sender<Outcome>,
) -> Result<()> {
    for candidate in pending {
        lease.check_cancelled()?;
        let previous = known.get(&candidate.rel_path);
        let outcome = examine(idx, lease, &candidate, previous, full).await?;
        // A closed channel means the writer failed; its error is the one the
        // pass returns, so the scanner just stops.
        if tx.send(outcome).await.is_err() {
            break;
        }
    }
    Ok(())
}

/// Reads one file and runs gate 2, then chunks it if its bytes moved.
async fn examine(
    idx: &Indexer,
    lease: &Lease,
    candidate: &Candidate,
    previous: Option<&FileRecord>,
    full: bool,
) -> Result<Outcome> {
    let rel = &candidate.rel_path;
    let bytes = match tokio::fs::read(&candidate.abs_path).await {
        Ok(bytes) => bytes,
        Err(e) => return Ok(Outcome::Skipped(format!("{rel}: {e}"))),
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return Ok(Outcome::Skipped(format!("{rel}: not valid UTF-8")));
    };
    let hash = ContentHash::of(text.as_bytes());

    if !full
        && let Some(prev) = previous
        && prev.content_hash == hash
    {
        return Ok(Outcome::Same(FileRecord {
            mtime_ns: candidate.mtime_ns,
            size: candidate.size,
            ..prev.clone()
        }));
    }

    let project = &lease.project;
    let raw = idx
        .chunkers
        .chunk(rel, candidate.lang, &text, &idx.cfg.chunk)?;
    let chunks = materialize(project, rel, candidate.lang, raw);
    let record = FileRecord {
        path: rel.clone(),
        lang: candidate.lang,
        origin: Origin::File,
        content_hash: hash,
        size: candidate.size,
        mtime_ns: candidate.mtime_ns,
        chunk_count: chunks.len() as u32,
        indexed_at: now_unix(),
    };
    Ok(Outcome::Changed(Pending {
        record,
        chunks,
        is_new: previous.is_none(),
    }))
}

/// The writer: the only one that touches the member or the report.
async fn write(
    idx: &Indexer,
    lease: &Lease,
    mut rx: mpsc::Receiver<Outcome>,
    report: &mut SyncReport,
    progress: &ProgressHandle,
) -> Result<()> {
    let member = lease.writer()?;
    let mut batch = Batch::new(&idx.cfg.sync);
    while let Some(outcome) = rx.recv().await {
        match outcome {
            Outcome::Same(record) => {
                member.bucket.put_file(&lease.project.id, &record).await?;
                report.files_unchanged += 1;
                decided(progress, 1);
            }
            Outcome::Skipped(warning) => {
                report.warnings.push(warning);
                report.files_skipped += 1;
                decided(progress, 1);
            }
            Outcome::Changed(file) => {
                if batch.push(file) {
                    flush_counted(lease, batch.take(), report, progress).await?;
                }
            }
        }
    }
    // The last, partial batch is counted like the others, before pruning.
    flush_counted(lease, batch.take(), report, progress).await
}

async fn flush_counted(
    lease: &Lease,
    files: Vec<Pending>,
    report: &mut SyncReport,
    progress: &ProgressHandle,
) -> Result<()> {
    let count = files.len() as u64;
    flush(lease, files, report, progress).await?;
    decided(progress, count);
    Ok(())
}

fn decided(progress: &ProgressHandle, files: u64) {
    progress.write(|p| p.files_done += files);
}

#[cfg(test)]
mod tests {
    use findery_core::model::Lang;

    use super::*;
    use crate::gates::Access;
    use crate::tests::{boot, config, project_dir, register};

    /// A file the walk saw and the scanner cannot read — deleted in between,
    /// locked, gone with its drive — is reported, never fatal.
    #[tokio::test]
    async fn a_file_gone_between_the_walk_and_the_read_is_a_warning() {
        let (data, proj) = (tempfile::tempdir().expect("data"), project_dir());
        let idx = boot(&config(data.path(), "")).await;
        let id = register(&idx, proj.path(), "p").await;
        let lease = idx.lease(&id, Access::Write).await.expect("lease");
        let gone = Candidate {
            rel_path: "src/gone.rs".into(),
            abs_path: proj.path().join("src/gone.rs"),
            size: 3,
            mtime_ns: 1,
            lang: Lang::from_path("src/gone.rs"),
        };

        let outcome = examine(&idx, &lease, &gone, None, false).await;

        match outcome {
            Ok(Outcome::Skipped(warning)) => assert!(warning.starts_with("src/gone.rs: ")),
            _ => panic!("an unreadable file is a warning"),
        }
    }
}
