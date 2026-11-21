//! What `POST /v1/search` answers: the hits — grouped by leg for a workflow
//! intent — how they relate, how the request was understood, and what the
//! daemon had to give up to produce them.
//!
//! INTENT: a hybrid search whose vector leg cannot run still has a good keyword
//! leg; the warnings say what is missing instead of failing the request or
//! passing a degraded answer off as the real one. The echo says what the model
//! was actually handed, so a bad result can be told apart from a request that
//! was resolved differently than meant.

use serde::{Deserialize, Serialize};

use crate::intent::ResolvedPlan;
use crate::model::SearchHit;

/// The answer to a search.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SearchResponse {
    /// Descending by score. For a workflow intent, its first leg — what a
    /// client that predates `legs` shows.
    pub hits: Vec<SearchHit>,
    /// A workflow intent's answer, one list per leg. Empty for a single search.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub legs: Vec<LegHits>,
    /// Each hit of the first leg with its nearest hits of the second.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pairs: Vec<Pairing>,
    /// Near-identical hits in different files: copies that may have diverged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub duplicates: Vec<Duplicate>,
    /// How the request was understood.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<ResolvedEcho>,
    /// Why the answer is narrower than asked, each naming the repair. Empty for
    /// a complete answer.
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl SearchResponse {
    /// The hit a reference points at.
    pub fn hit(&self, at: HitRef) -> Option<&SearchHit> {
        self.legs.get(at.leg)?.hits.get(at.hit)
    }
}

/// One leg of a workflow intent, and what it found.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LegHits {
    pub label: String,
    /// The instruction sent; absent when the embedder's default applied, the
    /// query went raw, or the leg searched by example.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    pub hits: Vec<SearchHit>,
}

/// A hit, by its position in [`SearchResponse::legs`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HitRef {
    pub leg: usize,
    pub hit: usize,
}

/// A hit of the first leg and the hits of the second closest to it, by the
/// cosine of their stored vectors.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Pairing {
    pub left: HitRef,
    /// Best first; empty when the second leg found nothing.
    pub matches: Vec<PairMatch>,
    /// Even the best match is weaker than `search.drift_below`: what this hit
    /// says may no longer be what the other side does.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub drifted: bool,
}

/// One match of a [`Pairing`].
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct PairMatch {
    pub right: HitRef,
    pub similarity: f32,
}

/// Two hits of different files whose stored vectors are nearly identical.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Duplicate {
    pub a: HitRef,
    pub b: HitRef,
    pub similarity: f32,
}

/// The resolution of one request, as a response echoes it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedEcho {
    /// The canonical intent name, when one drove the plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    pub legs: Vec<EchoLeg>,
    /// The model profile the vector legs ran under. Absent from a daemon that
    /// predates it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// One leg of [`ResolvedEcho`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EchoLeg {
    pub label: String,
    /// The instruction sent; absent when the embedder's default applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    /// The query went to the model verbatim.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub raw: bool,
    /// The leg searched with the stored vector of the `like` chunk.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub by_example: bool,
}

impl From<&ResolvedPlan> for ResolvedEcho {
    fn from(plan: &ResolvedPlan) -> Self {
        let legs = plan.legs.iter().map(|leg| EchoLeg {
            label: leg.label.clone(),
            instruction: leg.instruction.clone(),
            raw: leg.raw,
            by_example: leg.by_example,
        });
        Self {
            intent: plan.intent.clone(),
            legs: legs.collect(),
            model: None,
        }
    }
}

/// Either shape a daemon may answer `POST /v1/search` with.
///
/// INTENT: a client newer than its daemon must still read the bare array the
/// daemon sent before warnings existed — the CLI and the daemon are upgraded
/// separately. Decode into this, then convert.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum SearchWire {
    Current(SearchResponse),
    Legacy(Vec<SearchHit>),
}

impl From<SearchWire> for SearchResponse {
    fn from(wire: SearchWire) -> Self {
        match wire {
            SearchWire::Current(response) => response,
            SearchWire::Legacy(hits) => Self {
                hits,
                ..Self::default()
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::ResolvedLeg;
    use crate::model::SearchFilter;

    #[test]
    fn the_current_object_the_warnings_object_and_the_older_array_all_decode() {
        let current: SearchWire = serde_json::from_str(
            r#"{"hits":[],"legs":[{"label":"docs","hits":[]}],
                "pairs":[{"left":{"leg":0,"hit":0},"matches":[],"drifted":true}],
                "resolved":{"intent":"doc-sync","legs":[{"label":"docs"}]},"warnings":[]}"#,
        )
        .expect("object");
        let current = SearchResponse::from(current);
        assert_eq!(current.legs[0].label, "docs");
        assert!(current.pairs[0].drifted);

        let warned: SearchWire =
            serde_json::from_str(r#"{"hits":[],"warnings":["keyword only"]}"#).expect("object");
        let warned = SearchResponse::from(warned);
        assert_eq!(warned.warnings, vec!["keyword only".to_string()]);
        assert!(warned.legs.is_empty() && warned.resolved.is_none());

        let legacy = SearchResponse::from(serde_json::from_str::<SearchWire>("[]").expect("array"));
        assert!(legacy.hits.is_empty() && legacy.warnings.is_empty());
    }

    #[test]
    fn a_body_of_neither_shape_is_refused_rather_than_read_as_empty() {
        assert!(serde_json::from_str::<SearchWire>(r#"{"results":[]}"#).is_err());
    }

    #[test]
    fn a_single_search_keeps_the_grouping_fields_off_the_wire() {
        let wire = serde_json::to_string(&SearchResponse::default()).expect("serialize");
        assert_eq!(wire, r#"{"hits":[],"warnings":[]}"#);
    }

    #[test]
    fn the_echo_carries_each_leg_and_round_trips() {
        let plan = ResolvedPlan {
            intent: Some("usage".into()),
            workflow: false,
            legs: vec![ResolvedLeg {
                label: "usage".into(),
                instruction: Some("Given an API, retrieve its callers.".into()),
                raw: false,
                by_example: false,
                filter: SearchFilter::default(),
                mode: None,
                k: None,
            }],
            pairing: false,
            duplicates: false,
            warnings: Vec::new(),
        };
        let echo = ResolvedEcho::from(&plan);
        let wire = serde_json::to_string(&echo).expect("serialize");
        assert!(!wire.contains("raw"), "a default flag stays off: {wire}");
        let back: ResolvedEcho = serde_json::from_str(&wire).expect("parse");
        assert_eq!(back, echo);
    }

    #[test]
    fn an_echo_without_its_legs_is_refused() {
        assert!(serde_json::from_str::<ResolvedEcho>(r#"{"intent":"x"}"#).is_err());
    }
}
