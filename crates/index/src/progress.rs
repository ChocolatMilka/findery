//! The live counters of a running job, and who is allowed to write them.
//!
//! INTENT: progress has to travel from inside a sync loop up to a client that is
//! polling an HTTP route. A callback would be an edge climbing back up the
//! layers (CONV-005), so there is none: the registry hands the sync a *handle*
//! it writes into, and reads the same handle back when a client asks. The value
//! goes down as an argument and comes back up as a return value — the law holds,
//! and no crate edge is added.
//!
//! One writer, one reader, one lock. The handle deliberately holds no `Indexer`
//! and no store, so it cannot keep anything alive or grow into a second engine.

use findery_core::api::{Progress, ProgressPhase};
use parking_lot::Mutex;
use std::sync::Arc;

/// How often a pass reports file progress.
///
/// INTENT: a run where every file is unchanged performs no flush at all, so a
/// bar driven only by flushes would sit still for the whole pass. Counting every
/// file would instead take the lock fifty thousand times to move a bar nobody
/// can see move. This is the compromise, and it is small enough that a flush —
/// every 96 files — dominates on a real index.
pub const PROGRESS_FILES: u64 = 512;

/// A shared, writable view of one job's counters.
///
/// Cloned cheaply; every clone points at the same counters.
#[derive(Clone, Default)]
pub struct ProgressHandle(Arc<Mutex<Progress>>);

impl ProgressHandle {
    /// A handle nobody reads.
    ///
    /// INTENT: the sync is one function with several callers, and only the job
    /// path has a client watching. Rather than fork the engine in two, the
    /// callers nobody watches pass this: the sync is written once, and the
    /// absence of a reader costs one uncontended lock.
    pub fn detached() -> Self {
        Self::default()
    }

    /// Mutates the counters under the lock. The closure must stay trivial —
    /// it runs on the sync's hot path.
    pub fn write(&self, update: impl FnOnce(&mut Progress)) {
        update(&mut self.0.lock());
    }

    /// The counters as a client would see them.
    pub fn snapshot(&self) -> Progress {
        self.0.lock().clone()
    }

    /// Moves every future write into `phase`.
    pub fn enter(&self, phase: ProgressPhase) {
        self.write(|p| p.phase = phase);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clone_shares_the_counters_it_was_made_from() {
        let handle = ProgressHandle::detached();
        let reader = handle.clone();
        handle.write(|p| {
            p.files_done = 3;
            p.files_total = 12;
        });
        assert_eq!(reader.snapshot().files_done, 3);
        assert_eq!(reader.snapshot().files_total, 12);
    }

    /// The walk is what establishes the denominator, so a bar drawn before it
    /// returns must read 0% rather than dividing by zero.
    #[test]
    fn an_unknown_total_is_not_a_division_by_zero() {
        let handle = ProgressHandle::detached();
        assert_eq!(handle.snapshot().fraction(), 0.0);
        handle.write(|p| p.files_total = 4);
        handle.write(|p| p.files_done = 1);
        assert!((handle.snapshot().fraction() - 0.25).abs() < f32::EPSILON);
    }

    /// A detached handle is a real handle: writing to it must not panic, which
    /// is the whole point of using one for the callers nobody watches.
    #[test]
    fn a_detached_handle_absorbs_writes_without_a_reader() {
        let handle = ProgressHandle::detached();
        handle.enter(ProgressPhase::Pruning);
        assert_eq!(handle.snapshot().phase, ProgressPhase::Pruning);
    }
}
