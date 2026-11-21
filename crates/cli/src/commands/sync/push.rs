//! Client-driven sync: the CLI walks, reads and sends.
//!
//! INTENT: the daemon can only walk what it can see. In a container that means
//! a bind mount per project root, and filesystem events that do not survive the
//! boundary. Pushing inverts it — the client reads at native speed, watches with
//! native events, and the daemon never touches your disk. A daemon on another
//! machine becomes possible for the same reason.
//!
//! The three gates still apply, they just move: the manifest replaces the
//! bucket lookup, and the vector cache stays where it is, server-side.

use findery_core::api::{AddDocument, PruneRequest, PushDocuments};
use findery_core::ids::{ContentHash, ProjectId};
use findery_core::model::{Project, SyncReport};
use findery_core::{Error, Result};
use std::collections::HashMap;

/// Documents per HTTP request.
const BATCH_FILES: usize = 64;
/// Bytes per HTTP request, whichever limit trips first. Well under the server's
/// 32 MiB body ceiling, with room for JSON escaping.
const BATCH_BYTES: usize = 4 * 1024 * 1024;

/// Walks `project.host_root`, sends what changed, and prunes what vanished.
pub async fn sync(client: &findery_client::Client, project: &Project) -> Result<SyncReport> {
    let started = std::time::Instant::now();
    let root = std::path::PathBuf::from(&project.host_root);
    if !root.is_dir() {
        return Err(Error::Invalid(format!(
            "`{}` is not a directory on this machine",
            project.host_root
        )));
    }

    // What the daemon already holds, so only differences travel.
    let known: HashMap<String, ContentHash> = client
        .file_manifest(&project.id)
        .await?
        .into_iter()
        .map(|f| (f.path, f.content_hash))
        .collect();

    let spec = project.spec.clone();
    let outcome = tokio::task::spawn_blocking(move || findery_walk::walk(&root, &spec))
        .await
        .map_err(|e| Error::Internal(format!("walk task: {e}")))??;

    let mut total = SyncReport {
        project: project.id.to_string(),
        mode: "push".into(),
        files_scanned: outcome.files.len() as u64,
        warnings: outcome.warnings,
        ..SyncReport::default()
    };
    let mut seen = Vec::with_capacity(outcome.files.len());
    let mut batch: Vec<AddDocument> = Vec::new();
    let mut batch_bytes = 0usize;

    for candidate in outcome.files {
        seen.push(candidate.rel_path.clone());

        let bytes = match tokio::fs::read(&candidate.abs_path).await {
            Ok(b) => b,
            Err(e) => {
                total.warnings.push(format!("{}: {e}", candidate.rel_path));
                total.files_skipped += 1;
                continue;
            }
        };
        let Ok(text) = String::from_utf8(bytes) else {
            total
                .warnings
                .push(format!("{}: not valid UTF-8", candidate.rel_path));
            total.files_skipped += 1;
            continue;
        };

        // The only gate the client can evaluate: same bytes, nothing to send.
        // Reading and hashing locally is far cheaper than shipping the file.
        if known.get(&candidate.rel_path) == Some(&ContentHash::of(text.as_bytes())) {
            total.files_unchanged += 1;
            continue;
        }

        batch_bytes += text.len();
        batch.push(AddDocument {
            path: candidate.rel_path,
            text: Some(text),
        });
        if batch.len() >= BATCH_FILES || batch_bytes >= BATCH_BYTES {
            let sent = std::mem::take(&mut batch);
            batch_bytes = 0;
            absorb(&mut total, push(client, &project.id, sent).await?);
        }
    }
    if !batch.is_empty() {
        absorb(&mut total, push(client, &project.id, batch).await?);
    }

    total.files_removed = client
        .prune(&project.id, &PruneRequest { keep: seen })
        .await?;
    total.duration_ms = started.elapsed().as_millis() as u64;
    Ok(total)
}

async fn push(
    client: &findery_client::Client,
    project: &ProjectId,
    documents: Vec<AddDocument>,
) -> Result<SyncReport> {
    client
        .push_documents(project, &PushDocuments { documents })
        .await
}

/// Folds one batch's report into the running total.
///
/// `files_scanned` and `files_unchanged` are owned by the caller, which is the
/// only side that saw the whole walk; everything else accumulates.
fn absorb(total: &mut SyncReport, batch: SyncReport) {
    total.files_added += batch.files_added;
    total.files_updated += batch.files_updated;
    total.files_unchanged += batch.files_unchanged;
    total.chunks_written += batch.chunks_written;
    total.chunks_deleted += batch.chunks_deleted;
    total.embeddings_computed += batch.embeddings_computed;
    total.embeddings_cached += batch.embeddings_cached;
    total.warnings.extend(batch.warnings);
}

/// Watches the project locally and re-syncs after each quiet period.
///
/// INTENT: this is the half that cannot live in the daemon. Filesystem events do
/// not cross a container boundary reliably, so whoever owns the files must be
/// the one watching them.
///
/// Each burst triggers a full re-diff rather than a targeted push. Walking and
/// hashing is cheap and local; tracking individual paths would have to get
/// renames, directory moves and editor swap-files right, and getting that wrong
/// leaves the index quietly stale.
pub async fn watch(
    client: &findery_client::Client,
    project: &Project,
    debounce: std::time::Duration,
) -> Result<()> {
    use notify::{RecursiveMode, Watcher as _};

    let root = std::path::PathBuf::from(&project.host_root);
    if !root.is_dir() {
        return Err(Error::Invalid(format!(
            "`{}` is not a directory on this machine",
            project.host_root
        )));
    }

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res
            && matches!(
                event.kind,
                notify::EventKind::Create(_)
                    | notify::EventKind::Modify(_)
                    | notify::EventKind::Remove(_)
            )
        {
            let _ = tx.send(());
        }
    })
    .map_err(|e| Error::Internal(format!("watcher: {e}")))?;
    watcher
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|e| Error::Internal(format!("watch `{}`: {e}", root.display())))?;

    println!("watching {} — Ctrl-C to stop", project.host_root);
    let mut dirty = false;
    loop {
        tokio::select! {
            received = rx.recv() => match received {
                Some(()) => dirty = true,
                None => return Ok(()),
            },
            _ = tokio::time::sleep(debounce), if dirty => {
                dirty = false;
                match sync(client, project).await {
                    Ok(report) if report.chunks_written > 0 || report.files_removed > 0 => {
                        println!(
                            "  +{} ~{} -{} · chunks +{} -{} · {} embedded",
                            report.files_added,
                            report.files_updated,
                            report.files_removed,
                            report.chunks_written,
                            report.chunks_deleted,
                            report.embeddings_computed
                        );
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("  sync failed: {e}"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(added: u64, computed: u64) -> SyncReport {
        SyncReport {
            files_added: added,
            chunks_written: added * 3,
            embeddings_computed: computed,
            warnings: vec![format!("w{added}")],
            ..SyncReport::default()
        }
    }

    #[test]
    fn batch_reports_accumulate_into_one() {
        let mut total = SyncReport {
            files_scanned: 10,
            ..SyncReport::default()
        };
        absorb(&mut total, report(2, 6));
        absorb(&mut total, report(3, 9));

        assert_eq!(total.files_added, 5);
        assert_eq!(total.chunks_written, 15);
        assert_eq!(total.embeddings_computed, 15);
        assert_eq!(total.warnings.len(), 2);
        // The walk count belongs to the caller and must survive the folds.
        assert_eq!(total.files_scanned, 10);
    }

    #[test]
    fn an_unchanged_file_is_recognised_by_content_alone() {
        // The client has no mtime to compare against: the manifest carries the
        // hash, so identical bytes are the only signal, and it is enough.
        let text = "fn main() {}";
        let known = ContentHash::of(text.as_bytes());
        assert_eq!(ContentHash::of(text.as_bytes()), known);
        assert_ne!(ContentHash::of(b"fn main() { () }"), known);
    }
}
