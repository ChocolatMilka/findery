//! Intents: named retrieval tasks, and the plan of searches each one runs.
//!
//! The library is data — `crates/core/intents.toml`, embedded at compile time —
//! layered by the daemon config and the project manifest
//! ([`IntentLibrary::merged`]), and turned into legs by
//! [`crate::model::SearchRequest::resolve_plan`].
//!
//! INTENT: an instruction-aware model gains 1-5% from an instruction that
//! states the TASK. An agent asked to write one per call paraphrases its query;
//! a short list of named tasks is what it can pick well.

mod build;
mod plan;
mod source;
mod task;
mod template;

pub use plan::{DEFAULT_LEG_LABEL, LIKE_INTENT, PlanContext, ResolvedLeg, ResolvedPlan};
pub use source::{HintTable, IntentOverride, LegSpec};
pub use task::QueryTask;
pub use template::{Hint, InstructionTemplate};

use std::collections::BTreeMap;
use std::sync::LazyLock;

use crate::model::{ChunkKind, Lang, SearchMode};
use crate::{Error, Result};
use source::{IntentSource, LibraryFile};

/// The library every daemon starts from.
const BUILTIN_SOURCE: &str = include_str!("../../intents.toml");

/// Default of `search.max_instruction_chars`: the sentence and the hints of
/// about three languages.
pub const DEFAULT_MAX_INSTRUCTION_CHARS: usize = 640;

/// Parsed once. A failure is kept as text because [`Error`] is not `Clone`.
static BUILTIN: LazyLock<std::result::Result<IntentLibrary, String>> =
    LazyLock::new(|| IntentLibrary::from_toml_str(BUILTIN_SOURCE).map_err(|e| e.to_string()));

/// What an intent is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntentKind {
    /// One question, one leg: where is X, who calls X.
    Locate,
    /// The several searches one task needs: review, refactor.
    Workflow,
}

impl IntentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Locate => "locate",
            Self::Workflow => "workflow",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "locate" => Some(Self::Locate),
            "workflow" => Some(Self::Workflow),
            _ => None,
        }
    }
}

crate::serde_via_parse!(IntentKind, option);

/// One leg of an intent, with what it borrows already resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leg {
    pub label: String,
    /// The locate intent this leg borrows its sentence and hints from.
    pub borrows: Option<String>,
    instruction: InstructionTemplate,
    hints: Vec<Hint>,
    pub kinds: Vec<ChunkKind>,
    pub langs: Vec<Lang>,
    pub exclude_langs: Vec<Lang>,
    pub mode: Option<SearchMode>,
    pub k: Option<usize>,
}

impl Leg {
    /// This leg's sentence, as written.
    pub fn instruction(&self) -> &InstructionTemplate {
        &self.instruction
    }

    /// This leg's instruction for `langs` (priority order), within `max_chars`.
    pub fn instruction_for(&self, langs: &[Lang], max_chars: usize) -> String {
        self.instruction.render(langs, &self.hints, max_chars)
    }

    /// The constructs this leg looks for in `lang`; empty when it names none.
    pub fn hints_for(&self, lang: Lang) -> &[String] {
        hints_for(&self.hints, lang)
    }

    /// Takes from the borrowed intent's leg what this one leaves unsaid.
    fn inherit(&mut self, from: &Leg) {
        if self.langs.is_empty() && self.exclude_langs.is_empty() {
            self.langs.clone_from(&from.langs);
            self.exclude_langs.clone_from(&from.exclude_langs);
        }
        if self.kinds.is_empty() {
            self.kinds.clone_from(&from.kinds);
        }
        self.mode = self.mode.or(from.mode);
        self.k = self.k.or(from.k);
    }
}

/// A named retrieval task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Intent {
    pub name: String,
    pub aliases: Vec<String>,
    /// One line, for an agent choosing between intents.
    pub summary: String,
    pub kind: IntentKind,
    instruction: InstructionTemplate,
    hints: Vec<Hint>,
    pub legs: Vec<Leg>,
    /// Each hit of the first leg is matched with its nearest hits of the second.
    pub pairing: bool,
    /// Near-duplicate code across files is reported among the hits.
    pub duplicates: bool,
    /// The sentence with no language named, rendered once.
    neutral: String,
}

impl Intent {
    /// The intent's own sentence, as written.
    pub fn instruction(&self) -> &InstructionTemplate {
        &self.instruction
    }

    /// The languages this intent carries a hint for.
    pub fn hinted_langs(&self) -> impl Iterator<Item = Lang> + '_ {
        self.hints.iter().map(|hint| hint.lang)
    }

    /// The instruction for `langs` (priority order), within `max_chars`.
    pub fn instruction_for(&self, langs: &[Lang], max_chars: usize) -> String {
        self.instruction.render(langs, &self.hints, max_chars)
    }

    /// The instruction when nothing is known about the languages.
    pub fn neutral_instruction(&self) -> &str {
        &self.neutral
    }

    /// The constructs this intent looks for in `lang`; empty when it names none.
    pub fn hints_for(&self, lang: Lang) -> &[String] {
        hints_for(&self.hints, lang)
    }

    fn answers_to(&self, wanted: &str) -> bool {
        self.name == wanted || self.aliases.iter().any(|a| a == wanted)
    }
}

/// An ordered, validated set of intents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntentLibrary {
    /// Kept so a layer can be added on top and the whole re-validated.
    sources: Vec<IntentSource>,
    intents: Vec<Intent>,
}

impl IntentLibrary {
    /// The library shipped in the binary. An `Err` is a bug in `intents.toml`,
    /// which a unit test rules out; it is still a `Result`, because a daemon
    /// does not panic.
    pub fn builtin() -> Result<&'static IntentLibrary> {
        BUILTIN
            .as_ref()
            .map_err(|e| Error::Internal(format!("the built-in intent library does not load: {e}")))
    }

    /// Parses and validates a library file.
    pub fn from_toml_str(src: &str) -> Result<Self> {
        let file: LibraryFile = toml::from_str(src)
            .map_err(|e| Error::Config(format!("intent library: invalid TOML: {e}")))?;
        Self::build(file.intent)
    }

    /// This library with `overrides` on top. A key names an intent or an
    /// alias; a key that names nothing defines a new intent.
    pub fn merged(&self, overrides: &BTreeMap<String, IntentOverride>) -> Result<Self> {
        // INTENT: the library below stays shared and unchanged, so the layered
        // one starts from a copy of its sources.
        let mut sources = self.sources.clone();
        for (key, layer) in overrides {
            let wanted = normalize(key);
            match sources.iter_mut().find(|s| s.answers_to(&wanted)) {
                Some(existing) => layer.apply(existing),
                None => sources.push(layer.define(&wanted)?),
            }
        }
        Self::build(sources)
    }

    /// The intent a name or an alias refers to, or the message listing the
    /// known ones.
    pub fn get(&self, name: &str) -> Result<&Intent> {
        self.find(name).ok_or_else(|| {
            let known: Vec<&str> = self.names().collect();
            Error::Invalid(format!(
                "unknown intent `{}`; known: {} (or one of their aliases)",
                name.trim(),
                known.join(", ")
            ))
        })
    }

    /// The intent a name or an alias refers to.
    pub fn find(&self, name: &str) -> Option<&Intent> {
        let wanted = normalize(name);
        self.intents.iter().find(|i| i.answers_to(&wanted))
    }

    /// Every intent, in library order.
    pub fn iter(&self) -> impl Iterator<Item = &Intent> {
        self.intents.iter()
    }

    /// Every canonical name, in library order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.intents.iter().map(|i| i.name.as_str())
    }

    fn build(sources: Vec<IntentSource>) -> Result<Self> {
        let intents = build::build_all(&sources)?;
        Ok(Self { sources, intents })
    }
}

fn hints_for(hints: &[Hint], lang: Lang) -> &[String] {
    hints
        .iter()
        .find(|hint| hint.lang == lang)
        .map_or(&[], |hint| hint.constructs.as_slice())
}

/// How a name is compared: trimmed, lowercase.
fn normalize(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests;
