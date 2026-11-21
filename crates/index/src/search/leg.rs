//! One leg of a search: its vector list and the shared keyword list, fused,
//! then hydrated into hits a human can open.

use std::collections::HashMap;

use findery_core::Result;
use findery_core::ids::ChunkId;
use findery_core::model::filter::CompiledFilter;
use findery_core::model::{Chunk, Project, ScoredChunk, SearchFilter, SearchHit, SearchMode};

use super::fuse::{Fused, fuse, host_path, payload_of, truncate_text};
use crate::storage::families::BucketHandle;

/// What every leg of one search reads.
pub(super) struct Shared<'a> {
    pub(super) project: &'a Project,
    /// The member every chunk is read from: a chunk id means nothing in
    /// another file, so every list must come from this one.
    pub(super) source: &'a BucketHandle,
    /// The query's BM25 list, run once for every leg and narrowed by each.
    pub(super) keyword: &'a [ScoredChunk],
    pub(super) rrf_k: f32,
    /// `max_snippet_chars`; `0` is unlimited.
    pub(super) budget: usize,
}

/// One leg, ready to run.
pub(super) struct LegRun<'a> {
    pub(super) k: usize,
    /// Fetched from the vector store before filtering and fusion.
    pub(super) candidates: usize,
    pub(super) mode: SearchMode,
    pub(super) filter: SearchFilter,
    pub(super) compiled: CompiledFilter,
    /// The query vector; `None` when the leg has no vector list (keyword
    /// mode, or a hybrid leg whose vectors could not be had).
    pub(super) vector: Option<&'a [f32]>,
    /// A chunk the answer must not contain: the example of a `like` leg.
    pub(super) exclude: Option<ChunkId>,
}

/// Runs `leg`, best first, at most `k` hits.
pub(super) async fn run(shared: &Shared<'_>, leg: &LegRun<'_>) -> Result<Vec<SearchHit>> {
    let vector_hits = match leg.vector {
        Some(query) => {
            let store = &shared.source.store;
            let wanted = leg.candidates + usize::from(leg.exclude.is_some());
            store
                .search(&shared.project.id, query, wanted, &leg.filter)
                .await?
        }
        None => Vec::new(),
    };
    let keyword_hits = match leg.mode {
        SearchMode::Vector => &[][..],
        SearchMode::Keyword | SearchMode::Hybrid => shared.keyword,
    };
    let fused = fuse(&vector_hits, keyword_hits, shared.rrf_k, leg.mode);
    hydrate(shared, leg, fused).await
}

/// Loads the fused chunks, keeps the ones the filter accepts, and trims them.
async fn hydrate(
    shared: &Shared<'_>,
    leg: &LegRun<'_>,
    fused: Vec<Fused>,
) -> Result<Vec<SearchHit>> {
    let ids: Vec<ChunkId> = fused.iter().map(|f| f.id.clone()).collect();
    let chunks = shared
        .source
        .bucket
        .get_chunks(&shared.project.id, &ids)
        .await?;
    let mut by_id: HashMap<String, Chunk> =
        chunks.into_iter().map(|c| (c.id.to_string(), c)).collect();

    let mut out = Vec::with_capacity(leg.k);
    for scored in fused {
        if leg.exclude.as_ref() == Some(&scored.id) {
            continue;
        }
        let Some(mut chunk) = by_id.remove(scored.id.as_str()) else {
            continue;
        };
        // The keyword list carries no payload, so its filter is applied here.
        if !leg.compiled.is_pass_through() && !leg.compiled.matches(&payload_of(&chunk)) {
            continue;
        }
        if shared.budget > 0 {
            truncate_text(&mut chunk.text, shared.budget);
        }
        out.push(SearchHit {
            host_path: host_path(shared.project, &chunk.path),
            score: scored.score,
            vector_score: scored.vector_score,
            keyword_score: scored.keyword_score,
            chunk,
        });
        if out.len() == leg.k {
            break;
        }
    }
    Ok(out)
}
