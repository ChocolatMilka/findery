//! The live counters of a running job.
//!
//! A callback would be an edge climbing back up the layers (CONV-005), so there
//! is none: the registry hands the sync a *handle* it writes into and reads back
//! when a client asks. The value goes down as an argument and comes up as a
//! return value. The handle holds no `Indexer` and no store, so it cannot grow
//! into a second engine.

use findery_core::api::{Progress, ProgressPhase};
use parking_lot::Mutex;
use std::sync::Arc;

/// How often a pass reports file progress when no flush happens first. A pass
/// where nothing changed flushes nothing, and a bar driven only by flushes would
/// sit still for all of it; counting every file would take the lock 50k times to
/// move a bar nobody can see move.
pub const PROGRESS_FILES: u64 = 512;

/// A shared, writable view of one job's counters.
///
/// Cloned cheaply; every clone points at the same counters.
#[derive(Clone, Default)]
pub struct ProgressHandle(Arc<Mutex<Progress>>);

impl ProgressHandle {
    /// A handle nobody reads, for the callers that have no client watching — so
    /// the sync is written once instead of forked in two.
    pub fn detached() -> Self {
        Self::default()
    }

    /// Mutates the counters under the lock. Keep the closure trivial: it runs on
    /// the sync's hot path.
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
