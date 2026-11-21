//! `[search]`: how a query is answered, and how much of the index a read may
//! hand back.
//!
//! INTENT: split from `sections.rs` (CONV-029). Every knob here bounds what a
//! client receives — hits, excerpt sizes, a read's lines — which is the budget
//! an agent pays in tokens, so they read better side by side.

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
    pub default_k: usize,
    pub max_k: usize,
    pub default_mode: crate::model::SearchMode,
    /// Reciprocal-rank-fusion constant. 60 is the value from the original
    /// Cormack et al. paper and holds up well here.
    pub rrf_k: f32,
    /// Candidates fetched per leg before fusion, as a multiple of `k`.
    pub candidate_multiplier: usize,
    /// Ceiling of a leg's candidates, as a multiple of `max_k`: what keeps a
    /// large `k` times a large multiplier from scanning the whole index.
    pub candidate_ceiling_multiplier: usize,
    /// Extra BM25 candidates a leg fetches when its filter narrows them only
    /// once the chunks are in hand, as a multiple of its candidate count.
    pub keyword_overfetch: usize,
    /// Maximum characters of chunk text returned per hit; 0 means unlimited.
    /// This is the single most effective token-budget knob for MCP clients.
    pub max_snippet_chars: usize,
    /// Ceiling of a rendered intent instruction, in bytes. The sentence is
    /// always kept whole; per-language hints are appended in priority order
    /// while they fit. `0` appends none.
    pub max_instruction_chars: usize,
    /// A paired hit (doc-sync, divergence) whose best match is less similar
    /// than this, by stored-vector cosine, is flagged `drifted`.
    pub drift_below: f32,
    /// How many hits of the second leg each hit of the first is paired with.
    pub pair_matches: usize,
    /// Two hits of different files at least this similar are near-duplicates…
    pub duplicate_min: f32,
    /// …unless they are more similar than this: identical copies, not copies
    /// that diverged.
    pub duplicate_max: f32,
    /// Most context lines a read may add on each side of its target.
    pub max_read_context: u32,
    /// Most lines one read returns; a longer range is cut and says so.
    pub max_read_lines: u32,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            default_k: 10,
            max_k: 100,
            default_mode: crate::model::SearchMode::Hybrid,
            rrf_k: 60.0,
            candidate_multiplier: 4,
            candidate_ceiling_multiplier: 8,
            keyword_overfetch: 2,
            max_snippet_chars: 2_400,
            max_instruction_chars: crate::intent::DEFAULT_MAX_INSTRUCTION_CHARS,
            drift_below: 0.5,
            pair_matches: 2,
            duplicate_min: 0.85,
            duplicate_max: 0.98,
            max_read_context: 200,
            max_read_lines: 1_000,
        }
    }
}

impl SearchConfig {
    /// The similarity thresholds are cosines, and the duplicate band must be
    /// a band.
    pub(crate) fn validate_similarities(&self) -> Result<()> {
        let cosine = |v: f32| (-1.0..=1.0).contains(&v);
        if !cosine(self.drift_below) {
            return Err(Error::Config(
                "search.drift_below is a cosine, within [-1, 1]".into(),
            ));
        }
        let band = cosine(self.duplicate_min)
            && cosine(self.duplicate_max)
            && self.duplicate_min < self.duplicate_max;
        if !band {
            return Err(Error::Config(
                "search.duplicate_min must be < search.duplicate_max, both within [-1, 1]".into(),
            ));
        }
        if self.max_read_lines == 0 {
            return Err(Error::Config(
                "search.max_read_lines must be at least 1".into(),
            ));
        }
        Ok(())
    }
}
