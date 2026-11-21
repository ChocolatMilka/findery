//! The query vector of a search: the member that holds the project's vectors
//! under its model, and the query embedded once — or, where hybrid allows it,
//! the warning that replaces them.

use std::sync::Arc;

use findery_core::embedding::prompt::QueryPrompt;
use findery_core::model::SearchMode;
use findery_core::{Error, Result};

use super::vectors;
use crate::gates::Lease;
use crate::storage::families::BucketHandle;

/// The vectors a search compares against: the member holding them, and the
/// query vector, if any.
#[derive(Default)]
pub(super) struct Gathered {
    pub(super) member: Option<Arc<BucketHandle>>,
    /// `None` in keyword mode, or where the embedder failed and hybrid degraded.
    pub(super) vector: Option<Vec<f32>>,
}

/// The member and the query vector `mode` needs. In hybrid mode a failure
/// becomes a warning; in vector mode it is the answer.
pub(super) async fn gather(
    lease: &Lease,
    query: &str,
    instruction: Option<&str>,
    mode: SearchMode,
    warnings: &mut Vec<String>,
) -> Result<Gathered> {
    if mode == SearchMode::Keyword {
        return Ok(Gathered::default());
    }
    let strict = mode == SearchMode::Vector;
    let member = match vectors::vector_member(lease).await {
        Ok(member) => member,
        Err(e) if !strict && degrades(&e) => {
            push_once(warnings, degraded(lease.alias.as_str(), &e));
            return Ok(Gathered::default());
        }
        Err(e) => return Err(e),
    };
    let prompt = match instruction {
        Some(instruction) => QueryPrompt::Instruct(instruction),
        None => QueryPrompt::Default,
    };
    let embedded = match lease.embedder().await {
        Ok(embedder) => embedder.embed_query(query, prompt).await,
        Err(e) => Err(e),
    };
    let vector = match embedded {
        Ok(vector) => Some(vector),
        Err(e) if !strict && degrades(&e) => {
            push_once(warnings, degraded(lease.alias.as_str(), &e));
            None
        }
        Err(e) => return Err(e),
    };
    Ok(Gathered {
        member: Some(member),
        vector,
    })
}

/// The failures of the vector list a hybrid search survives.
fn degrades(e: &Error) -> bool {
    matches!(
        e,
        Error::VectorsUnavailable(_)
            | Error::RecipeMismatch(_)
            | Error::Upstream { .. }
            | Error::Embed(_)
    )
}

/// What is missing, whose failure it is — the model's runtime or the vector
/// store's service — and the command that brings it back.
fn degraded(alias: &str, e: &Error) -> String {
    match e {
        Error::Upstream { service, detail } if service != findery_embed::UPSTREAM_SERVICE => {
            format!(
                "keyword results only: the vector store `{service}` did not answer ({detail}); \
                 start it, then search again"
            )
        }
        Error::Upstream { .. } | Error::Embed(_) => format!(
            "keyword results only: model `{alias}` did not answer ({}); start its runtime, then \
             search again",
            e.detail()
        ),
        _ => format!("keyword results only: {}", e.detail()),
    }
}

fn push_once(warnings: &mut Vec<String>, warning: String) {
    if !warnings.contains(&warning) {
        warnings.push(warning);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this locks: a vector store that was down was reported as the
    /// model not answering, sending the user to restart the wrong service.
    #[test]
    fn a_degraded_search_blames_the_service_that_failed() {
        let store_down = degraded("harrier", &Error::upstream("qdrant", "connection refused"));
        assert!(store_down.contains("vector store `qdrant`"), "{store_down}");
        assert!(!store_down.contains("model `harrier`"), "{store_down}");

        let model_down = degraded(
            "harrier",
            &Error::upstream(findery_embed::UPSTREAM_SERVICE, "connection refused"),
        );
        assert!(
            model_down.contains("model `harrier` did not answer"),
            "{model_down}"
        );

        let drifted = degraded(
            "harrier",
            &Error::recipe_drift("harrier", "h:2560", "h:1024"),
        );
        assert!(drifted.contains("now serves `h:2560`"), "{drifted}");
        assert!(!drifted.contains("did not answer"), "{drifted}");
    }
}
