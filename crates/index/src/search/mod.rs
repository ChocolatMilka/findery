//! Retrieval: a vector list and a keyword list, fused by rank, then hydrated
//! into hits a human can open.
//!
//! INTENT: on code, neither list is sufficient alone. Embeddings find "where is
//! retrying handled" in a file that never says "retry"; BM25 finds `E0432`
//! verbatim, which an embedding blurs into its neighbours.

mod fuse;
mod gather;
mod vectors;

use std::collections::HashMap;
use std::sync::Arc;

use findery_core::Result;
use findery_core::api::SearchResponse;
use findery_core::ids::ChunkId;
use findery_core::model::filter::CompiledFilter;
use findery_core::model::{Chunk, ScoredChunk, SearchFilter, SearchHit, SearchMode, SearchRequest};

use crate::Indexer;
use crate::gates::{Access, Lease};
use crate::storage::families::BucketHandle;
use fuse::{Fused, fuse, host_path, payload_of, truncate_text};

/// Stands in for "this request named no scope": an empty filter rather than an
/// `Option`, so [`SearchFilter::layer`] has one shape to merge.
static NO_SCOPE: SearchFilter = SearchFilter {
    paths: Vec::new(),
    langs: Vec::new(),
    kinds: Vec::new(),
    symbol_contains: None,
};

/// Runs a search and returns hits sorted by descending score.
///
/// In hybrid mode, a vector list that cannot be had — no vectors under the
/// project's model yet, its runtime down — becomes a warning and the keyword
/// list answers alone, from the freshest member that has chunks. In vector
/// mode the same failure is the answer.
pub async fn search(idx: &Indexer, req: &SearchRequest) -> Result<SearchResponse> {
    let lease = idx.lease(&req.project, Access::Read).await?;
    let project = &lease.project;
    let cfg = &idx.cfg.search;

    // A scope is a name the project declared, resolved here because the daemon
    // is the one component every transport reaches. An unknown name is refused
    // with the list of the ones that exist, rather than silently searching
    // everything.
    let scope = match &req.scope {
        Some(name) => project.spec.scope(name)?,
        None => &NO_SCOPE,
    };
    // Precedence (request > project > daemon default) is resolved in L0, so no
    // transport can invent its own ordering.
    let resolved = req.resolve(scope, project.spec.resolved_instruction());
    let compiled = CompiledFilter::new(&resolved.filter)?;
    let mode = req.mode.unwrap_or(cfg.default_mode);

    let mut warnings = Vec::new();
    let query = &req.query;
    let instruction = resolved.instruction.as_deref();
    let gathered = gather::gather(&lease, query, instruction, mode, &mut warnings).await?;
    let source = match gathered.member {
        Some(member) => Some(member),
        None => keyword_source(&lease).await?,
    };
    let Some(source) = source else {
        return Ok(SearchResponse {
            hits: Vec::new(),
            warnings,
        });
    };

    let k = req.k.unwrap_or(cfg.default_k).clamp(1, cfg.max_k);
    let run = Run {
        k,
        candidates: (k * cfg.candidate_multiplier.max(1))
            .min(cfg.max_k * cfg.candidate_ceiling_multiplier.max(1)),
        mode,
        filter: resolved.filter,
        compiled,
        vector: gathered.vector,
        budget: req.max_snippet_chars.unwrap_or(cfg.max_snippet_chars),
    };
    let keyword = keyword_list(idx, &lease, &source, req, &run).await?;
    let hits = ranked(&lease, &source, &run, &keyword, cfg.rrf_k).await?;
    Ok(SearchResponse { hits, warnings })
}

/// One search, ready to run.
struct Run {
    k: usize,
    /// Fetched from the vector store before filtering and fusion.
    candidates: usize,
    mode: SearchMode,
    filter: SearchFilter,
    compiled: CompiledFilter,
    /// The query vector; `None` when there is no vector list (keyword mode, or
    /// a hybrid search whose vectors could not be had).
    vector: Option<Vec<f32>>,
    /// `max_snippet_chars`; `0` is unlimited.
    budget: usize,
}

/// The query's BM25 list. A filter that is not a pass-through over-fetches,
/// since the list carries no payload and is narrowed once the chunks are in
/// hand.
async fn keyword_list(
    idx: &Indexer,
    lease: &Lease,
    source: &BucketHandle,
    req: &SearchRequest,
    run: &Run,
) -> Result<Vec<ScoredChunk>> {
    if run.mode == SearchMode::Vector || req.query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let factor = if run.compiled.is_pass_through() {
        1
    } else {
        idx.cfg.search.keyword_overfetch.max(1)
    };
    let id = &lease.project.id;
    source
        .bucket
        .keyword_search(id, &req.query, run.candidates * factor)
        .await
}

/// The vector list and the keyword list fused, best first, at most `k` hits.
async fn ranked(
    lease: &Lease,
    source: &BucketHandle,
    run: &Run,
    keyword: &[ScoredChunk],
    rrf_k: f32,
) -> Result<Vec<SearchHit>> {
    let project = &lease.project;
    let vector_hits = match &run.vector {
        Some(query) => {
            source
                .store
                .search(&project.id, query, run.candidates, &run.filter)
                .await?
        }
        None => Vec::new(),
    };
    let fused = fuse(&vector_hits, keyword, rrf_k, run.mode);
    hydrate(lease, source, run, fused).await
}

/// Loads the fused chunks, keeps the ones the filter accepts, and trims them.
async fn hydrate(
    lease: &Lease,
    source: &BucketHandle,
    run: &Run,
    fused: Vec<Fused>,
) -> Result<Vec<SearchHit>> {
    let project = &lease.project;
    let ids: Vec<ChunkId> = fused.iter().map(|f| f.id.clone()).collect();
    let chunks = source.bucket.get_chunks(&project.id, &ids).await?;
    let mut by_id: HashMap<String, Chunk> =
        chunks.into_iter().map(|c| (c.id.to_string(), c)).collect();

    let mut out = Vec::with_capacity(run.k);
    for scored in fused {
        let Some(mut chunk) = by_id.remove(scored.id.as_str()) else {
            continue;
        };
        // The keyword list carries no payload, so its filter is applied here.
        if !run.compiled.is_pass_through() && !run.compiled.matches(&payload_of(&chunk)) {
            continue;
        }
        if run.budget > 0 {
            truncate_text(&mut chunk.text, run.budget);
        }
        out.push(SearchHit {
            host_path: host_path(project, &chunk.path),
            score: scored.score,
            vector_score: scored.vector_score,
            keyword_score: scored.keyword_score,
            chunk,
        });
        if out.len() == run.k {
            break;
        }
    }
    Ok(out)
}

/// Where the keyword list reads when no vector member answered: the lease's
/// member when it holds chunks of the project, else the sibling member with
/// the freshest row that does. BM25 needs no vector, so any model's chunks
/// answer it — which keeps a search useful right after a switch.
async fn keyword_source(lease: &Lease) -> Result<Option<Arc<BucketHandle>>> {
    let id = &lease.project.id;
    let mut siblings = Vec::new();
    for member in lease.family.members() {
        let Some(row) = member.bucket.get_project(id).await? else {
            continue;
        };
        let active = lease
            .member
            .as_ref()
            .is_some_and(|m| Arc::ptr_eq(m, &member));
        siblings.push((active, row.updated_at, member));
    }
    // The active member first, then the freshest; the tag breaks a tie so two
    // identical searches read the same file.
    siblings.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.2.tag.cmp(&b.2.tag))
    });
    for (_, _, member) in siblings {
        if member.bucket.counts(id).await?.1 > 0 {
            return Ok(Some(member));
        }
    }
    Ok(None)
}
