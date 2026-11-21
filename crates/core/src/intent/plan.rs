//! A search request once every layer has had its say: the legs to run, each
//! with its instruction and its filter.
//!
//! The ladder, highest first, resolved here and nowhere else:
//!
//! ```text
//! raw_query            the query goes to the model verbatim
//! instruction          the request's free sentence, for every leg
//! intent / task        from the library (built-in < daemon < manifest)
//! like                 with no intent named: the `similar` intent
//! project instruction  the manifest's free sentence
//! project intent       the manifest's `task`
//! (nothing)            the embedder's configured default
//! ```
//!
//! The intent decides the LEGS even when a free sentence replaces theirs.
//! Filters only narrow: a leg intersects the request's filter with its own
//! `langs` and `kinds`; a leg left with nothing is dropped and reported.

use super::{Intent, IntentKind, IntentLibrary, Leg};
use crate::embedding::prompt::QueryPrompt;
use crate::model::{ChunkKind, Lang, SearchFilter, SearchMode, SearchRequest};
use crate::{Error, Result};

/// The label of the one leg a request without an intent runs.
pub const DEFAULT_LEG_LABEL: &str = "search";

/// The intent a request with `like` runs when it names none, and the one whose
/// legs take the example.
pub const LIKE_INTENT: &str = "similar";

/// Everything outside the request that the resolution reads.
#[derive(Clone, Copy, Debug)]
pub struct PlanContext<'a> {
    /// The built-in library with the daemon's and the project's layers.
    pub library: &'a IntentLibrary,
    /// The project scope the request named, or the empty filter.
    pub scope: &'a SearchFilter,
    /// The project's default free instruction.
    pub project_instruction: Option<&'a str>,
    /// The project's default intent, by name or alias.
    pub project_intent: Option<&'a str>,
    /// The project's languages, most chunks first: what `{codebase}` names
    /// when the filter names none.
    pub langs: &'a [Lang],
    /// `search.max_instruction_chars`.
    pub max_instruction_chars: usize,
}

/// One search to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedLeg {
    pub label: String,
    /// `None`: the embedder's configured default applies.
    pub instruction: Option<String>,
    /// The query goes to the model verbatim.
    pub raw: bool,
    /// The leg searches by the stored vector of the request's `like` chunk,
    /// and embeds nothing.
    pub by_example: bool,
    pub filter: SearchFilter,
    /// `None`: `search.default_mode`.
    pub mode: Option<SearchMode>,
    /// `None`: `search.default_k`.
    pub k: Option<usize>,
}

impl ResolvedLeg {
    /// How the embedder is to render this leg's query.
    pub fn prompt(&self) -> QueryPrompt<'_> {
        match (&self.instruction, self.raw) {
            (_, true) => QueryPrompt::Raw,
            (Some(text), false) => QueryPrompt::Instruct(text),
            (None, false) => QueryPrompt::Default,
        }
    }
}

/// A request, resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedPlan {
    /// The canonical name of the intent that drove the plan, if any did.
    pub intent: Option<String>,
    /// A workflow intent drove it: the answer is grouped by leg.
    pub workflow: bool,
    pub legs: Vec<ResolvedLeg>,
    /// The first two legs are to be matched against each other.
    pub pairing: bool,
    /// Near-duplicate code across files is to be reported among the hits.
    pub duplicates: bool,
    /// What the resolution had to leave out, and why.
    pub warnings: Vec<String>,
}

impl SearchRequest {
    /// Resolves this request against `ctx`; see the module docs for the ladder.
    ///
    /// Refused with [`Error::Invalid`]: an unknown intent (listing the known
    /// ones), `intent` and `task` naming two intents, a `like` the intent has
    /// no leg for, and a filter that leaves every leg nothing to search.
    pub fn resolve_plan(&self, ctx: &PlanContext<'_>) -> Result<ResolvedPlan> {
        let base = ctx.scope.layer(&self.filter);
        match self.pick_intent(ctx)? {
            Some(intent) => self.intent_plan(intent, &base, ctx),
            None => Ok(self.single_leg(base, ctx)),
        }
    }

    /// The intent the ladder lands on, if any.
    fn pick_intent<'l>(&self, ctx: &PlanContext<'l>) -> Result<Option<&'l Intent>> {
        if let Some(intent) = self.requested_intent(ctx.library)? {
            return Ok(Some(intent));
        }
        if self.like.is_some() {
            return ctx.library.get(LIKE_INTENT).map(Some);
        }
        let speaks = non_blank(self.instruction.as_deref()).is_some()
            || non_blank(ctx.project_instruction).is_some();
        match non_blank(ctx.project_intent) {
            Some(name) if !speaks => ctx.library.get(name).map(Some),
            _ => Ok(None),
        }
    }

    /// The intent the request itself names, through `intent` or `task`.
    fn requested_intent<'l>(&self, library: &'l IntentLibrary) -> Result<Option<&'l Intent>> {
        let by_intent = match non_blank(self.intent.as_deref()) {
            Some(name) => Some(library.get(name)?),
            None => None,
        };
        let by_task = match self.task {
            Some(task) => Some(library.get(task.as_str())?),
            None => None,
        };
        match (by_intent, by_task) {
            (Some(a), Some(b)) if a.name != b.name => Err(Error::Invalid(format!(
                "`intent` names `{}` and `task` names `{}`; send one of them",
                a.name, b.name
            ))),
            (a, b) => Ok(a.or(b)),
        }
    }

    /// No intent: one leg, with the free or the project's sentence.
    fn single_leg(&self, filter: SearchFilter, ctx: &PlanContext<'_>) -> ResolvedPlan {
        let sentence =
            non_blank(self.instruction.as_deref()).or(non_blank(ctx.project_instruction));
        let instruction = if self.raw_query {
            None
        } else {
            sentence.map(str::to_string)
        };
        let leg = ResolvedLeg {
            label: DEFAULT_LEG_LABEL.to_string(),
            instruction,
            raw: self.raw_query,
            by_example: false,
            filter,
            mode: self.mode,
            k: self.k,
        };
        ResolvedPlan {
            intent: None,
            workflow: false,
            legs: vec![leg],
            pairing: false,
            duplicates: false,
            warnings: Vec::new(),
        }
    }

    fn intent_plan(
        &self,
        intent: &Intent,
        base: &SearchFilter,
        ctx: &PlanContext<'_>,
    ) -> Result<ResolvedPlan> {
        let mut legs = Vec::with_capacity(intent.legs.len());
        let mut warnings = Vec::new();
        for leg in &intent.legs {
            match narrow(base, leg) {
                Some(narrowed) => legs.push(self.resolve_leg(intent, leg, narrowed, ctx)),
                None => warnings.push(dropped(intent, leg)),
            }
        }
        if legs.is_empty() {
            return Err(Error::Invalid(format!(
                "intent `{}` has nothing to search under this filter: {}",
                intent.name,
                warnings.join("; ")
            )));
        }
        if self.like.is_some() && !legs.iter().any(|l| l.by_example) {
            return Err(Error::Invalid(format!(
                "`like` is the example of a `{LIKE_INTENT}` search, and intent `{}` runs none; \
                 drop the intent, or pick one with a `{LIKE_INTENT}` leg",
                intent.name
            )));
        }
        let pairing = intent.pairing && legs.len() == intent.legs.len();
        if intent.pairing && !pairing {
            warnings.push(format!(
                "`{}` pairs its legs, and one of them was left out, so nothing is paired",
                intent.name
            ));
        }
        Ok(ResolvedPlan {
            intent: Some(intent.name.clone()),
            workflow: intent.kind == IntentKind::Workflow,
            legs,
            pairing,
            duplicates: intent.duplicates,
            warnings,
        })
    }

    fn resolve_leg(
        &self,
        intent: &Intent,
        leg: &Leg,
        narrowed: Narrowed,
        ctx: &PlanContext<'_>,
    ) -> ResolvedLeg {
        let by_example = self.like.is_some() && takes_example(intent, leg);
        let instruction = if by_example || self.raw_query {
            None
        } else if let Some(free) = non_blank(self.instruction.as_deref()) {
            Some(free.to_string())
        } else {
            let langs = narrowed.named_langs(leg, ctx.langs);
            Some(leg.instruction_for(&langs, ctx.max_instruction_chars))
        };
        ResolvedLeg {
            label: leg.label.clone(),
            instruction,
            raw: self.raw_query && !by_example,
            by_example,
            filter: narrowed.filter,
            mode: self.mode.or(leg.mode),
            k: self.k.or(leg.k),
        }
    }
}

/// Whether `leg` of `intent` searches by the request's example.
fn takes_example(intent: &Intent, leg: &Leg) -> bool {
    intent.name == LIKE_INTENT || leg.borrows.as_deref() == Some(LIKE_INTENT)
}

fn non_blank(raw: Option<&str>) -> Option<&str> {
    raw.map(str::trim).filter(|s| !s.is_empty())
}

/// A leg's filter, and whether its languages were chosen (by the request, the
/// scope or the leg) rather than "everything but".
struct Narrowed {
    filter: SearchFilter,
    chosen_langs: bool,
}

impl Narrowed {
    /// The languages a leg's instruction names: the chosen ones, else the
    /// project's mix less what the leg excludes.
    fn named_langs(&self, leg: &Leg, project: &[Lang]) -> Vec<Lang> {
        if self.chosen_langs {
            return self.filter.langs.clone();
        }
        let kept = project.iter().filter(|l| !leg.exclude_langs.contains(l));
        kept.copied().collect()
    }
}

/// `base` narrowed by `leg`, or `None` when nothing is left.
fn narrow(base: &SearchFilter, leg: &Leg) -> Option<Narrowed> {
    let mut filter = base.clone();
    let mut chosen_langs = !base.langs.is_empty();
    if !leg.langs.is_empty() {
        filter.langs = intersect(&base.langs, &leg.langs)?;
        chosen_langs = true;
    }
    if !leg.exclude_langs.is_empty() {
        filter.langs = exclude(&filter.langs, &leg.exclude_langs)?;
    }
    if !leg.kinds.is_empty() {
        filter.kinds = intersect(&base.kinds, &leg.kinds)?;
    }
    Some(Narrowed {
        filter,
        chosen_langs,
    })
}

/// `wanted` within `allowed`, where an empty `allowed` allows everything.
fn intersect<T: Copy + PartialEq>(allowed: &[T], wanted: &[T]) -> Option<Vec<T>> {
    if allowed.is_empty() {
        return Some(wanted.to_vec());
    }
    let kept: Vec<T> = allowed
        .iter()
        .copied()
        .filter(|x| wanted.contains(x))
        .collect();
    (!kept.is_empty()).then_some(kept)
}

/// `langs` without `excluded`, where an empty `langs` means every language.
fn exclude(langs: &[Lang], excluded: &[Lang]) -> Option<Vec<Lang>> {
    let universe = if langs.is_empty() { Lang::ALL } else { langs };
    let kept: Vec<Lang> = universe
        .iter()
        .copied()
        .filter(|l| !excluded.contains(l))
        .collect();
    (!kept.is_empty()).then_some(kept)
}

/// The warning for a leg the filter emptied, naming what it searches.
fn dropped(intent: &Intent, leg: &Leg) -> String {
    let langs: Vec<&str> = leg.langs.iter().map(Lang::as_str).collect();
    let kinds: Vec<&str> = leg.kinds.iter().map(ChunkKind::as_str).collect();
    let needs = match (kinds.is_empty(), langs.is_empty()) {
        (true, true) => String::new(),
        (true, false) => format!(" (it searches {})", langs.join(", ")),
        (false, true) => format!(" (it searches {} chunks)", kinds.join(", ")),
        (false, false) => format!(
            " (it searches {} chunks in {})",
            kinds.join(", "),
            langs.join(", ")
        ),
    };
    format!(
        "leg `{}` of `{}` was left out: the filter leaves it nothing to search{needs}",
        leg.label, intent.name
    )
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;
