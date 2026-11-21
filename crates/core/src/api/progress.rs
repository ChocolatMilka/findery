//! The live counters of a running sync, and the arithmetic a watcher needs
//! from them: a proportion, a rate, an ETA, a one-line summary.
//!
//! INTENT: the rate and the ETA are computed here, in L0 and purely, so the
//! daemon, the CLI's push mode and an agent reading `GET /v1/jobs` all derive
//! them the same way from the same counters.

use serde::{Deserialize, Serialize};

/// How fast the rate forgets: a sample this old weighs `1/e` of a fresh one.
/// Long enough to smooth two sub-batches landing together, short enough to
/// follow a runtime that slows down.
const RATE_MEMORY_MS: f64 = 5_000.0;

/// What a running job is doing, in the order the phases happen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressPhase {
    /// Walking the project: only `files_seen` moves.
    #[default]
    Scanning,
    /// Reading, chunking and embedding what gate 1 could not clear.
    Indexing,
    /// Dropping what the walk no longer sees.
    Pruning,
}

impl ProgressPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Scanning => "scanning",
            Self::Indexing => "indexing",
            Self::Pruning => "pruning",
        }
    }
}

/// Live counters of a running job.
///
/// The work is planned before it is done: the walk streams `files_seen`, then
/// gate 1 runs over the whole list, so `files_total`, `files_unchanged` and
/// `files_pending` are all known before the first file is read. `files_done`
/// counts pending files whose outcome is decided, and ends equal to
/// `files_pending`.
///
/// A terminal job carries none: its numbers are its `SyncReport`, and two
/// records of one fact are free to disagree. Every field defaults, so a client
/// reads an older daemon's progress and the reverse.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Progress {
    pub phase: ProgressPhase,
    /// Files the walk accepted so far.
    pub files_seen: u64,
    /// Every file the walk accepted. Zero while scanning.
    pub files_total: u64,
    /// Files gate 1 cleared without reading them.
    pub files_unchanged: u64,
    /// Files left to read: `files_total - files_unchanged`.
    pub files_pending: u64,
    /// Pending files whose outcome is decided: unchanged bytes, written, or
    /// reported as a warning.
    pub files_done: u64,
    pub chunks_done: u64,
    /// Cache misses found so far: the chunks this pass has to embed.
    pub chunks_pending_embed: u64,
    pub embeddings_computed: u64,
    pub embeddings_cached: u64,
    /// Chunks embedded per second, smoothed. Zero until the first batch lands.
    pub rate: f64,
    /// Time left to embed the misses found so far; absent without a rate, or
    /// with no misses to count down from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta_ms: Option<u64>,
    pub elapsed_ms: u64,
    /// Where the rate was last sampled. Bookkeeping, never on the wire.
    #[serde(skip)]
    pub meter: RateMeter,
}

/// Where a rate was last sampled: `(elapsed_ms, embeddings_computed)`.
/// Opaque, so only [`Progress`] moves it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RateMeter(Option<(u64, u64)>);

impl Progress {
    /// How far along the pending files are, in `[0, 1]`. Zero while nothing is
    /// planned: `0 / 0` is not a proportion.
    pub fn fraction(&self) -> f32 {
        if self.files_pending == 0 {
            return 0.0;
        }
        (self.files_done as f32 / self.files_pending as f32).clamp(0.0, 1.0)
    }

    /// Records `misses` more chunks to embed, and starts the rate's clock at
    /// the first of them, so the time spent walking does not count against it.
    pub fn expect_embeddings(&mut self, misses: u64) {
        self.chunks_pending_embed += misses;
        if misses > 0 && self.meter.0.is_none() {
            self.meter.0 = Some((self.elapsed_ms, self.embeddings_computed));
        }
        self.refresh_eta();
    }

    /// Records `embedded` chunks that just came back from the runtime, at
    /// `elapsed_ms`, and updates the rate and the ETA.
    ///
    /// INTENT: the smoothing is weighted by time, not by sample: two batches
    /// landing a millisecond apart move the rate by a millisecond's worth
    /// instead of reading as a burst of thousands per second.
    pub fn observe(&mut self, embedded: u64) {
        self.embeddings_computed += embedded;
        let (at, before) = self.meter.0.unwrap_or((0, 0));
        let dt = self.elapsed_ms.saturating_sub(at);
        if dt > 0 {
            let landed = self.embeddings_computed.saturating_sub(before);
            let instant = landed as f64 * 1000.0 / dt as f64;
            self.rate = if self.rate == 0.0 {
                instant
            } else {
                let weight = 1.0 - (-(dt as f64) / RATE_MEMORY_MS).exp();
                self.rate + weight * (instant - self.rate)
            };
            self.meter.0 = Some((self.elapsed_ms, self.embeddings_computed));
        }
        self.refresh_eta();
    }

    fn refresh_eta(&mut self) {
        let left = self
            .chunks_pending_embed
            .saturating_sub(self.embeddings_computed);
        let known = self.rate > 0.0 && self.chunks_pending_embed > 0;
        self.eta_ms = known.then(|| (left as f64 * 1000.0 / self.rate) as u64);
    }

    /// The one-line description a log, a job list or an agent prints.
    pub fn summary(&self) -> String {
        let mut parts = vec![self.phase.as_str().to_string()];
        if self.phase == ProgressPhase::Scanning {
            parts.push(format!("{} files seen", self.files_seen));
        } else {
            parts.push(format!(
                "files {}/{}, {} unchanged",
                self.files_done, self.files_pending, self.files_unchanged
            ));
        }
        if self.chunks_pending_embed > 0 || self.embeddings_cached > 0 {
            parts.push(format!(
                "embedded {}/{}, {} cached",
                self.embeddings_computed, self.chunks_pending_embed, self.embeddings_cached
            ));
        }
        if self.rate > 0.0 {
            parts.push(format!("{:.0}/s", self.rate));
        }
        if let Some(eta) = self.eta_ms {
            parts.push(format!("ETA {}s", eta.div_ceil(1000)));
        }
        parts.push(format!("{:.1}s", self.elapsed_ms as f64 / 1000.0));
        parts.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn indexing() -> Progress {
        Progress {
            phase: ProgressPhase::Indexing,
            files_total: 7200,
            files_unchanged: 6888,
            files_pending: 312,
            ..Progress::default()
        }
    }

    #[test]
    fn nothing_planned_is_zero_not_a_division_by_zero() {
        assert_eq!(Progress::default().fraction(), 0.0);
    }

    /// The proportion is over the pending files, not every walked one: an
    /// incremental pass over an unchanged tree must not start at 96%.
    #[test]
    fn the_fraction_is_over_the_pending_files_and_never_exceeds_one() {
        let mut p = indexing();
        p.files_done = 78;
        assert!((p.fraction() - 0.25).abs() < f32::EPSILON);
        p.files_done = 400;
        assert_eq!(p.fraction(), 1.0);
    }

    #[test]
    fn the_rate_starts_when_there_is_something_to_embed() {
        let mut p = indexing();
        p.elapsed_ms = 3_000; // the walk took three seconds
        p.expect_embeddings(200);
        p.elapsed_ms = 4_000;
        p.observe(50);
        assert!((p.rate - 50.0).abs() < 1e-9, "{}", p.rate);
        assert_eq!(p.eta_ms, Some(3_000), "150 left at 50/s");
    }

    /// Two batches landing together must not read as a burst.
    #[test]
    fn a_batch_landing_right_after_another_barely_moves_the_rate() {
        let mut p = indexing();
        p.expect_embeddings(1_000);
        p.elapsed_ms = 1_000;
        p.observe(32);
        p.elapsed_ms = 1_001;
        p.observe(32);
        assert!(
            p.rate < 40.0,
            "a one-millisecond sample moved the rate to {}",
            p.rate
        );
    }

    #[test]
    fn no_rate_means_no_eta() {
        let mut p = indexing();
        p.expect_embeddings(10);
        assert_eq!(p.eta_ms, None);
        assert_eq!(p.rate, 0.0);
    }

    /// A count with no denominator — a client pushing, which never learns
    /// the misses — has a rate and no ETA.
    #[test]
    fn a_rate_without_known_misses_has_no_eta() {
        let mut p = indexing();
        p.elapsed_ms = 2_000;
        p.observe(64);
        assert!((p.rate - 32.0).abs() < 1e-9, "{}", p.rate);
        assert_eq!(p.eta_ms, None);
    }

    #[test]
    fn an_older_daemons_progress_still_deserializes() {
        let p: Progress = serde_json::from_str(
            r#"{"phase":"indexing","files_done":3,"files_total":9,"chunks_done":1,
                "embeddings_computed":0,"embeddings_cached":0,"elapsed_ms":10}"#,
        )
        .expect("parse");
        assert_eq!(p.files_total, 9);
        assert_eq!(p.files_pending, 0);
        assert!(p.eta_ms.is_none());
    }

    #[test]
    fn the_bookkeeping_sample_never_reaches_the_wire() {
        let mut p = indexing();
        p.expect_embeddings(5);
        let json = serde_json::to_string(&p).expect("json");
        assert!(!json.contains("meter"), "{json}");
    }

    #[test]
    fn a_scanning_summary_counts_what_the_walk_has_seen() {
        let p = Progress {
            files_seen: 4120,
            elapsed_ms: 1500,
            ..Progress::default()
        };
        assert_eq!(p.summary(), "scanning · 4120 files seen · 1.5s");
    }

    #[test]
    fn an_indexing_summary_names_both_denominators() {
        let mut p = indexing();
        p.files_done = 312;
        p.expect_embeddings(2300);
        p.embeddings_cached = 5937;
        p.elapsed_ms = 10_000;
        p.observe(380);
        assert_eq!(
            p.summary(),
            "indexing · files 312/312, 6888 unchanged · embedded 380/2300, 5937 cached · 38/s \
             · ETA 51s · 10.0s"
        );
    }
}
