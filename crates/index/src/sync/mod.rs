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

mod ingest;

use ingest::{FLUSH_CHUNKS, FLUSH_FILES, Pending, flush, materialize};
pub use ingest::{forget_file, index_inline, prune, push_documents};

use crate::BucketHandle;
use crate::Indexer;
use findery_core::clock::{now_millis, now_unix};
use findery_core::ids::ContentHash;
use findery_core::model::{FileRecord, Lang, Origin, Project, SyncMode, SyncReport};
use findery_core::{Error, Result};

/// Runs a synchronization pass and returns exactly what it did.
pub async fn run(
    idx: &Indexer,
    cat: &BucketHandle,
    project: &Project,
    mode: SyncMode,
) -> Result<SyncReport> {
    let started = now_millis();
    let mut report = SyncReport {
        project: project.id.to_string(),
        mode: mode.as_str().to_string(),
        ..SyncReport::default()
    };

    // A bucket built by another model must not receive vectors of this one:
    // the two spaces would share a store and neither would be searchable.
    cat.ensure_vectors_usable()?;
    cat.store
        .ensure_project(&project.id, idx.embedder.dim())
        .await?;
    let known = cat.bucket.file_map(&project.id).await?;

    // Switching vector backend leaves the bucket intact and the new store
    // empty. An incremental pass would then find every file unchanged, write
    // nothing, and leave search returning nothing — silently. Detect that and
    // rebuild instead of reporting a successful no-op.
    let mode = if !matches!(mode, SyncMode::Full)
        && !known.is_empty()
        && cat.store.count(&project.id).await.unwrap_or(1) == 0
    {
        tracing::warn!(
            project = %project.id,
            "vector store is empty but the bucket is not; rebuilding it"
        );
        report.mode = SyncMode::Full.as_str().to_string();
        report
            .warnings
            .push("vector store was empty; rebuilt from the bucket".into());
        SyncMode::Full
    } else {
        mode
    };
    let full = matches!(mode, SyncMode::Full);

    let (candidates, warnings) = collect_candidates(project, &mode).await?;
    report.warnings.extend(warnings);
    report.files_scanned = candidates.len() as u64;

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
            cat.bucket.put_file(&project.id, &record).await?;
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
            flush(idx, cat, project, std::mem::take(&mut pending), &mut report).await?;
            buffered_chunks = 0;
        }
    }
    if !pending.is_empty() {
        flush(idx, cat, project, pending, &mut report).await?;
    }

    // A targeted sync never deletes: the caller asked about specific paths and
    // said nothing about the rest of the project.
    if !matches!(mode, SyncMode::Paths(_)) {
        let seen: std::collections::HashSet<&str> = seen.iter().map(String::as_str).collect();
        for (path, record) in &known {
            // Only what the daemon itself walked. Pushed and inline files
            // belong to whoever fed them.
            if record.origin != Origin::File || seen.contains(path.as_str()) {
                continue;
            }
            let removed = forget_file(cat, project, path).await?;
            report.chunks_deleted += removed;
            report.files_removed += 1;
        }
    }

    cat.bucket.touch_sync(&project.id).await?;
    report.duration_ms = now_millis().saturating_sub(started);
    Ok(report)
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
