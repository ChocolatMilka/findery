//! The live counters of a running job.
//!
//! A callback would be an edge climbing back up the layers (CONV-005), so there
//! is none: the registry hands the sync a *handle* it writes into and reads back
//! when a client asks. The value goes down as an argument and comes up as a
//! return value. The handle holds no `Indexer` and no store, so it cannot grow
//! into a second engine.
//!
//! The handle also owns the clock: every write and every read stamps
//! `elapsed_ms`, so a snapshot is as old as the moment it is taken, not the
//! moment the sync last wrote.

use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;

use findery_core::api::{Progress, ProgressPhase};

#[derive(Default)]
struct Shared {
    progress: Progress,
    /// When the sync began; `None` while the job waits for a slot.
    started: Option<Instant>,
}

impl Shared {
    fn stamp(&mut self) {
        if let Some(started) = self.started {
            self.progress.elapsed_ms = started.elapsed().as_millis() as u64;
        }
    }
}

/// A shared, writable view of one job's counters.
///
/// Cloned cheaply; every clone points at the same counters.
#[derive(Clone, Default)]
pub struct ProgressHandle(Arc<Mutex<Shared>>);

impl ProgressHandle {
    /// A handle nobody reads, for the callers that have no client watching — so
    /// the sync is written once instead of forked in two.
    pub fn detached() -> Self {
        Self::default()
    }

    /// Starts the clock and clears the counters: the sync begins now.
    pub fn begin(&self) {
        let mut shared = self.0.lock();
        *shared = Shared {
            started: Some(Instant::now()),
            ..Shared::default()
        };
    }

    /// Mutates the counters under the lock, `elapsed_ms` already current. Keep
    /// the closure trivial: it runs on the sync's hot path.
    pub fn write(&self, update: impl FnOnce(&mut Progress)) {
        let mut shared = self.0.lock();
        shared.stamp();
        update(&mut shared.progress);
    }

    /// The counters as a client would see them, right now.
    pub fn snapshot(&self) -> Progress {
        let mut shared = self.0.lock();
        shared.stamp();
        shared.progress.clone()
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
            p.files_pending = 12;
        });
        assert_eq!(reader.snapshot().files_done, 3);
        assert_eq!(reader.snapshot().files_pending, 12);
    }

    /// A detached handle is a real handle: writing to it must not panic, which
    /// is the whole point of using one for the callers nobody watches.
    #[test]
    fn a_detached_handle_absorbs_writes_without_a_reader() {
        let handle = ProgressHandle::detached();
        handle.enter(ProgressPhase::Pruning);
        assert_eq!(handle.snapshot().phase, ProgressPhase::Pruning);
    }

    /// The elapsed time a client reads is the time now, even when the sync
    /// has not written for a while — a stalled clock reads as a hung job.
    #[test]
    fn a_snapshot_carries_the_time_it_was_taken() {
        let handle = ProgressHandle::detached();
        assert_eq!(handle.snapshot().elapsed_ms, 0, "a queued job has no clock");
        handle.begin();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(handle.snapshot().elapsed_ms >= 20);
    }

    #[test]
    fn beginning_clears_what_an_earlier_pass_left() {
        let handle = ProgressHandle::detached();
        handle.write(|p| p.files_done = 9);
        handle.begin();
        assert_eq!(handle.snapshot().files_done, 0);
    }
}
