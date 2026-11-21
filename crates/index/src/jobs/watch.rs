//! Filesystem watching: keep an index live without anyone asking.
//!
//! INTENT: editors write files in bursts — a save, a formatter, a build script
//! touching a dozen generated modules. Reacting per event would re-embed the
//! same file three times in a second, so events are coalesced per path and only
//! flushed after the directory has been quiet for `debounce`.
//!
//! The watchers belong to the engine, not to a transport: `detach` and `rm`
//! stop a project's watcher when they commit, and the watcher's own syncs are
//! jobs like any other — never a pass running beside one, racing it over the
//! same files. A path the project's walk rules refuse (`.findery/`, an
//! exclude) is dropped on arrival: a bucket inside the tree writes on every
//! sync, and syncing its writes would never end.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Duration;

use notify::{Event, EventKind, RecursiveMode, Watcher as _};

use findery_core::api::{WatchRequest, Watching};
use findery_core::ids::ProjectId;
use findery_core::model::{Project, SyncMode};
use findery_core::{Error, Result};

use crate::Indexer;
use crate::jobs::JobState;

/// The narrowest and widest quiet period a client may ask for.
const DEBOUNCE_MS: (u64, u64) = (100, 60_000);

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

impl Indexer {
    /// Turns watching on — replacing a watcher already there — or off.
    pub async fn set_watch(
        self: &Arc<Self>,
        id: &ProjectId,
        req: &WatchRequest,
    ) -> Result<Watching> {
        if !req.enabled {
            self.stop_watch(id);
            return Ok(Watching {
                project: id.to_string(),
                watching: false,
            });
        }
        let project = self.get_project(id).await?;
        let debounce = Duration::from_millis(req.debounce_ms.clamp(DEBOUNCE_MS.0, DEBOUNCE_MS.1));
        let handle = spawn(Arc::downgrade(self), project, debounce)?;
        // Replacing an existing handle drops it, which stops the previous task.
        self.watches.lock().insert(id.clone(), handle);
        Ok(Watching {
            project: id.to_string(),
            watching: true,
        })
    }

    /// Stops a project's watcher, when it has one.
    pub(crate) fn stop_watch(&self, id: &ProjectId) -> bool {
        self.watches.lock().remove(id).is_some()
    }
}

/// Starts watching `project`'s root and re-syncing touched paths.
///
/// The task holds the engine weakly: the engine owns the handle that stops the
/// task, and a strong reference back would keep both alive forever.
pub fn spawn(indexer: Weak<Indexer>, project: Project, debounce: Duration) -> Result<WatchHandle> {
    let root = PathBuf::from(&project.spec.root);
    if !root.is_dir() {
        return Err(Error::Invalid(format!(
            "cannot watch `{}`: not a directory",
            root.display()
        )));
    }

    // notify calls back from its own thread, so the bridge to async is an
    // unbounded channel; the debounce below is what keeps it from flooding.
    let rules = crate::walker::Rules::new(&project.spec)?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<PathBuf>>();
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

    let (shutdown, stop_rx) = tokio::sync::watch::channel(false);
    let label = project.id.to_string();
    let watched = Watched {
        indexer,
        project: project.id,
        root,
        rules,
        debounce,
    };
    tokio::spawn(watched.run(watcher, rx, stop_rx));

    Ok(WatchHandle {
        project: label,
        shutdown,
    })
}

/// One watcher's loop and what it remembers between quiet periods.
struct Watched {
    indexer: Weak<Indexer>,
    project: ProjectId,
    root: PathBuf,
    rules: crate::walker::Rules,
    debounce: Duration,
}

/// The touched paths, and the batch a job of this watcher is syncing.
#[derive(Default)]
struct Touched {
    dirty: HashSet<String>,
    /// The last job this watcher queued, and its paths: kept until it ends,
    /// and given back to `dirty` when it fails.
    in_flight: Option<(String, Vec<String>)>,
}

impl Watched {
    async fn run(
        self,
        // Held for the whole loop, or the OS handle closes underneath.
        _watcher: notify::RecommendedWatcher,
        mut events: tokio::sync::mpsc::UnboundedReceiver<Vec<PathBuf>>,
        mut stop: tokio::sync::watch::Receiver<bool>,
    ) {
        let mut touched = Touched::default();
        loop {
            tokio::select! {
                _ = stop.changed() => {
                    if *stop.borrow() {
                        tracing::info!(project = %self.project, "watch stopped");
                        return;
                    }
                }
                received = events.recv() => match received {
                    Some(paths) => self.note(&mut touched, paths),
                    None => return,
                },
                _ = tokio::time::sleep(self.debounce), if !touched.dirty.is_empty() => {
                    if !self.flush(&mut touched).await {
                        return;
                    }
                }
            }
        }
    }

    /// Keeps the touched paths the project's walk could accept.
    fn note(&self, touched: &mut Touched, paths: Vec<PathBuf>) {
        for path in paths {
            if let Some(rel) = crate::walker::relative(&self.root, &path)
                && self.rules.admits(&rel)
            {
                touched.dirty.insert(rel);
            }
        }
    }

    /// Queues a sync of the touched paths after a quiet period. `false` once
    /// the watcher has nothing left to watch.
    async fn flush(&self, touched: &mut Touched) -> bool {
        let Some(indexer) = self.indexer.upgrade() else {
            return false;
        };
        self.reclaim(&indexer, touched);
        // A job already running on the project: the touched paths wait for the
        // next quiet period rather than racing it — and are kept, not dropped.
        if indexer.jobs.has_active(self.project.as_str()).is_some() {
            return true;
        }
        match indexer.get_project(&self.project).await {
            Ok(_) => {}
            Err(Error::NotFound(_)) => {
                tracing::info!(project = %self.project, "project gone; watch stopped");
                return false;
            }
            Err(e) => {
                tracing::warn!(project = %self.project, error = %e, "watch sync deferred");
                return true;
            }
        }
        let batch: Vec<String> = touched.dirty.drain().collect();
        // Kept beside the job: a failed sync gives its paths back.
        let job = indexer.admit_sync(&self.project, SyncMode::Paths(batch.clone()));
        tracing::info!(project = %self.project, job = %job.id, "watch queued a sync");
        touched.in_flight = Some((job.id, batch));
        true
    }

    /// Forgets the last job once it ended — its paths back in `dirty` when it
    /// failed, so the next sync carries them.
    fn reclaim(&self, indexer: &Indexer, touched: &mut Touched) {
        let Some((job, _)) = &touched.in_flight else {
            return;
        };
        let state = indexer.jobs.get(job).map(|j| j.state);
        if state.is_some_and(|s| !s.is_terminal()) {
            return;
        }
        if let Some((_, batch)) = touched.in_flight.take()
            && state == Some(JobState::Failed)
        {
            touched.dirty.extend(batch);
        }
    }
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
    use notify::event::{AccessKind, CreateKind, ModifyKind, RemoveKind};

    use super::*;

    #[test]
    fn only_content_events_trigger_a_resync() {
        assert!(is_content_change(&EventKind::Create(CreateKind::File)));
        assert!(is_content_change(&EventKind::Modify(ModifyKind::Any)));
        assert!(is_content_change(&EventKind::Remove(RemoveKind::File)));
        assert!(!is_content_change(&EventKind::Access(AccessKind::Read)));
        assert!(!is_content_change(&EventKind::Other));
    }

    /// The guard runs before any watcher exists: a root that is not a
    /// directory is the caller's mistake, refused as such.
    #[tokio::test]
    async fn watching_a_missing_root_is_an_invalid_request() {
        let project = Project {
            id: ProjectId::new("ghost").expect("id"),
            name: "ghost".into(),
            bucket: findery_core::model::DEFAULT_BUCKET.into(),
            model: None,
            spec: findery_core::model::ProjectSpec {
                root: "/definitely/not/here".into(),
                ..Default::default()
            },
            host_root: String::new(),
            created_at: 0,
            updated_at: 0,
        };
        let err = match spawn(Weak::new(), project, Duration::from_millis(100)) {
            Ok(_) => panic!("a missing root cannot be watched"),
            Err(e) => e,
        };
        assert_eq!(err.code(), "invalid");
    }
}
