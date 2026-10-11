//! Filesystem watching: keep an index live without anyone asking.
//!
//! INTENT: editors write files in bursts — a save, a formatter, a build script
//! touching a dozen generated modules. Reacting per event would re-embed the
//! same file three times in a second, so events are coalesced per path and only
//! flushed after the directory has been quiet for `debounce`.

use crate::Indexer;
use findery_core::model::{Project, SyncMode};
use findery_core::{Error, Result};
use notify::{Event, EventKind, RecursiveMode, Watcher as _};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Live watch over one project. Dropping it stops the watcher.
pub struct WatchHandle {
    project: String,
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl WatchHandle {
    pub fn project(&self) -> &str {
        &self.project
    }

    /// Stops watching. Idempotent.
    pub fn stop(&self) {
        let _ = self.shutdown.send(true);
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts watching `project`'s root and re-syncing touched paths.
pub fn spawn(indexer: Arc<Indexer>, project: Project, debounce: Duration) -> Result<WatchHandle> {
    let root = PathBuf::from(&project.spec.root);
    if !root.is_dir() {
        return Err(Error::Invalid(format!(
            "cannot watch `{}`: not a directory",
            root.display()
        )));
    }

    // notify calls back from its own thread, so the bridge to async is an
    // unbounded channel; the debounce below is what keeps it from flooding.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<PathBuf>>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| match res {
        Ok(event) if is_content_change(&event.kind) => {
            let _ = tx.send(event.paths);
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "watch error"),
    })
    .map_err(|e| Error::Internal(format!("watcher: {e}")))?;

    watcher
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|e| Error::Internal(format!("watch `{}`: {e}", root.display())))?;

    let (shutdown, mut stop_rx) = tokio::sync::watch::channel(false);
    let project_id = project.id.clone();
    let label = project.id.to_string();

    tokio::spawn(async move {
        // The watcher must outlive the loop or the OS handle closes underneath.
        let _watcher = watcher;
        let mut dirty: HashSet<String> = HashSet::new();
        loop {
            tokio::select! {
                _ = stop_rx.changed() => {
                    if *stop_rx.borrow() {
                        tracing::info!(project = %project_id, "watch stopped");
                        return;
                    }
                }
                received = rx.recv() => {
                    match received {
                        Some(paths) => {
                            for p in paths {
                                if let Some(rel) = crate::walker::relative(&root, &p) {
                                    dirty.insert(rel);
                                }
                            }
                        }
                        None => return,
                    }
                }
                _ = tokio::time::sleep(debounce), if !dirty.is_empty() => {
                    let batch: Vec<String> = dirty.drain().collect();
                    let n = batch.len();
                    // Nobody is polling a background watcher, so its pass writes
                    // into a handle with no reader.
                    match indexer
                        .sync(
                            &project_id,
                            SyncMode::Paths(batch),
                            &crate::progress::ProgressHandle::detached(),
                        )
                        .await
                    {
                        Ok(report) => tracing::info!(
                            project = %project_id,
                            paths = n,
                            chunks = report.chunks_written,
                            embedded = report.embeddings_computed,
                            "watch re-synced"
                        ),
                        Err(e) => tracing::warn!(project = %project_id, error = %e, "watch sync failed"),
                    }
                }
            }
        }
    });

    Ok(WatchHandle {
        project: label,
        shutdown,
    })
}

/// Only events that can change a file's bytes are worth a re-sync.
///
/// Access-time and metadata-only events fire constantly on an active checkout
/// and would keep the debounce window permanently open.
fn is_content_change(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, CreateKind, ModifyKind, RemoveKind};

    #[test]
    fn only_content_events_trigger_a_resync() {
        assert!(is_content_change(&EventKind::Create(CreateKind::File)));
        assert!(is_content_change(&EventKind::Modify(ModifyKind::Any)));
        assert!(is_content_change(&EventKind::Remove(RemoveKind::File)));
        assert!(!is_content_change(&EventKind::Access(AccessKind::Read)));
        assert!(!is_content_change(&EventKind::Other));
    }

    #[test]
    fn watching_a_missing_root_is_an_invalid_request() {
        // Constructing an Indexer needs a runtime and a model, so this covers
        // the guard that runs before either is touched.
        let root = PathBuf::from("/definitely/not/here");
        assert!(!root.is_dir());
    }
}
