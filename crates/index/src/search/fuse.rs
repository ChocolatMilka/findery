//! Reciprocal-rank fusion of a vector list and a keyword list, and the small
//! helpers that turn a fused id into a hit a human can open.
//!
//! INTENT: RRF fuses on RANK rather than on score, so a cosine in [-1, 1] and
//! an unbounded BM25 need no calibration.

use std::collections::HashMap;

use findery_core::ids::ChunkId;
use findery_core::model::{Chunk, Project, ScoredChunk, SearchMode};
use findery_core::traits::VectorPayload;

/// A fused candidate, carrying both original scores for explainability.
pub(super) struct Fused {
    pub(super) id: ChunkId,
    pub(super) score: f32,
    pub(super) vector_score: Option<f32>,
    pub(super) keyword_score: Option<f32>,
}

/// Fuses the two ranked lists, best first.
///
/// With one list only, its own score is kept: `score: 0.83` for a pure vector
/// search should read as a cosine, not as an RRF artefact.
pub(super) fn fuse(
    vector: &[ScoredChunk],
    keyword: &[ScoredChunk],
    rrf_k: f32,
    mode: SearchMode,
) -> Vec<Fused> {
    let mut acc: HashMap<String, Fused> = HashMap::new();
    for (rank, hit) in vector.iter().enumerate() {
        let entry = entry(&mut acc, hit);
        entry.vector_score = Some(hit.score);
        entry.score += 1.0 / (rrf_k + rank as f32 + 1.0);
    }
    for (rank, hit) in keyword.iter().enumerate() {
        let entry = entry(&mut acc, hit);
        entry.keyword_score = Some(hit.score);
        entry.score += 1.0 / (rrf_k + rank as f32 + 1.0);
    }

    let mut out: Vec<Fused> = acc.into_values().collect();
    for fused in &mut out {
        match mode {
            SearchMode::Vector => fused.score = fused.vector_score.unwrap_or(0.0),
            SearchMode::Keyword => fused.score = fused.keyword_score.unwrap_or(0.0),
            SearchMode::Hybrid => {}
        }
    }
    // Ties are broken by id, not by hash-map order: two identical queries must
    // return identical lists, or every cache above this is a lie.
    out.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });
    out
}

fn entry<'a>(acc: &'a mut HashMap<String, Fused>, hit: &ScoredChunk) -> &'a mut Fused {
    acc.entry(hit.id.to_string()).or_insert_with(|| Fused {
        id: hit.id.clone(),
        score: 0.0,
        vector_score: None,
        keyword_score: None,
    })
}

/// The payload a filter is evaluated against, rebuilt from a stored chunk.
pub(super) fn payload_of(chunk: &Chunk) -> VectorPayload {
    VectorPayload {
        path: chunk.path.clone(),
        lang: chunk.lang,
        kind: chunk.kind,
        symbol: chunk.breadcrumb.clone().or_else(|| chunk.symbol.clone()),
    }
}

/// Absolute path on the host, so a hit is something a human can open.
pub(crate) fn host_path(project: &Project, rel: &str) -> String {
    let root = project.host_root.trim_end_matches(['/', '\\']);
    if root.is_empty() {
        rel.to_string()
    } else {
        format!("{root}/{rel}")
    }
}

/// Trims a snippet on a line boundary and says so, instead of cutting a token
/// in half and letting the reader believe the file ends there.
pub(super) fn truncate_text(text: &mut String, budget: usize) {
    if text.len() <= budget {
        return;
    }
    let mut end = budget;
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
    use findery_core::ids::ProjectId;

    use super::*;

    fn scored(id: &str, score: f32) -> ScoredChunk {
        ScoredChunk {
            id: ChunkId::from_hex(id),
            score,
        }
    }

    #[test]
    fn fusion_rewards_agreement_between_the_two_lists() {
        let vector = vec![scored("a", 0.9), scored("b", 0.8), scored("c", 0.7)];
        let keyword = vec![scored("c", 12.0), scored("a", 3.0)];
        let out = fuse(&vector, &keyword, 60.0, SearchMode::Hybrid);
        // `a` is ranked by both lists; `b` by one only, despite a better cosine.
        assert_eq!(out[0].id.as_str(), "a");
        let c = out.iter().position(|f| f.id.as_str() == "c");
        assert!(c.expect("c present") < 2);
        assert_eq!(out[0].vector_score, Some(0.9));
        assert_eq!(out[0].keyword_score, Some(3.0));
    }

    #[test]
    fn a_single_list_keeps_its_own_score_scale() {
        let vector = vec![scored("a", 0.91), scored("b", 0.42)];
        let out = fuse(&vector, &[], 60.0, SearchMode::Vector);
        assert!((out[0].score - 0.91).abs() < 1e-6);
        assert!((out[1].score - 0.42).abs() < 1e-6);

        let out = fuse(&[], &[scored("x", 9.5)], 60.0, SearchMode::Keyword);
        assert!((out[0].score - 9.5).abs() < 1e-6);
    }

    #[test]
    fn fusion_is_deterministic_and_descending() {
        let vector = vec![scored("a", 0.9), scored("b", 0.8)];
        let keyword = vec![scored("b", 2.0), scored("a", 1.0)];
        let ids = |list: Vec<Fused>| list.into_iter().map(|f| f.id).collect::<Vec<_>>();
        let first = fuse(&vector, &keyword, 60.0, SearchMode::Hybrid);
        assert!(first.windows(2).all(|w| w[0].score >= w[1].score));
        let second = fuse(&vector, &keyword, 60.0, SearchMode::Hybrid);
        assert_eq!(ids(first), ids(second));
    }

    #[test]
    fn snippets_are_cut_on_a_line_and_marked() {
        let mut text = "line one\nline two\nline three\n".to_string();
        truncate_text(&mut text, 12);
        assert!(text.starts_with("line one"));
        assert!(text.ends_with("[truncated]"));
        assert!(!text.contains("line three"));

        let mut short = "short".to_string();
        truncate_text(&mut short, 100);
        assert_eq!(short, "short");
    }

    #[test]
    fn host_paths_are_absolute_and_separator_clean() {
        let project = Project {
            id: ProjectId::new("demo").expect("id"),
            name: "demo".into(),
            bucket: findery_core::model::DEFAULT_BUCKET.into(),
            model: None,
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
