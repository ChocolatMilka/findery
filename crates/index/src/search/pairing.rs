//! Relations between the hits of one answer, from their STORED vectors — no
//! embedding: doc ↔ code pairs, and near-duplicates across files.
//!
//! The work is bounded by the hits returned (a few dozen), never by the
//! project: comparing every chunk with every other is what this avoids.

use findery_core::api::{Duplicate, HitRef, PairMatch, Pairing};
use findery_core::ids::ChunkId;

/// Cosine similarity; `0.0` for a zero vector or a width mismatch.
pub(super) fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// How pairs and duplicates are judged — `search.*` in the configuration.
#[derive(Clone, Copy, Debug)]
pub(super) struct Thresholds {
    pub(super) drift_below: f32,
    pub(super) pair_matches: usize,
    pub(super) duplicate_min: f32,
    pub(super) duplicate_max: f32,
}

/// Each hit of leg 0 with its nearest hits of leg 1, best first. A hit
/// whose stored vector is missing is left out; the caller reports it.
pub(super) fn pair(
    left: &[Option<&[f32]>],
    right: &[Option<&[f32]>],
    limits: Thresholds,
) -> Vec<Pairing> {
    let mut out = Vec::with_capacity(left.len());
    for (hit, vector) in left.iter().enumerate() {
        let Some(vector) = vector else {
            continue;
        };
        let matches = nearest(vector, right, limits.pair_matches);
        let best = matches.first().map_or(f32::MIN, |m| m.similarity);
        out.push(Pairing {
            left: HitRef { leg: 0, hit },
            drifted: best < limits.drift_below,
            matches,
        });
    }
    out
}

/// The `n` hits of leg 1 most similar to `vector`, ties broken by position.
fn nearest(vector: &[f32], right: &[Option<&[f32]>], n: usize) -> Vec<PairMatch> {
    let mut scored: Vec<PairMatch> = Vec::with_capacity(right.len());
    for (hit, other) in right.iter().enumerate() {
        let Some(other) = other else {
            continue;
        };
        scored.push(PairMatch {
            right: HitRef { leg: 1, hit },
            similarity: cosine(vector, other),
        });
    }
    scored.sort_by(|a, b| {
        b.similarity
            .total_cmp(&a.similarity)
            .then_with(|| a.right.hit.cmp(&b.right.hit))
    });
    scored.truncate(n);
    scored
}

/// One hit that may be a near-duplicate of another.
pub(super) struct Candidate<'a> {
    pub(super) at: HitRef,
    pub(super) id: &'a ChunkId,
    pub(super) path: &'a str,
    pub(super) vector: &'a [f32],
}

/// Every two candidates of different files whose similarity falls within
/// `[duplicate_min, duplicate_max]`, most similar first. A chunk found by
/// several legs is compared once, under its first appearance.
pub(super) fn duplicates(candidates: &[Candidate<'_>], limits: Thresholds) -> Vec<Duplicate> {
    let unique = first_appearances(candidates);
    let mut out = Vec::new();
    for (i, a) in unique.iter().enumerate() {
        for b in &unique[i + 1..] {
            if a.path == b.path {
                continue;
            }
            let similarity = cosine(a.vector, b.vector);
            if similarity >= limits.duplicate_min && similarity <= limits.duplicate_max {
                out.push(Duplicate {
                    a: a.at,
                    b: b.at,
                    similarity,
                });
            }
        }
    }
    out.sort_by(|x, y| {
        y.similarity.total_cmp(&x.similarity).then_with(|| {
            (x.a.leg, x.a.hit, x.b.leg, x.b.hit).cmp(&(y.a.leg, y.a.hit, y.b.leg, y.b.hit))
        })
    });
    out
}

fn first_appearances<'c, 'a>(candidates: &'c [Candidate<'a>]) -> Vec<&'c Candidate<'a>> {
    let mut seen: Vec<&ChunkId> = Vec::with_capacity(candidates.len());
    let mut out = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if !seen.contains(&candidate.id) {
            seen.push(candidate.id);
            out.push(candidate);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: Thresholds = Thresholds {
        drift_below: 0.5,
        pair_matches: 2,
        duplicate_min: 0.85,
        duplicate_max: 0.98,
    };

    /// A unit vector at `degrees` in the plane: the cosine of two of them is
    /// the cosine of the angle between.
    fn at(degrees: f32) -> Vec<f32> {
        let r = degrees.to_radians();
        vec![r.cos(), r.sin()]
    }

    #[test]
    fn cosine_survives_a_zero_vector_and_a_width_mismatch() {
        assert!((cosine(&at(0.0), &at(60.0)) - 0.5).abs() < 1e-5);
        assert_eq!(cosine(&[0.0, 0.0], &at(0.0)), 0.0);
        assert_eq!(cosine(&[1.0], &at(0.0)), 0.0);
    }

    #[test]
    fn a_doc_takes_its_nearest_code_and_a_weak_best_is_drifted() {
        let (far, close, mid) = (at(80.0), at(10.0), at(40.0));
        let docs = [Some(at(0.0)), Some(at(200.0)), None];
        let docs: Vec<Option<&[f32]>> = docs.iter().map(|v| v.as_deref()).collect();
        let code: Vec<Option<&[f32]>> = vec![Some(&far), Some(&close), Some(&mid)];
        let pairs = pair(&docs, &code, LIMITS);

        assert_eq!(pairs.len(), 2, "a doc with no stored vector is left out");
        let order: Vec<usize> = pairs[0].matches.iter().map(|m| m.right.hit).collect();
        assert_eq!(order, [1, 2], "best first, capped at pair_matches");
        assert!(!pairs[0].drifted);
        assert!(pairs[1].drifted, "its best is 120° away");
        assert_eq!(pairs[1].matches[0].right.hit, 0);
    }

    #[test]
    fn a_doc_with_no_code_at_all_is_drifted() {
        let doc = at(0.0);
        let pairs = pair(&[Some(&doc)], &[], LIMITS);
        assert!(pairs[0].drifted && pairs[0].matches.is_empty());
    }

    fn hit(leg: usize, hit: usize) -> HitRef {
        HitRef { leg, hit }
    }

    #[test]
    fn duplicates_keep_the_band_and_compare_each_chunk_once() {
        let ids: Vec<ChunkId> = ["a", "b", "c", "d"].map(ChunkId::from_hex).into();
        let (base, copy, same, other, near) = (at(0.0), at(20.0), at(1.0), at(60.0), at(21.0));
        let candidates = [
            Candidate {
                at: hit(0, 0),
                id: &ids[0],
                path: "a.rs",
                vector: &base,
            },
            Candidate {
                at: hit(0, 1),
                id: &ids[1],
                path: "b.rs",
                vector: &copy,
            },
            Candidate {
                at: hit(1, 0),
                id: &ids[0],
                path: "a.rs",
                vector: &base,
            },
            Candidate {
                at: hit(1, 1),
                id: &ids[2],
                path: "c.rs",
                vector: &same,
            },
            Candidate {
                at: hit(1, 2),
                id: &ids[3],
                path: "d.rs",
                vector: &other,
            },
            // `d` again: were it compared twice, it would pair with `a`.
            Candidate {
                at: hit(1, 3),
                id: &ids[3],
                path: "d.rs",
                vector: &near,
            },
        ];
        let found = duplicates(&candidates, LIMITS);
        let pairs: Vec<(HitRef, HitRef)> = found.iter().map(|d| (d.a, d.b)).collect();
        // b ↔ c (cos 19°) and a ↔ b (cos 20°); a ↔ c is an identical copy,
        // above the band, and d is unrelated.
        assert_eq!(pairs, [(hit(0, 1), hit(1, 1)), (hit(0, 0), hit(0, 1))]);
    }

    #[test]
    fn two_chunks_of_the_same_file_are_never_duplicates() {
        let ids: Vec<ChunkId> = ["a", "b"].map(ChunkId::from_hex).into();
        let (one, two) = (at(0.0), at(20.0));
        let candidates = [
            Candidate {
                at: hit(0, 0),
                id: &ids[0],
                path: "x.rs",
                vector: &one,
            },
            Candidate {
                at: hit(0, 1),
                id: &ids[1],
                path: "x.rs",
                vector: &two,
            },
        ];
        assert!(duplicates(&candidates, LIMITS).is_empty());
    }
}
