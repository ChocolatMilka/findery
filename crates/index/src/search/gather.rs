//! The query vectors of a search: the member that holds the project's vectors
//! under its model, the example of a `like` leg, and each distinct prompt
//! embedded once — or, where hybrid allows it, the warning that replaces them.

use std::sync::Arc;

use findery_core::embedding::prompt::QueryPrompt;
use findery_core::intent::ResolvedPlan;
use findery_core::model::{SearchMode, SearchRequest};
use findery_core::{Error, Result};

use super::vectors::{self, Example};
use crate::gates::Lease;
use crate::storage::families::BucketHandle;

/// The vectors a search compares against: the member holding them, and for
/// each leg the query vector it searches with, if any.
#[derive(Default)]
pub(super) struct Gathered {
    pub(super) member: Option<Arc<BucketHandle>>,
    pub(super) example: Option<Example>,
    /// One per distinct prompt; `None` where the embedder failed and the leg
    /// degraded.
    embedded: Vec<Option<Vec<f32>>>,
    /// Per leg, which of `embedded` it uses.
    slots: Vec<Option<usize>>,
}

impl Gathered {
    pub(super) fn vector_of(&self, leg: usize, by_example: bool) -> Option<&[f32]> {
        if by_example {
            return self.example.as_ref().map(|e| e.vector.as_slice());
        }
        let slot = self.slots.get(leg).copied().flatten()?;
        self.embedded.get(slot)?.as_deref()
    }
}

pub(super) async fn gather(
    lease: &Lease,
    req: &SearchRequest,
    plan: &ResolvedPlan,
    modes: &[SearchMode],
    warnings: &mut Vec<String>,
) -> Result<Gathered> {
    let strict = modes.contains(&SearchMode::Vector);
    if modes.iter().all(|m| *m == SearchMode::Keyword) {
        return Ok(Gathered::default());
    }
    let member = match vectors::vector_member(lease).await {
        Ok(member) => member,
        Err(e) if !strict && degrades(&e) => {
            push_once(warnings, degraded(lease.alias.as_str(), &e));
            return Ok(Gathered::default());
        }
        Err(e) => return Err(e),
    };
    let example = match &req.like {
        Some(raw) => Some(vectors::example(lease, &member, raw).await?),
        None => None,
    };

    let prompts = Prompts::of(plan, modes);
    let embedded = embed(lease, &req.query, &prompts, warnings).await?;
    Ok(Gathered {
        member: Some(member),
        example,
        embedded,
        slots: prompts.slots,
    })
}

/// The distinct prompts of a plan — legs that render their query the same
/// way share one embedding — and which one each leg uses.
struct Prompts<'p> {
    distinct: Vec<QueryPrompt<'p>>,
    /// Per prompt: a strict vector leg uses it, so its failure is the answer.
    strict: Vec<bool>,
    /// Per leg; `None` for a keyword or a `like` leg, which embeds nothing.
    slots: Vec<Option<usize>>,
}

impl<'p> Prompts<'p> {
    fn of(plan: &'p ResolvedPlan, modes: &[SearchMode]) -> Self {
        let mut prompts = Self {
            distinct: Vec::new(),
            strict: Vec::new(),
            slots: Vec::with_capacity(plan.legs.len()),
        };
        for (leg, mode) in plan.legs.iter().zip(modes) {
            let embeds = !leg.by_example && *mode != SearchMode::Keyword;
            let slot = if embeds {
                Some(prompts.slot_for(leg.prompt(), *mode))
            } else {
                None
            };
            prompts.slots.push(slot);
        }
        prompts
    }

    fn slot_for(&mut self, prompt: QueryPrompt<'p>, mode: SearchMode) -> usize {
        let slot = match self.distinct.iter().position(|p| *p == prompt) {
            Some(slot) => slot,
            None => {
                self.distinct.push(prompt);
                self.strict.push(false);
                self.distinct.len() - 1
            }
        };
        self.strict[slot] |= mode == SearchMode::Vector;
        slot
    }
}

/// Each prompt embedded once; a failure degrades the legs using it, unless one
/// of them is a strict vector leg.
async fn embed(
    lease: &Lease,
    query: &str,
    prompts: &Prompts<'_>,
    warnings: &mut Vec<String>,
) -> Result<Vec<Option<Vec<f32>>>> {
    let (distinct, strict) = (&prompts.distinct, &prompts.strict);
    if distinct.is_empty() {
        return Ok(Vec::new());
    }
    let embedder = match lease.embedder().await {
        Ok(embedder) => embedder,
        Err(e) if !strict.contains(&true) && degrades(&e) => {
            push_once(warnings, degraded(lease.alias.as_str(), &e));
            return Ok(vec![None; distinct.len()]);
        }
        Err(e) => return Err(e),
    };
    let results = vectors::embed_all(embedder.as_ref(), query, distinct).await;
    let mut out = Vec::with_capacity(results.len());
    for (result, strict) in results.into_iter().zip(strict) {
        match result {
            Ok(vector) => out.push(Some(vector)),
            Err(e) if !strict && degrades(&e) => {
                push_once(warnings, degraded(lease.alias.as_str(), &e));
                out.push(None);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(out)
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
