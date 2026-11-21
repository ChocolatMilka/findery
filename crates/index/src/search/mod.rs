//! Retrieval: a request resolved into legs, each a vector list and a keyword
//! list fused by rank, run together and answered grouped.
//!
//! INTENT: on code, neither list is sufficient alone. Embeddings find "where is
//! retrying handled" in a file that never says "retry"; BM25 finds `E0432`
//! verbatim, which an embedding blurs into its neighbours. A workflow intent
//! is the handful of such searches one task needs, run at once.

mod fuse;
mod gather;
pub(crate) mod intents;
mod leg;
mod pairing;
pub(crate) mod read;
mod vectors;

pub(crate) use fuse::host_path;

use std::sync::Arc;

use findery_core::Result;
use findery_core::api::{Duplicate, LegHits, Pairing, ResolvedEcho, SearchResponse};
use findery_core::ids::ContentHash;
use findery_core::intent::{PlanContext, ResolvedPlan};
use findery_core::model::filter::CompiledFilter;
use findery_core::model::{Lang, SearchFilter, SearchHit, SearchMode, SearchRequest};

use crate::Indexer;
use crate::gates::{Access, Lease};
use crate::storage::families::BucketHandle;
use gather::Gathered;
use leg::{LegRun, Shared};
use pairing::{Candidate, Thresholds};

/// Stands in for "this request named no scope": an empty filter rather than an
/// `Option`, so [`SearchFilter::layer`] has one shape to merge.
static NO_SCOPE: SearchFilter = SearchFilter {
    paths: Vec::new(),
    langs: Vec::new(),
    kinds: Vec::new(),
    symbol_contains: None,
};

/// Runs a search: every leg of the resolved plan, concurrently.
///
/// In hybrid mode, a vector list that cannot be had — no vectors under the
/// project's model yet, its runtime down — becomes a warning and the keyword
/// list answers alone, from the freshest member that has chunks. In vector
/// mode, and for a `like` leg, the same failure is the answer.
pub async fn search(idx: &Indexer, req: &SearchRequest) -> Result<SearchResponse> {
    let lease = idx.lease(&req.project, Access::Read).await?;
    let mut plan = resolve(idx, &lease, req).await?;
    let mut warnings = std::mem::take(&mut plan.warnings);
    let modes = leg_modes(idx, &plan);

    let mut gathered = gather::gather(&lease, req, &plan, &modes, &mut warnings).await?;
    let source = match gathered.member.take() {
        Some(member) => Some(member),
        None => keyword_source(&lease).await?,
    };
    let Some(source) = source else {
        let empty = vec![Vec::new(); plan.legs.len()];
        let response = assemble(&plan, empty, Relations::default(), warnings);
        return Ok(stamped(response, &lease));
    };

    let runs = prepare(idx, &plan, &modes, &gathered)?;
    let keyword = keyword_list(idx, &lease, &source, req, &runs).await?;
    let shared = Shared {
        project: &lease.project,
        source: &source,
        keyword: &keyword,
        rrf_k: idx.cfg.search.rrf_k,
        budget: req
            .max_snippet_chars
            .unwrap_or(idx.cfg.search.max_snippet_chars),
    };
    let lists = futures::future::join_all(runs.iter().map(|run| leg::run(&shared, run))).await;
    let hits = lists.into_iter().collect::<Result<Vec<_>>>()?;

    let relations = relate(idx, &lease, &source, &plan, &hits, &mut warnings).await?;
    Ok(stamped(assemble(&plan, hits, relations, warnings), &lease))
}

/// The answer with the model it ran under, so a client can say which.
fn stamped(mut response: SearchResponse, lease: &Lease) -> SearchResponse {
    if let Some(echo) = response.resolved.as_mut() {
        echo.model = Some(lease.alias.to_string());
    }
    response
}

/// The plan, against the project's scope, its intent library and its
/// language mix. Precedence lives in L0, so no transport invents its own.
async fn resolve(idx: &Indexer, lease: &Lease, req: &SearchRequest) -> Result<ResolvedPlan> {
    let project = &lease.project;
    // A scope is resolved here because the daemon is the one component every
    // transport reaches, and the only one holding the project's definition.
    let scope = match &req.scope {
        Some(name) => project.spec.scope(name)?,
        None => &NO_SCOPE,
    };
    let library = idx.intent_cache.for_project(project)?;
    let langs = project_langs(lease).await?;
    let ctx = PlanContext {
        library: &library,
        scope,
        project_instruction: project.spec.instruction.as_deref(),
        project_intent: project.spec.task.map(|task| task.as_str()),
        langs: &langs,
        max_instruction_chars: idx.cfg.search.max_instruction_chars,
    };
    req.resolve_plan(&ctx)
}

/// The project's languages, most chunks first, from the member of its model.
async fn project_langs(lease: &Lease) -> Result<Vec<Lang>> {
    let Some(member) = &lease.member else {
        return Ok(Vec::new());
    };
    let mix = member.bucket.lang_mix(&lease.project.id).await?;
    Ok(mix.into_iter().map(|(lang, _)| lang).collect())
}

/// Each leg's recall strategy: its own, else the daemon's; a `like` leg
/// searches by vector only — its query IS a vector.
fn leg_modes(idx: &Indexer, plan: &ResolvedPlan) -> Vec<SearchMode> {
    let default = idx.cfg.search.default_mode;
    let mut modes = Vec::with_capacity(plan.legs.len());
    for leg in &plan.legs {
        if leg.by_example {
            modes.push(SearchMode::Vector);
        } else {
            modes.push(leg.mode.unwrap_or(default));
        }
    }
    modes
}

/// Every leg with its k, its candidate count, its compiled filter and its
/// query vector.
fn prepare<'g>(
    idx: &Indexer,
    plan: &ResolvedPlan,
    modes: &[SearchMode],
    gathered: &'g Gathered,
) -> Result<Vec<LegRun<'g>>> {
    let cfg = &idx.cfg.search;
    let mut runs = Vec::with_capacity(plan.legs.len());
    for (at, (leg, mode)) in plan.legs.iter().zip(modes).enumerate() {
        let k = leg.k.unwrap_or(cfg.default_k).clamp(1, cfg.max_k);
        // A `like` leg must not answer with its own example.
        let exclude = match (&gathered.example, leg.by_example) {
            (Some(example), true) => Some(example.chunk.id.clone()),
            _ => None,
        };
        runs.push(LegRun {
            k,
            candidates: (k * cfg.candidate_multiplier.max(1))
                .min(cfg.max_k * cfg.candidate_ceiling_multiplier.max(1)),
            mode: *mode,
            compiled: CompiledFilter::new(&leg.filter)?,
            filter: leg.filter.clone(),
            vector: gathered.vector_of(at, leg.by_example),
            exclude,
        });
    }
    Ok(runs)
}

/// The query's BM25 list, fetched once for every leg that fuses one. A leg
/// whose filter is not a pass-through over-fetches, since the list carries no
/// payload and is narrowed once the chunks are in hand.
async fn keyword_list(
    idx: &Indexer,
    lease: &Lease,
    source: &BucketHandle,
    req: &SearchRequest,
    runs: &[LegRun<'_>],
) -> Result<Vec<findery_core::model::ScoredChunk>> {
    if req.query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let overfetch = idx.cfg.search.keyword_overfetch.max(1);
    let mut limit = 0;
    for run in runs.iter().filter(|run| run.mode != SearchMode::Vector) {
        let factor = if run.compiled.is_pass_through() {
            1
        } else {
            overfetch
        };
        limit = limit.max(run.candidates * factor);
    }
    if limit == 0 {
        return Ok(Vec::new());
    }
    let id = &lease.project.id;
    source.bucket.keyword_search(id, &req.query, limit).await
}

/// Where the keyword list reads when no vector member answered: the lease's
/// member when it holds chunks of the project, else the sibling member with
/// the freshest row that does. BM25 needs no vector, so any model's chunks
/// answer it — which keeps a search useful right after a switch.
pub(crate) async fn keyword_source(lease: &Lease) -> Result<Option<Arc<BucketHandle>>> {
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

#[derive(Default)]
struct Relations {
    pairs: Vec<Pairing>,
    duplicates: Vec<Duplicate>,
}

/// Pairs and near-duplicates among the hits, from their stored vectors.
async fn relate(
    idx: &Indexer,
    lease: &Lease,
    source: &BucketHandle,
    plan: &ResolvedPlan,
    hits: &[Vec<SearchHit>],
    warnings: &mut Vec<String>,
) -> Result<Relations> {
    if !plan.pairing && !plan.duplicates {
        return Ok(Relations::default());
    }
    let mut hashes: Vec<ContentHash> = Vec::new();
    for hit in hits.iter().flatten() {
        if !hashes.contains(&hit.chunk.content_hash) {
            hashes.push(hit.chunk.content_hash.clone());
        }
    }
    let stored = vectors::stored(source, lease, &hashes).await?;
    let vector_of = |hit: &SearchHit| {
        stored
            .get(hit.chunk.content_hash.as_str())
            .map(Vec::as_slice)
    };
    let missing = hashes
        .iter()
        .filter(|h| !stored.contains_key(h.as_str()))
        .count();
    if missing > 0 {
        warnings.push(format!(
            "{missing} hit(s) have no stored vector under model `{}` and are left out of the \
             pairing; run `findery sync -p {}`",
            lease.alias, lease.project.id
        ));
    }

    let cfg = &idx.cfg.search;
    let limits = Thresholds {
        drift_below: cfg.drift_below,
        pair_matches: cfg.pair_matches,
        duplicate_min: cfg.duplicate_min,
        duplicate_max: cfg.duplicate_max,
    };
    let mut relations = Relations::default();
    if plan.pairing && hits.len() >= 2 {
        let left: Vec<_> = hits[0].iter().map(vector_of).collect();
        let right: Vec<_> = hits[1].iter().map(vector_of).collect();
        relations.pairs = pairing::pair(&left, &right, limits);
    }
    if plan.duplicates {
        let candidates = code_candidates(hits, &vector_of);
        relations.duplicates = pairing::duplicates(&candidates, limits);
    }
    Ok(relations)
}

/// The hits that are code — prose and data files are not "copies" — with
/// their stored vectors.
fn code_candidates<'h>(
    hits: &'h [Vec<SearchHit>],
    vector_of: &impl Fn(&SearchHit) -> Option<&'h [f32]>,
) -> Vec<Candidate<'h>> {
    let mut out = Vec::new();
    for (leg, list) in hits.iter().enumerate() {
        for (hit, found) in list.iter().enumerate() {
            let is_code = !matches!(
                found.chunk.lang,
                Lang::Markdown | Lang::Toml | Lang::Json | Lang::Other
            );
            let Some(vector) = vector_of(found).filter(|_| is_code) else {
                continue;
            };
            out.push(Candidate {
                at: findery_core::api::HitRef { leg, hit },
                id: &found.chunk.id,
                path: &found.chunk.path,
                vector,
            });
        }
    }
    out
}

/// The response: one list for a single search, one per leg for a workflow.
fn assemble(
    plan: &ResolvedPlan,
    mut hits: Vec<Vec<SearchHit>>,
    relations: Relations,
    warnings: Vec<String>,
) -> SearchResponse {
    let resolved = Some(ResolvedEcho::from(plan));
    if !plan.workflow {
        return SearchResponse {
            hits: hits.pop().unwrap_or_default(),
            resolved,
            warnings,
            ..SearchResponse::default()
        };
    }
    let legs: Vec<LegHits> = plan
        .legs
        .iter()
        .zip(hits)
        .map(|(leg, hits)| LegHits {
            label: leg.label.clone(),
            instruction: leg.instruction.clone(),
            hits,
        })
        .collect();
    // INTENT: repeated for a client that predates `legs` — the first leg is the
    // plan's main question, so an older CLI still shows a sensible list.
    let first = legs.first().map(|leg| leg.hits.clone()).unwrap_or_default();
    SearchResponse {
        hits: first,
        legs,
        pairs: relations.pairs,
        duplicates: relations.duplicates,
        resolved,
        warnings,
    }
}
