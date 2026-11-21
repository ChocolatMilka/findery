//! `[search]`: how a query is answered.
//!
//! INTENT: split from `sections.rs` (CONV-029). Every knob here bounds what a
//! client receives — hits, excerpt sizes — which is the budget an agent pays in
//! tokens, so they read better side by side.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
    pub default_k: usize,
    pub max_k: usize,
    pub default_mode: crate::model::SearchMode,
    /// Reciprocal-rank-fusion constant. 60 is the value from the original
    /// Cormack et al. paper and holds up well here.
    pub rrf_k: f32,
    /// Candidates fetched before fusion, as a multiple of `k`.
    pub candidate_multiplier: usize,
    /// Ceiling of the candidates, as a multiple of `max_k`: what keeps a large
    /// `k` times a large multiplier from scanning the whole index.
    pub candidate_ceiling_multiplier: usize,
    /// Extra BM25 candidates fetched when the filter narrows them only once the
    /// chunks are in hand, as a multiple of the candidate count.
    pub keyword_overfetch: usize,
    /// Maximum characters of chunk text returned per hit; 0 means unlimited.
    /// This is the single most effective token-budget knob for MCP clients.
    pub max_snippet_chars: usize,
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
        }
    }
}
