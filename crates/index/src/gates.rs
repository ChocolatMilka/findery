//! Per-project gates and the leases operations hold through them.
//!
//! INTENT: `detach` and `rm` must be able to say "nothing touches this project
//! any more" before they close or delete a file. Every operation on a project
//! therefore runs under a [`Lease`], which holds the read side of the project's
//! gate; draining takes the write side, so it waits for the operations in
//! flight and keeps new ones out. A long sync is asked to stop through the
//! gate's cancel requests, which it reads between files and before every flush.
//!
//! The gate also knows which task holds each lease: a drain past its timeout
//! may abort a job, but only when the job is what holds the door — aborting it
//! for a search that will not let go would cost the job and free nothing.
//!
//! A lease is also where an operation learns which vector space it works in:
//! the project's pin or the default model, its recipe, and the member file of
//! the project's bucket that holds it. A write creates that member; a read
//! never creates a file.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::{OwnedMutexGuard, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};
use tokio::task::Id as TaskId;

use findery_core::embedding::recipe::Recipe;
use findery_core::ids::{ModelAlias, ProjectId};
use findery_core::model::Project;
use findery_core::traits::Embedder;
use findery_core::{Error, Result};

use crate::Indexer;
use crate::models::EmbedderPool;
use crate::storage::families::{BucketFamily, BucketHandle};

/// Whether an operation may create the member file of its model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Reads, and deletions that only remove what exists: never creates a file.
    Read,
    /// Writes vectors: creates the member, seeded with the project's row.
    Write,
}

/// The door every operation on one project goes through.
pub struct ProjectGate {
    /// `true` while the project is attached. A drain holds the write side and
    /// leaves it `false` once it commits.
    open: Arc<RwLock<bool>>,
    /// How many drains are asking operations to stop. A counter, not a flag:
    /// one drain giving up must not withdraw another's request.
    cancels: AtomicUsize,
    /// The command that asked last, for the `Cancelled` message.
    by: Mutex<String>,
    /// Every lease held right now, and the task holding it when there is one.
    holders: Mutex<HashMap<u64, Option<TaskId>>>,
    next_holder: AtomicU64,
    /// Taken by a sync for its whole pass: two passes over one project queue,
    /// they never write side by side (CONV-024).
    turn: Arc<tokio::sync::Mutex<()>>,
}

impl ProjectGate {
    fn new() -> Self {
        Self {
            open: Arc::new(RwLock::new(true)),
            cancels: AtomicUsize::new(0),
            by: Mutex::new(String::new()),
            holders: Mutex::new(HashMap::new()),
            next_holder: AtomicU64::new(0),
            turn: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Asks every operation in flight to stop at its next check, until the
    /// returned request is dropped.
    pub(crate) fn request_cancel(self: &Arc<Self>, by: &str) -> CancelRequest {
        *self.by.lock() = by.to_string();
        self.cancels.fetch_add(1, Ordering::SeqCst);
        CancelRequest { gate: self.clone() }
    }

    fn is_cancelled(&self) -> bool {
        self.cancels.load(Ordering::SeqCst) > 0
    }

    /// The write side, waited for — every lease released.
    pub(crate) async fn close_door(&self) -> OwnedRwLockWriteGuard<bool> {
        self.open.clone().write_owned().await
    }

    /// The read side, recorded with the task that holds it.
    async fn enter(self: &Arc<Self>) -> Held {
        let guard = self.open.clone().read_owned().await;
        let key = self.next_holder.fetch_add(1, Ordering::SeqCst);
        self.holders.lock().insert(key, tokio::task::try_id());
        Held {
            gate: self.clone(),
            key,
            guard,
        }
    }

    /// True when `task` holds a lease and nothing else does.
    pub(crate) fn held_only_by(&self, task: TaskId) -> bool {
        let holders = self.holders.lock();
        !holders.is_empty() && holders.values().all(|holder| *holder == Some(task))
    }

    /// Waits for the project's pass to be the only one writing it.
    pub(crate) async fn take_turn(&self) -> OwnedMutexGuard<()> {
        self.turn.clone().lock_owned().await
    }
}

/// A drain's request that operations stop, withdrawn when dropped.
pub(crate) struct CancelRequest {
    gate: Arc<ProjectGate>,
}

impl Drop for CancelRequest {
    fn drop(&mut self) {
        self.gate.cancels.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One lease's hold on the read side, forgotten by the gate when dropped.
struct Held {
    gate: Arc<ProjectGate>,
    key: u64,
    guard: OwnedRwLockReadGuard<bool>,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.gate.holders.lock().remove(&self.key);
    }
}

/// One operation's hold on a project, and the vector space it works in.
pub struct Lease {
    /// The project's freshest row, `model` stamped.
    pub project: Project,
    /// The model profile this operation uses: the pin, else the default.
    pub alias: ModelAlias,
    /// That profile's vector space, and so the member file it reads and writes.
    pub recipe: Recipe,
    /// The member file holding `recipe`'s vectors. Always present for a write.
    pub member: Option<Arc<BucketHandle>>,
    /// The project's bucket, every member of it.
    pub family: Arc<BucketFamily>,
    pool: Arc<EmbedderPool>,
    held: Held,
}

impl Lease {
    /// The embedder of this lease's model, built on first use. Refused when it
    /// turns out to serve another vector space than the one the lease resolved
    /// from a remembered width — writing those vectors here would mix two.
    pub async fn embedder(&self) -> Result<Arc<dyn Embedder>> {
        let embedder = self.pool.embedder(&self.alias).await?;
        let served = self.pool.recipe_of(&self.alias)?;
        if served != self.recipe {
            return Err(Error::recipe_drift(
                self.alias.as_str(),
                served.as_str(),
                self.recipe.as_str(),
            ));
        }
        Ok(embedder)
    }

    /// `(batch_size, concurrency)` of this lease's model: how a flush slices
    /// what it embeds.
    pub fn batching(&self) -> Result<(usize, usize)> {
        self.pool.batching(&self.alias)
    }

    /// The member a write goes to.
    pub fn writer(&self) -> Result<&Arc<BucketHandle>> {
        self.member.as_ref().ok_or_else(|| {
            Error::Internal(format!(
                "a read lease on `{}` was asked to write",
                self.project.id
            ))
        })
    }

    /// `Cancelled` while a drain asks this project to stop.
    pub fn check_cancelled(&self) -> Result<()> {
        let gate = &self.held.gate;
        if gate.is_cancelled() {
            return Err(Error::cancelled(&self.project.id, &gate.by.lock()));
        }
        Ok(())
    }
}

impl Indexer {
    /// The gate of a project, created open on first use.
    pub(crate) fn gate(&self, id: &ProjectId) -> Arc<ProjectGate> {
        self.gates
            .lock()
            .entry(id.clone())
            .or_insert_with(|| Arc::new(ProjectGate::new()))
            .clone()
    }

    /// Forgets a project's gate once a drain committed: the next attachment
    /// starts with a fresh, open one.
    pub(crate) fn drop_gate(&self, id: &ProjectId) {
        self.gates.lock().remove(id);
    }

    /// Resolves an operation on `id`: waits at the project's gate, reads its
    /// freshest row, picks its model (pin, else default), and the member of
    /// that model's recipe — created for a write, never for a read.
    pub async fn lease(&self, id: &ProjectId, access: Access) -> Result<Lease> {
        self.home_of(id)?;
        let held = self.gate(id).enter().await;
        if !*held.guard {
            return Err(Error::NotFound(format!("project `{id}`")));
        }
        // Read again behind the gate: a drain that committed while this one
        // waited moved or removed the home.
        let family = self.family(&self.home_of(id)?)?;
        let Some(mut project) = family.freshest_row(id).await? else {
            return Err(Error::NotFound(format!("project `{id}`")));
        };
        let alias = self.pool.resolve(project.spec.model.as_ref())?;
        let (recipe, member) = match access {
            Access::Read => {
                let recipe = self.pool.resolve_recipe(&alias).await?;
                let member = family.member(&recipe.tag());
                (recipe, member)
            }
            Access::Write => {
                let recipe = self.pool.write_recipe(&alias).await?;
                let member = self.writable_member(&family, &recipe, &project).await?;
                (recipe, Some(member))
            }
        };
        project.model = Some(alias.clone());
        Ok(Lease {
            project,
            alias,
            recipe,
            member,
            family,
            pool: self.pool.clone(),
            held,
        })
    }

    /// The member of `recipe` in `family`, created and seeded when missing.
    ///
    /// INTENT: the seed is the project's ROW, copied from the freshest sibling,
    /// and nothing else. Copying `files` and `chunks` would make gate 1 skip
    /// every file on the next sync and leave chunks with no vector under the new
    /// model; with the row alone, that sync re-chunks (pure and cheap) and embeds
    /// exactly what the new model has never seen.
    pub(crate) async fn writable_member(
        &self,
        family: &BucketFamily,
        recipe: &Recipe,
        project: &Project,
    ) -> Result<Arc<BucketHandle>> {
        let member = family.ensure_member(&self.cfg.store, recipe).await?;
        if member.bucket.get_project(&project.id).await?.is_none() {
            member.bucket.put_project(project).await?;
        }
        Ok(member)
    }

    /// Holds a project still for a registration: the write side of its gate,
    /// waited for the way a drain waits — every lease in flight let go first —
    /// but asking nothing to stop, and within `server.drain_timeout_secs`.
    ///
    /// INTENT: a registration may move the project's rows to another bucket,
    /// and a sync writing them meanwhile would leave chunks and vectors behind
    /// in the bucket it left.
    pub(crate) async fn hold_still(&self, id: &ProjectId) -> Result<OwnedRwLockWriteGuard<bool>> {
        let wait = Duration::from_secs(self.cfg.server.drain_timeout_secs);
        loop {
            let gate = self.gate(id);
            let Ok(door) = tokio::time::timeout(wait, gate.close_door()).await else {
                return Err(self.busy(id, wait));
            };
            if *door {
                return Ok(door);
            }
            // A drain committed while this one waited: its gate is gone from
            // the map, and the next one is open.
        }
    }

    /// The refusal for a project something held past `waited`.
    pub(crate) fn busy(&self, id: &ProjectId, waited: Duration) -> Error {
        match self.jobs.running_for(id) {
            Some((job, _)) => Error::busy_job(&job, id, waited),
            None => Error::busy_project(id, waited),
        }
    }
}
