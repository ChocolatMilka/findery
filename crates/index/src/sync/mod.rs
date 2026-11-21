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
//! INTENT: gate 3 is what makes re-running a sync, running a `--full` or
//! re-cloning the repository free. Vectors are keyed by content and by recipe,
//! never by location — and the content that is hashed is the text actually
//! embedded, `path:` header included, so a fragment that MOVED is re-embedded.
//! The path is signal; that cost is the point of it.
//!
//! A pass is planned before it is done (`plan`: the walk, then gate 1 over the
//! whole list), then worked by two overlapping halves (`pipeline`).

mod flush;
mod ingest;
mod pipeline;
mod plan;

pub use ingest::{forget_file, index_inline, prune, push_documents};

use std::collections::HashMap;

use findery_core::Result;
use findery_core::api::ProgressPhase;
use findery_core::model::{FileRecord, Origin, SyncMode, SyncReport};
use findery_core::support::clock::now_millis;

use crate::Indexer;
use crate::gates::Lease;
use crate::jobs::progress::ProgressHandle;
use plan::Plan;

/// Runs a synchronization pass into the lease's member and returns exactly
/// what it did. `progress` carries the counters out to whoever is watching;
/// the pass never waits on it. A drain stops it between two files, or before a
/// flush, with `Cancelled`.
pub async fn run(
    idx: &Indexer,
    lease: &Lease,
    mode: SyncMode,
    progress: &ProgressHandle,
) -> Result<SyncReport> {
    progress.begin();
    let member = lease.writer()?;
    let project = &lease.project;
    let started = now_millis();
    let mut report = SyncReport {
        project: project.id.to_string(),
        mode: mode.as_str().to_string(),
        ..SyncReport::default()
    };

    // A file holding another model's vectors must not receive this one's: the
    // two spaces would share a store and neither would be searchable.
    member.ensure_serves(&lease.recipe)?;
    member
        .store
        .ensure_project(&project.id, lease.recipe.dim())
        .await?;
    let known = member.bucket.file_map(&project.id).await?;
    let mode = rebuild_if_store_is_empty(lease, mode, &known, &mut report).await?;
    let full = matches!(mode, SyncMode::Full);

    let (candidates, warnings) = plan::candidates(project, &mode, progress).await?;
    report.warnings.extend(warnings);
    report.files_scanned = candidates.len() as u64;
    let plan = Plan::new(candidates, &known, full);
    plan.publish(progress);
    report.files_unchanged = plan.unchanged;

    let Plan { seen, pending, .. } = plan;
    pipeline::run(idx, lease, pending, &known, full, &mut report, progress).await?;

    // A targeted sync never deletes: the caller asked about specific paths and
    // said nothing about the rest of the project.
    if !matches!(mode, SyncMode::Paths(_)) {
        progress.enter(ProgressPhase::Pruning);
        // Only what the daemon itself walked. Pushed and inline files belong
        // to whoever fed them.
        let vanished = known
            .iter()
            .filter(|(path, record)| record.origin == Origin::File && !seen.contains(*path));
        for (path, _) in vanished {
            report.chunks_deleted += forget_file(member, project, path).await?;
            report.files_removed += 1;
        }
    }

    member.bucket.touch_sync(&project.id).await?;
    report.duration_ms = now_millis().saturating_sub(started);
    Ok(report)
}

/// Switching vector backend leaves the bucket intact and the new store empty.
/// An incremental pass would then find every file unchanged, write nothing,
/// and leave search returning nothing — silently. Rebuild instead, and say so.
async fn rebuild_if_store_is_empty(
    lease: &Lease,
    mode: SyncMode,
    known: &HashMap<String, FileRecord>,
    report: &mut SyncReport,
) -> Result<SyncMode> {
    if matches!(mode, SyncMode::Full) || known.is_empty() {
        return Ok(mode);
    }
    let store = &lease.writer()?.store;
    if store.count(&lease.project.id).await? != 0 {
        return Ok(mode);
    }
    tracing::warn!(
        project = %lease.project.id,
        "vector store is empty but the bucket is not; rebuilding it"
    );
    report.mode = SyncMode::Full.as_str().to_string();
    report
        .warnings
        .push("vector store was empty; rebuilt from the bucket".into());
    Ok(SyncMode::Full)
}
