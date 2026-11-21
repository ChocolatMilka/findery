//! What a search answer reads like, pinned: warnings first, one header, the
//! root once, citable locators, no score, legs as sections, pairs and
//! near-duplicates as locators, and a default answer inside its token budget.

use findery_core::api::{HitRef, LegHits, PairMatch};
use findery_core::ids::{ChunkId, ContentHash};
use findery_core::model::{ChunkKind, Lang};

use super::*;

fn hit(path: &str, lines: (u32, u32), symbol: &str, text: &str) -> SearchHit {
    let project = ProjectId::new("axle").expect("id");
    SearchHit {
        chunk: Chunk {
            id: ChunkId::derive(&project, path, lines.0),
            project,
            path: path.into(),
            lang: Lang::from_path(path),
            ordinal: 0,
            kind: ChunkKind::Method,
            symbol: None,
            breadcrumb: Some(symbol.into()),
            start_line: lines.0,
            end_line: lines.1,
            content_hash: ContentHash::of(text.as_bytes()),
            text: text.into(),
        },
        score: 0.0317,
        vector_score: Some(0.842),
        keyword_score: None,
        host_path: format!("C:/work/axle/{path}"),
    }
}

fn project() -> ProjectId {
    ProjectId::new("axle").expect("id")
}

fn echo(intent: &str, legs: &[&str]) -> ResolvedEcho {
    ResolvedEcho {
        intent: Some(intent.into()),
        legs: legs
            .iter()
            .map(|label| EchoLeg {
                label: (*label).into(),
                instruction: Some("Given an API of a Rust codebase, retrieve its callers.".into()),
                ..EchoLeg::default()
            })
            .collect(),
        model: Some("harrier".into()),
    }
}

#[test]
fn a_single_search_reads_header_root_then_citable_fenced_hits_without_a_score() {
    let response = SearchResponse {
        hits: vec![hit(
            "src/net.rs",
            (12, 48),
            "Client > retry",
            "pub fn retry() {}",
        )],
        resolved: Some(echo("usage", &["usage"])),
        ..SearchResponse::default()
    };
    let text = render(&project(), "retry", &response, Detail::Compact);
    let mut lines = text.lines();
    assert_eq!(
        lines.next(),
        Some(
            "1 hit · `axle` · intent usage · \"Given an API of a Rust codebase, retrieve its \
             callers.\" · model harrier"
        )
    );
    assert_eq!(lines.next(), Some("root C:/work/axle"));
    assert!(
        text.contains("[1] src/net.rs:L12-L48 · method Client > retry\n```rust\n"),
        "{text}"
    );
    assert!(
        !text.contains("0.03") && !text.contains("0.84"),
        "no raw score: {text}"
    );
    assert_eq!(
        text.matches("src/net.rs").count(),
        1,
        "one path per hit: {text}"
    );
}

#[test]
fn warnings_come_first_as_notes() {
    let response = SearchResponse {
        hits: vec![hit("src/a.rs", (1, 2), "a", "fn a() {}")],
        warnings: vec!["keyword results only: run `findery sync -p axle`".into()],
        ..SearchResponse::default()
    };
    let text = render(&project(), "a", &response, Detail::Compact);
    assert!(text.starts_with("note: keyword results only"), "{text}");
}

#[test]
fn a_hit_outside_the_shared_root_names_its_own_host_path() {
    let mut away = hit("src/b.rs", (1, 2), "b", "fn b() {}");
    away.host_path = "D:/mirror/b.rs".into();
    let response = SearchResponse {
        hits: vec![hit("src/a.rs", (1, 2), "a", "fn a() {}"), away],
        ..SearchResponse::default()
    };
    let text = render(&project(), "x", &response, Detail::Compact);
    assert!(!text.contains("root "), "no shared root: {text}");
    assert!(
        text.contains("src/b.rs:L1-L2 · method b · D:/mirror/b.rs"),
        "{text}"
    );
}

#[test]
fn an_empty_answer_says_what_to_try_next() {
    let text = render(
        &project(),
        "nothing",
        &SearchResponse::default(),
        Detail::Compact,
    );
    assert!(text.contains("No match in `axle`"), "{text}");
    assert!(
        text.contains("mode: \"keyword\"") && text.contains("`sync`"),
        "{text}"
    );
}

fn workflow() -> SearchResponse {
    let at = |leg, hit| HitRef { leg, hit };
    let docs = vec![
        hit("README.md", (3, 9), "# Retry", "Retries back off."),
        hit("docs/net.md", (1, 4), "# Pool", "Pools are bounded."),
    ];
    let code: Vec<SearchHit> = (0..5)
        .map(|i| {
            hit(
                &format!("src/m{i}.rs"),
                (1, 9),
                "f",
                &format!("fn f{i}() {{}}"),
            )
        })
        .collect();
    SearchResponse {
        hits: docs.clone(),
        legs: vec![
            LegHits {
                label: "docs".into(),
                instruction: None,
                hits: docs,
            },
            LegHits {
                label: "code".into(),
                instruction: None,
                hits: code,
            },
            LegHits {
                label: "tests".into(),
                instruction: None,
                hits: Vec::new(),
            },
        ],
        pairs: vec![
            Pairing {
                left: at(0, 0),
                matches: vec![PairMatch {
                    right: at(1, 0),
                    similarity: 0.71,
                }],
                drifted: false,
            },
            Pairing {
                left: at(0, 1),
                matches: Vec::new(),
                drifted: true,
            },
        ],
        duplicates: vec![Duplicate {
            a: at(1, 1),
            b: at(1, 2),
            similarity: 0.914,
        }],
        resolved: Some(echo("divergence", &["docs", "code", "tests"])),
        warnings: Vec::new(),
    }
}

#[test]
fn a_workflow_renders_its_legs_pairs_and_duplicates_as_sections() {
    let text = render(&project(), "retry", &workflow(), Detail::Compact);
    assert!(
        text.contains("intent divergence · 3 legs: docs, code, tests"),
        "{text}"
    );
    assert!(text.contains("\n## docs — 2 hits\n"), "{text}");
    assert!(text.contains("\n## tests — no match\n"), "{text}");
    assert!(
        text.contains("- README.md:L3-L9 → src/m0.rs:L1-L9 (0.71)"),
        "{text}"
    );
    assert!(
        text.contains("- docs/net.md:L1-L4 possibly drifted → no counterpart found"),
        "{text}"
    );
    assert!(
        text.contains("- src/m1.rs:L1-L9 ≈ src/m2.rs:L1-L9 (0.91)"),
        "{text}"
    );
}

#[test]
fn a_compact_workflow_shows_three_excerpts_per_leg_and_full_shows_them_all() {
    let compact = render(&project(), "retry", &workflow(), Detail::Compact);
    assert_eq!(compact.matches("```rust").count(), 3, "{compact}");
    assert!(
        compact.contains("[4] src/m3.rs:L1-L9 · method f — fn f3() {}"),
        "{compact}"
    );
    let full = render(&project(), "retry", &workflow(), Detail::Full);
    assert_eq!(full.matches("```rust").count(), 5, "{full}");
}

/// The budget the docs promise: `k = 8` excerpts of 1200 characters — the
/// MCP default — stay within ~3.5k tokens, at four characters a token.
#[test]
fn a_default_search_of_eight_full_excerpts_stays_inside_its_token_budget() {
    let body: String = "    let value = compute(input, &config)?; // a typical line\n"
        .repeat(40)
        .chars()
        .take(1_200)
        .collect();
    let hits: Vec<SearchHit> = (0..8)
        .map(|i| {
            hit(
                &format!("crates/core/src/module_{i}.rs"),
                (10, 60),
                "Engine > run",
                &body,
            )
        })
        .collect();
    let response = SearchResponse {
        hits,
        resolved: Some(echo("code", &["code"])),
        ..SearchResponse::default()
    };
    let text = render(
        &project(),
        "where is the engine run",
        &response,
        Detail::Compact,
    );
    assert!(text.len() <= 14_000, "{} chars", text.len());
    assert!(
        text.len() > 9_600,
        "the excerpts are all there: {} chars",
        text.len()
    );
}

#[test]
fn every_detail_round_trips_through_its_name() {
    for detail in [Detail::Compact, Detail::Full] {
        assert_eq!(Detail::parse(detail.as_str()), Some(detail));
    }
    assert_eq!(Detail::parse("brief"), None);
}
