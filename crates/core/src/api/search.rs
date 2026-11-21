//! What `POST /v1/search` answers: the hits, and what the daemon had to give
//! up to produce them.
//!
//! INTENT: a hybrid search whose vector leg cannot run still has a good keyword
//! leg; the warnings say what is missing instead of failing the request or
//! passing a degraded answer off as the real one.

use serde::{Deserialize, Serialize};

use crate::model::SearchHit;

/// The answer to a search.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SearchResponse {
    /// Descending by score.
    pub hits: Vec<SearchHit>,
    /// Why the answer is narrower than asked, each naming the repair. Empty for
    /// a complete answer.
    #[serde(default)]
    pub warnings: Vec<String>,
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

    #[test]
    fn the_warnings_object_and_the_older_array_both_decode() {
        let warned: SearchWire =
            serde_json::from_str(r#"{"hits":[],"warnings":["keyword only"]}"#).expect("object");
        let warned = SearchResponse::from(warned);
        assert_eq!(warned.warnings, vec!["keyword only".to_string()]);

        let legacy = SearchResponse::from(serde_json::from_str::<SearchWire>("[]").expect("array"));
        assert!(legacy.hits.is_empty() && legacy.warnings.is_empty());
    }

    #[test]
    fn a_body_of_neither_shape_is_refused_rather_than_read_as_empty() {
        assert!(serde_json::from_str::<SearchWire>(r#"{"results":[]}"#).is_err());
    }

    #[test]
    fn a_complete_answer_is_its_hits_and_an_empty_warning_list() {
        let wire = serde_json::to_string(&SearchResponse::default()).expect("serialize");
        assert_eq!(wire, r#"{"hits":[],"warnings":[]}"#);
    }
}
