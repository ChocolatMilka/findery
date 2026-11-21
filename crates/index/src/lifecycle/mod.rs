//! Taking projects and buckets off the daemon: `detach`, `rm`, `bucket rm`.
//!
//! INTENT: each verb ends with a file the user wants to do something with, so
//! each one must first be able to say "nothing of findery touches this any
//! more". That is the drain: ask the project's sync to stop, then take the
//! write side of its gate — which waits for every lease in flight and keeps
//! new ones out. A drain that cannot finish within `server.drain_timeout_secs`
//! aborts the project's job when the job is what holds the gate, waits
//! `server.abort_grace_secs` more, and otherwise gives up with `busy` **having
//! changed nothing**. Committed, the gate stays shut and the watcher stops; a
//! sync the watcher queued meanwhile meets the shut gate and ends `not_found`.
//!
//! The destructive verbs then follow one order — `removal`'s — so a removal
//! cut short is resumed rather than half-forgotten: the tombstone in
//! `buckets.toml` first, the vectors and rows next, the files after, the
//! registry entry last.

mod bucket;
mod project;
pub(crate) mod removal;
pub(crate) mod tombstones;

use std::time::Duration;

use tokio::sync::OwnedRwLockWriteGuard;

use findery_core::ids::ProjectId;
use findery_core::{Error, Result};

use crate::Indexer;
use crate::gates::{CancelRequest, ProjectGate};

/// A project nothing touches any more, until this is committed or dropped.
///
/// Dropped without [`Drained::commit`], the gate reopens and the cancel
/// request is withdrawn: a verb that fails after draining has changed nothing.
pub(crate) struct Drained {
    id: ProjectId,
    door: OwnedRwLockWriteGuard<bool>,
    _cancel: CancelRequest,
}

impl Drained {
    /// Shuts the door for good: every lease waiting on it gets `NotFound`, and
    /// the next attachment starts with a fresh gate — forgotten before the door
    /// opens, so a waiter never meets this dead one twice.
    fn commit(mut self, idx: &Indexer) {
        idx.drop_gate(&self.id);
        idx.stop_watch(&self.id);
        *self.door = false;
    }
}

impl Indexer {
    /// Waits until nothing touches `id`, asking its sync to stop and, past the
    /// timeout, aborting its job. `by` names the command, for the job's
    /// `Cancelled` message.
    pub(crate) async fn drain(&self, id: &ProjectId, by: &str) -> Result<Drained> {
        let gate = self.gate(id);
        let cancel = gate.request_cancel(by);
        let wait = Duration::from_secs(self.cfg.server.drain_timeout_secs);
        let door = match tokio::time::timeout(wait, gate.close_door()).await {
            Ok(door) => door,
            Err(_) => self.abort_the_holder(id, &gate, by, wait).await?,
        };
        if !*door {
            // Another drain of the same project committed first.
            return Err(Error::NotFound(format!("project `{id}`")));
        }
        Ok(Drained {
            id: id.clone(),
            door,
            _cancel: cancel,
        })
    }

    /// Past the timeout: aborts the project's job when it is the only thing
    /// holding the gate, then waits `server.abort_grace_secs` for the door.
    /// Anything else holding it — a search, a sync started inline — is not the
    /// drain's to abort, and aborting the job beside it would free nothing.
    async fn abort_the_holder(
        &self,
        id: &ProjectId,
        gate: &ProjectGate,
        by: &str,
        waited: Duration,
    ) -> Result<OwnedRwLockWriteGuard<bool>> {
        let Some((job, task)) = self.jobs.running_for(id) else {
            return Err(Error::busy_project(id, waited));
        };
        if !gate.held_only_by(task.id()) {
            return Err(Error::busy_job(&job, id, waited));
        }
        task.abort();
        // Recorded now rather than when the task drops: whoever polls the job
        // next sees it ended.
        self.jobs.fail(&job, &Error::cancelled(id, by));
        self.jobs.untrack(&job);
        let grace = Duration::from_secs(self.cfg.server.abort_grace_secs);
        tokio::time::timeout(grace, gate.close_door())
            .await
            .map_err(|_| Error::busy_job(&job, id, waited))
    }

    /// Drains every project of `ids`, or none: the first that cannot be
    /// drained releases the others.
    async fn drain_all(&self, ids: &[ProjectId], by: &str) -> Result<Vec<Drained>> {
        let mut drained = Vec::with_capacity(ids.len());
        for id in ids {
            drained.push(self.drain(id, by).await?);
        }
        Ok(drained)
    }
}

/// Project ids as a reader sees them in a refusal: `a`, `b`.
pub(super) fn quoted(ids: &[ProjectId]) -> String {
    ids.iter()
        .map(|id| format!("`{id}`"))
        .collect::<Vec<_>>()
        .join(", ")
}
