//! Retrieval: a vector leg, a keyword leg, and reciprocal-rank fusion.
//!
//! INTENT: on code, neither leg is sufficient alone. Embeddings find "where is
//! retrying handled" across a file that never says "retry"; BM25 finds
//! `E0432` or `try_from_utf8_lossy` verbatim, which an embedding blurs into its
//! neighbours. RRF fuses them on RANK rather than on score, so two
//! incomparable scales (cosine in [-1,1], BM25 unbounded) need no calibration.

use crate::Indexer;
use findery_core::Result;
use findery_core::filter::CompiledFilter;
use findery_core::ids::ChunkId;
use findery_core::model::{Chunk, Project, ScoredChunk, SearchHit, SearchMode, SearchRequest};
use findery_core::traits::VectorPayload;
use std::collections::HashMap;

/// Runs a search and returns hits sorted by descending score.
pub async fn search(idx: &Indexer, req: &SearchRequest) -> Result<Vec<SearchHit>> {
    let cat = idx.catalog_for(&req.project)?;
    let project = cat.catalog.require_project(&req.project).await?;
    let cfg = &idx.cfg.search;

    let k = req.k.unwrap_or(cfg.default_k).clamp(1, cfg.max_k);
    let mode = req.mode.unwrap_or(cfg.default_mode);
    let candidates = (k * cfg.candidate_multiplier.max(1)).min(cfg.max_k * 8);
    let compiled = CompiledFilter::new(&req.filter)?;

    let vector_hits = if mode == SearchMode::Keyword {
        Vec::new()
    } else {
        // A catalog built with another recipe can still be keyword-searched:
        // only the leg that would compare vectors from two embedding spaces is
        // refused.
        cat.ensure_vectors_usable()?;
        // Precedence (free text > preset > daemon default) is resolved in L0,
        // so no transport can invent its own ordering.
        let query = idx
            .embedder
            .embed_query(&req.query, req.resolved_instruction())
            .await?;
        cat.store
            .search(&req.project, &query, candidates, &req.filter)
            .await?
    };

    let keyword_hits = if mode == SearchMode::Vector {
        Vec::new()
    } else {
        // The keyword leg has no payload to filter on, so it over-fetches and
        // is narrowed below, once the chunks are in hand.
        let extra = if compiled.is_pass_through() {
            candidates
        } else {
            candidates * 2
        };
        cat.catalog
            .keyword_search(&req.project, &req.query, extra)
            .await?
    };

    let fused = fuse(&vector_hits, &keyword_hits, cfg.rrf_k, mode);
    if fused.is_empty() {
        return Ok(Vec::new());
    }

    let ids: Vec<ChunkId> = fused.iter().map(|f| f.id.clone()).collect();
    let chunks = cat.catalog.get_chunks(&req.project, &ids).await?;
    let mut by_id: HashMap<String, Chunk> =
        chunks.into_iter().map(|c| (c.id.to_string(), c)).collect();

    let budget = req.max_snippet_chars.unwrap_or(cfg.max_snippet_chars);
    let mut out = Vec::with_capacity(k);
    for scored in fused {
        let Some(mut chunk) = by_id.remove(scored.id.as_str()) else {
            continue;
        };
        if !compiled.is_pass_through() && !compiled.matches(&payload_of(&chunk)) {
            continue;
        }
        if budget > 0 {
            truncate_text(&mut chunk.text, budget);
        }
        out.push(SearchHit {
            host_path: host_path(&project, &chunk.path),
            score: scored.score,
            vector_score: scored.vector_score,
            keyword_score: scored.keyword_score,
            chunk,
        });
        if out.len() == k {
            break;
        }
    }
    Ok(out)
}

/// A fused candidate, carrying both original scores for explainability.
struct Fused {
    id: ChunkId,
    score: f32,
    vector_score: Option<f32>,
    keyword_score: Option<f32>,
}

/// Reciprocal-rank fusion of the two ranked lists.
///
/// With a single leg active, the leg's own score is kept: an operator reading
/// `score: 0.83` for a pure vector search should see a cosine, not an RRF
/// artefact that means nothing on its own.
fn fuse(
    vector: &[ScoredChunk],
    keyword: &[ScoredChunk],
    rrf_k: f32,
    mode: SearchMode,
) -> Vec<Fused> {
    let mut acc: HashMap<String, Fused> = HashMap::new();

    for (rank, hit) in vector.iter().enumerate() {
        let entry = acc.entry(hit.id.to_string()).or_insert_with(|| Fused {
            id: hit.id.clone(),
            score: 0.0,
            vector_score: None,
            keyword_score: None,
        });
        entry.vector_score = Some(hit.score);
        entry.score += 1.0 / (rrf_k + rank as f32 + 1.0);
    }
    for (rank, hit) in keyword.iter().enumerate() {
        let entry = acc.entry(hit.id.to_string()).or_insert_with(|| Fused {
            id: hit.id.clone(),
            score: 0.0,
            vector_score: None,
            keyword_score: None,
        });
        entry.keyword_score = Some(hit.score);
        entry.score += 1.0 / (rrf_k + rank as f32 + 1.0);
    }

    let mut out: Vec<Fused> = acc.into_values().collect();
    match mode {
        SearchMode::Vector => {
            for f in &mut out {
                f.score = f.vector_score.unwrap_or(0.0);
            }
        }
        SearchMode::Keyword => {
            for f in &mut out {
                f.score = f.keyword_score.unwrap_or(0.0);
            }
        }
        SearchMode::Hybrid => {}
    }
    // Ties are broken by id, not by hash-map iteration order: two identical
    // queries must return identical lists, or every cache above this is a lie.
    out.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });
    out
}

/// Rebuilds the payload a filter is evaluated against from a stored chunk.
fn payload_of(chunk: &Chunk) -> VectorPayload {
    VectorPayload {
        path: chunk.path.clone(),
        lang: chunk.lang,
        kind: chunk.kind,
        symbol: chunk.breadcrumb.clone().or_else(|| chunk.symbol.clone()),
    }
}

/// Absolute path on the host, so a hit is something a human can open.
fn host_path(project: &Project, rel: &str) -> String {
    let root = project.host_root.trim_end_matches(['/', '\\']);
    if root.is_empty() {
        rel.to_string()
    } else {
        format!("{root}/{rel}")
    }
}

/// Trims a snippet on a line boundary and says so, instead of cutting a token
/// in half and letting the reader believe the file ends there.
fn truncate_text(text: &mut String, budget: usize) {
    if text.len() <= budget {
        return;
    }
    let mut end = budget.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    if let Some(nl) = text[..end].rfind('\n') {
        end = nl;
    }
    text.truncate(end);
    text.push_str("\n… [truncated]");
}

#[cfg(test)]
mod tests {
    use super::*;
    use findery_core::ids::ProjectId;

    fn scored(id: &str, score: f32) -> ScoredChunk {
        ScoredChunk {
            id: ChunkId::from_hex(id),
            score,
        }
    }

    #[test]
    fn fusion_rewards_agreement_between_the_two_legs() {
        let vector = vec![scored("a", 0.9), scored("b", 0.8), scored("c", 0.7)];
        let keyword = vec![scored("c", 12.0), scored("a", 3.0)];
        let out = fuse(&vector, &keyword, 60.0, SearchMode::Hybrid);

        // `a` is ranked by both legs; `b` only by one, despite a better cosine.
        assert_eq!(out[0].id.as_str(), "a");
        assert!(
            out.iter()
                .position(|f| f.id.as_str() == "c")
                .expect("c present")
                < 2
        );
        assert_eq!(out[0].vector_score, Some(0.9));
        assert_eq!(out[0].keyword_score, Some(3.0));
    }

    #[test]
    fn a_single_leg_keeps_its_own_score_scale() {
        let vector = vec![scored("a", 0.91), scored("b", 0.42)];
        let out = fuse(&vector, &[], 60.0, SearchMode::Vector);
        assert!((out[0].score - 0.91).abs() < 1e-6);
        assert!((out[1].score - 0.42).abs() < 1e-6);

        let keyword = vec![scored("x", 9.5)];
        let out = fuse(&[], &keyword, 60.0, SearchMode::Keyword);
        assert!((out[0].score - 9.5).abs() < 1e-6);
    }

    #[test]
    fn fusion_is_deterministic_and_descending() {
        let vector = vec![scored("a", 0.9), scored("b", 0.8)];
        let keyword = vec![scored("b", 2.0), scored("a", 1.0)];
        let first = fuse(&vector, &keyword, 60.0, SearchMode::Hybrid);
        let second = fuse(&vector, &keyword, 60.0, SearchMode::Hybrid);
        assert_eq!(
            first.iter().map(|f| f.id.to_string()).collect::<Vec<_>>(),
            second.iter().map(|f| f.id.to_string()).collect::<Vec<_>>()
        );
        assert!(first.windows(2).all(|w| w[0].score >= w[1].score));
    }

    #[test]
    fn snippets_are_cut_on_a_line_and_marked() {
        let mut text = "line one\nline two\nline three\n".to_string();
        truncate_text(&mut text, 12);
        assert!(text.starts_with("line one"));
        assert!(text.ends_with("[truncated]"));
        assert!(!text.contains("line three"));
    }

    #[test]
    fn a_short_snippet_is_left_alone() {
        let mut text = "short".to_string();
        truncate_text(&mut text, 100);
        assert_eq!(text, "short");
    }

    #[test]
    fn host_paths_are_absolute_and_separator_clean() {
        let project = Project {
            id: ProjectId::new("demo").expect("id"),
            name: "demo".into(),
            catalog: findery_core::model::DEFAULT_CATALOG.into(),
            spec: Default::default(),
            host_root: "C:/Users/me/demo/".into(),
            created_at: 0,
            updated_at: 0,
        };
        assert_eq!(
            host_path(&project, "src/lib.rs"),
            "C:/Users/me/demo/src/lib.rs"
        );
    }
}
