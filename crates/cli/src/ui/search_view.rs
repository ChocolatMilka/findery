//! How a search answer reads in a terminal: the one-line resolution, a
//! workflow's legs, its pairs and near-duplicates, and the intent listing.
//!
//! Everything renders to a `String` with a [`Paint`], so a test reads exactly
//! what a user would, without escape codes.

use findery_core::api::{IntentInfo, Pairing, ResolvedEcho, SearchResponse};
use findery_core::intent::IntentKind;
use findery_core::model::SearchHit;

use crate::ui::style::{Col, Columns, Paint};

/// What the model was handed, on one line: the intent, and the instruction
/// or the legs.
pub(crate) fn resolved_line(echo: &ResolvedEcho, paint: Paint) -> String {
    let what = match echo.legs.as_slice() {
        [one] => how_prompted(one),
        legs => {
            let labels: Vec<&str> = legs.iter().map(|l| l.label.as_str()).collect();
            format!("{} legs: {}", legs.len(), labels.join(", "))
        }
    };
    let line = match &echo.intent {
        Some(intent) => format!("intent {intent} · {what}"),
        None => what,
    };
    paint.dim(&line)
}

fn how_prompted(leg: &findery_core::api::EchoLeg) -> String {
    if leg.by_example {
        return "by example (stored vector, nothing embedded)".to_string();
    }
    if leg.raw {
        return "raw query (no instruction)".to_string();
    }
    match &leg.instruction {
        Some(instruction) => instruction.clone(),
        None => "the model's default instruction".to_string(),
    }
}

/// A workflow's answer: each leg, then the pairs, then the near-duplicates.
pub(crate) fn grouped(response: &SearchResponse, paint: Paint) -> String {
    let mut out = String::new();
    for leg in &response.legs {
        let count = match leg.hits.len() {
            0 => "no match".to_string(),
            1 => "1 hit".to_string(),
            n => format!("{n} hits"),
        };
        out.push_str(&format!(
            "\n{}  {}\n",
            paint.bold(&leg.label),
            paint.dim(&count)
        ));
        for (i, hit) in leg.hits.iter().enumerate() {
            out.push_str(&hit_line(i + 1, hit, paint));
        }
    }
    out.push_str(&pairs(response, paint));
    out.push_str(&duplicates(response, paint));
    out
}

/// `  [1] 0.031  src/net.rs:1-4  function  Net > retry`, and the first line
/// of the excerpt beneath.
fn hit_line(rank: usize, hit: &SearchHit, paint: Paint) -> String {
    let chunk = &hit.chunk;
    let symbol = chunk.breadcrumb.as_deref().or(chunk.symbol.as_deref());
    let mut line = format!(
        "  [{rank}] {:.3}  {}  {}",
        hit.score,
        chunk.locator(),
        chunk.kind.as_str()
    );
    if let Some(symbol) = symbol {
        line.push_str(&format!("  {symbol}"));
    }
    let first = chunk.text.lines().map(str::trim).find(|l| !l.is_empty());
    match first {
        Some(text) => format!("{line}\n      {}\n", paint.dim(text)),
        None => format!("{line}\n"),
    }
}

fn pairs(response: &SearchResponse, paint: Paint) -> String {
    if response.pairs.is_empty() {
        return String::new();
    }
    let side = |at: usize| response.legs.get(at).map_or("?", |l| l.label.as_str());
    let title = format!("{} → {}", side(0), side(1));
    let mut out = format!("\n{}  {}\n", paint.bold("pairs"), paint.dim(&title));
    for pairing in &response.pairs {
        out.push_str(&pairing_lines(response, pairing, paint));
    }
    out
}

fn pairing_lines(response: &SearchResponse, pairing: &Pairing, paint: Paint) -> String {
    let Some(left) = response.hit(pairing.left) else {
        return String::new();
    };
    let flag = if pairing.drifted {
        "  possibly drifted"
    } else {
        ""
    };
    let mut out = format!("  {}{}\n", left.chunk.locator(), paint.bold(flag));
    if pairing.matches.is_empty() {
        out.push_str(&format!("      {}\n", paint.dim("no counterpart found")));
    }
    for found in &pairing.matches {
        if let Some(right) = response.hit(found.right) {
            let score = format!("{:.3}", found.similarity);
            out.push_str(&format!(
                "      {}  {}\n",
                paint.dim(&score),
                right.chunk.locator()
            ));
        }
    }
    out
}

fn duplicates(response: &SearchResponse, paint: Paint) -> String {
    if response.duplicates.is_empty() {
        return String::new();
    }
    let mut out = format!("\n{}\n", paint.bold("near-duplicates"));
    for duplicate in &response.duplicates {
        let (Some(a), Some(b)) = (response.hit(duplicate.a), response.hit(duplicate.b)) else {
            continue;
        };
        let score = format!("{:.3}", duplicate.similarity);
        out.push_str(&format!(
            "  {}  {}  ≈  {}\n",
            paint.dim(&score),
            a.chunk.locator(),
            b.chunk.locator()
        ));
    }
    out
}

/// The library, locate intents first: a name and its summary, and for a
/// workflow the legs it runs.
pub(crate) fn intents(list: &[IntentInfo], paint: Paint) -> String {
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|i| vec![i.name.clone(), i.summary.clone()])
        .collect();
    let columns = Columns::fit(&rows);
    let indent = list
        .iter()
        .map(|i| i.name.chars().count())
        .max()
        .unwrap_or(0)
        + 4;
    let mut out = String::new();
    for kind in [IntentKind::Locate, IntentKind::Workflow] {
        out.push_str(&format!("{}\n", paint.bold(kind.as_str())));
        for (info, row) in list.iter().zip(&rows).filter(|(i, _)| i.kind == kind) {
            out.push_str(&format!(
                "  {}\n",
                columns.line(row, &[Col::BOLD, Col::LEFT], paint)
            ));
            if kind == IntentKind::Workflow {
                let legs = info.legs.join(" → ");
                out.push_str(&format!("{:indent$}{}\n", "", paint.dim(&legs)));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use findery_core::api::{Duplicate, EchoLeg, HitRef, LegHits, PairMatch};
    use findery_core::ids::{ChunkId, ContentHash, ProjectId};
    use findery_core::model::{Chunk, ChunkKind, Lang};

    use super::*;

    fn hit(path: &str, lines: (u32, u32), kind: ChunkKind, symbol: &str, text: &str) -> SearchHit {
        let project = ProjectId::new("findery").expect("id");
        SearchHit {
            chunk: Chunk {
                id: ChunkId::derive(&project, path, lines.0),
                project,
                path: path.into(),
                lang: Lang::from_path(path),
                ordinal: 0,
                kind,
                symbol: None,
                breadcrumb: Some(symbol.into()),
                start_line: lines.0,
                end_line: lines.1,
                content_hash: ContentHash::of(text.as_bytes()),
                text: text.into(),
            },
            score: 0.031,
            vector_score: Some(0.61),
            keyword_score: None,
            host_path: format!("C:/dev/findery/{path}"),
        }
    }

    /// `findery search --intent doc-sync "how buckets are removed"`, as the
    /// daemon could answer it.
    fn doc_sync() -> SearchResponse {
        let docs = vec![
            hit(
                ".claude/docs/index/README.md",
                (210, 238),
                ChunkKind::DocSection,
                "Detach and rm",
                "## Detach and rm\n`findery rm` drains the project, then deletes its files.",
            ),
            hit(
                ".claude/docs/store/README.md",
                (88, 102),
                ChunkKind::DocSection,
                "Removing a bucket",
                "### Removing a bucket\nGoes through the daemon, never the filesystem.",
            ),
        ];
        let code = vec![
            hit(
                "crates/index/src/lifecycle.rs",
                (140, 196),
                ChunkKind::Method,
                "Indexer > remove_project",
                "    pub async fn remove_project(&self, id: &ProjectId) -> Result<ProjectRemoved> {",
            ),
            hit(
                "crates/index/src/buckets.rs",
                (61, 110),
                ChunkKind::Method,
                "Indexer > remove_bucket",
                "    pub async fn remove_bucket(&self, name: &str) -> Result<BucketRemoved> {",
            ),
        ];
        let at = |leg, hit| HitRef { leg, hit };
        let instruction = |s: &str| Some(s.to_string());
        SearchResponse {
            hits: docs.clone(),
            legs: vec![
                LegHits {
                    label: "docs".into(),
                    instruction: instruction("Given a feature or behaviour of a project, …"),
                    hits: docs,
                },
                LegHits {
                    label: "code".into(),
                    instruction: instruction("Given a documented feature or behaviour of …"),
                    hits: code,
                },
            ],
            pairs: vec![
                Pairing {
                    left: at(0, 0),
                    matches: vec![
                        PairMatch {
                            right: at(1, 0),
                            similarity: 0.712,
                        },
                        PairMatch {
                            right: at(1, 1),
                            similarity: 0.538,
                        },
                    ],
                    drifted: false,
                },
                Pairing {
                    left: at(0, 1),
                    matches: vec![PairMatch {
                        right: at(1, 1),
                        similarity: 0.431,
                    }],
                    drifted: true,
                },
            ],
            duplicates: vec![Duplicate {
                a: at(1, 0),
                b: at(1, 1),
                similarity: 0.874,
            }],
            resolved: Some(ResolvedEcho {
                intent: Some("doc-sync".into()),
                legs: vec![
                    EchoLeg {
                        label: "docs".into(),
                        ..EchoLeg::default()
                    },
                    EchoLeg {
                        label: "code".into(),
                        ..EchoLeg::default()
                    },
                ],
                model: None,
            }),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn a_doc_sync_answer_reads_as_legs_then_pairs() {
        let response = doc_sync();
        let plain = Paint::plain();
        let echo = response.resolved.as_ref().expect("echo");
        assert_eq!(
            resolved_line(echo, plain),
            "intent doc-sync · 2 legs: docs, code"
        );
        let text = grouped(&response, plain);
        let expected = "
docs  2 hits
  [1] 0.031  .claude/docs/index/README.md:210-238  doc_section  Detach and rm
      ## Detach and rm
  [2] 0.031  .claude/docs/store/README.md:88-102  doc_section  Removing a bucket
      ### Removing a bucket

code  2 hits
  [1] 0.031  crates/index/src/lifecycle.rs:140-196  method  Indexer > remove_project
      pub async fn remove_project(&self, id: &ProjectId) -> Result<ProjectRemoved> {
  [2] 0.031  crates/index/src/buckets.rs:61-110  method  Indexer > remove_bucket
      pub async fn remove_bucket(&self, name: &str) -> Result<BucketRemoved> {

pairs  docs → code
  .claude/docs/index/README.md:210-238
      0.712  crates/index/src/lifecycle.rs:140-196
      0.538  crates/index/src/buckets.rs:61-110
  .claude/docs/store/README.md:88-102  possibly drifted
      0.431  crates/index/src/buckets.rs:61-110

near-duplicates
  0.874  crates/index/src/lifecycle.rs:140-196  ≈  crates/index/src/buckets.rs:61-110
";
        assert_eq!(text, expected, "\n{text}");
    }

    #[test]
    fn a_single_leg_line_says_how_the_query_was_prompted() {
        let echo = |leg: EchoLeg, intent: Option<&str>| ResolvedEcho {
            intent: intent.map(str::to_string),
            legs: vec![leg],
            model: None,
        };
        let plain = Paint::plain();
        let raw = EchoLeg {
            label: "search".into(),
            raw: true,
            ..EchoLeg::default()
        };
        assert_eq!(
            resolved_line(&echo(raw, None), plain),
            "raw query (no instruction)"
        );
        let usage = EchoLeg {
            label: "usage".into(),
            instruction: Some("Given an API, retrieve its callers.".into()),
            ..EchoLeg::default()
        };
        assert_eq!(
            resolved_line(&echo(usage, Some("usage")), plain),
            "intent usage · Given an API, retrieve its callers."
        );
        let example = EchoLeg {
            label: "similar".into(),
            by_example: true,
            ..EchoLeg::default()
        };
        assert!(resolved_line(&echo(example, Some("similar")), plain).contains("by example"));
    }

    #[test]
    fn the_intent_listing_groups_by_kind_and_shows_a_workflows_legs() {
        let info = |name: &str, kind, legs: &[&str]| IntentInfo {
            name: name.into(),
            aliases: Vec::new(),
            kind,
            summary: format!("{name} summary"),
            legs: legs.iter().map(|l| l.to_string()).collect(),
        };
        let list = [
            info("usage", IntentKind::Locate, &["usage"]),
            info("doc-sync", IntentKind::Workflow, &["docs", "code"]),
        ];
        let text = intents(&list, Paint::plain());
        assert_eq!(
            text,
            "locate\n  usage     usage summary\nworkflow\n  doc-sync  doc-sync summary\n            docs → code\n"
        );
    }
}
