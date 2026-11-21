//! The library as WRITTEN — what `intents.toml` and an `[intents.<name>]` table
//! deserialize into — and how an override lands on the entry it names.
//!
//! INTENT: kept apart from the validated [`super::Intent`] because borrowing
//! (`intent = "usage"` in a leg) is resolved after every layer has landed, so an
//! override of `usage` reaches the `review` leg that borrows it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::IntentKind;
use crate::model::{ChunkKind, Lang, SearchMode};
use crate::{Error, Result};

/// Per-language constructs, keyed by language name.
pub type HintTable = BTreeMap<String, Vec<String>>;

/// The whole library file.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LibraryFile {
    pub(crate) intent: Vec<IntentSource>,
}

/// One intent, as written.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IntentSource {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) aliases: Vec<String>,
    pub(crate) summary: String,
    pub(crate) kind: IntentKind,
    pub(crate) instruction: String,
    #[serde(default)]
    pub(crate) pairing: bool,
    #[serde(default)]
    pub(crate) duplicates: bool,
    #[serde(default)]
    pub(crate) hints: HintTable,
    #[serde(default)]
    pub(crate) legs: Vec<LegSpec>,
}

impl IntentSource {
    /// An empty locate intent called `name`, for an override to fill.
    fn blank(name: &str) -> Self {
        Self {
            name: name.to_string(),
            aliases: Vec::new(),
            summary: String::new(),
            kind: IntentKind::Locate,
            instruction: String::new(),
            pairing: false,
            duplicates: false,
            hints: HintTable::new(),
            legs: Vec::new(),
        }
    }

    pub(crate) fn answers_to(&self, wanted: &str) -> bool {
        self.name == wanted || self.aliases.iter().any(|a| a == wanted)
    }
}

/// One leg of a workflow, as written. Only `label` is required: a leg that
/// borrows a locate intent inherits its sentence, hints and restrictions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegSpec {
    /// Names the leg in a plan and in a response: `callers`, `tests`.
    pub label: String,
    /// The locate intent this leg borrows from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    /// This leg's own sentence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<ChunkKind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub langs: Vec<Lang>,
    /// Languages left out — `markdown` for a leg that wants code.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_langs: Vec<Lang>,
    /// Unless the request names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<SearchMode>,
    /// Unless the request names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub k: Option<usize>,
}

/// `[intents.<name>]` in the daemon config or a project manifest.
///
/// On an existing intent (name or alias) each field set replaces that field;
/// `hints` merge language by language, and an empty list removes one. On a new
/// name it defines an intent, which then needs `summary` and `instruction`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IntentOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<IntentKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pairing: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicates: Option<bool>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub hints: HintTable,
    /// Replaces the whole plan: merging two ordered lists by position would
    /// pair labels nobody wrote together.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legs: Option<Vec<LegSpec>>,
}

impl IntentOverride {
    /// Lands this override on an existing intent.
    pub(crate) fn apply(&self, base: &mut IntentSource) {
        replace(&mut base.aliases, &self.aliases);
        replace(&mut base.summary, &self.summary);
        replace(&mut base.kind, &self.kind);
        replace(&mut base.instruction, &self.instruction);
        replace(&mut base.pairing, &self.pairing);
        replace(&mut base.duplicates, &self.duplicates);
        replace(&mut base.legs, &self.legs);
        for (lang, constructs) in &self.hints {
            if constructs.is_empty() {
                base.hints.remove(lang);
            } else {
                base.hints.insert(lang.clone(), constructs.clone());
            }
        }
    }

    /// Defines a new intent from this override alone.
    pub(crate) fn define(&self, name: &str) -> Result<IntentSource> {
        let mut missing = Vec::new();
        if self.summary.is_none() {
            missing.push("summary");
        }
        if self.instruction.is_none() {
            missing.push("instruction");
        }
        if !missing.is_empty() {
            return Err(Error::Config(format!(
                "intents.{name}: no intent is called `{name}`, so this table defines one and \
                 must set {}",
                missing.join(" and ")
            )));
        }
        let mut source = IntentSource::blank(name);
        self.apply(&mut source);
        Ok(source)
    }
}

fn replace<T: Clone>(field: &mut T, with: &Option<T>) {
    if let Some(value) = with {
        field.clone_from(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage() -> IntentSource {
        IntentSource {
            aliases: vec!["callers".into()],
            summary: "s".into(),
            instruction: "Given an API, retrieve its callers.".into(),
            hints: HintTable::from([
                ("rust".to_string(), vec!["call sites".to_string()]),
                ("php".to_string(), vec!["`new X`".to_string()]),
            ]),
            ..IntentSource::blank("usage")
        }
    }

    #[test]
    fn an_override_replaces_what_it_sets_and_merges_hints_by_language() {
        let mut base = usage();
        let layer: IntentOverride = toml::from_str(
            "instruction = \"Given an API, retrieve its importers.\"\n\
             [hints]\nrust = [\"`use` paths\"]\nphp = []\ngo = [\"receivers\"]\n",
        )
        .expect("parses");
        layer.apply(&mut base);
        assert_eq!(base.instruction, "Given an API, retrieve its importers.");
        assert_eq!(base.aliases, vec!["callers"], "an unset field is kept");
        assert_eq!(base.hints["rust"], vec!["`use` paths"]);
        assert!(!base.hints.contains_key("php"), "an empty list removes it");
        assert_eq!(base.hints["go"], vec!["receivers"]);
    }

    #[test]
    fn a_new_intent_needs_a_summary_and_an_instruction() {
        let half = IntentOverride {
            instruction: Some("Given x, retrieve y.".into()),
            ..IntentOverride::default()
        };
        let err = half.define("migration").expect_err("no summary");
        assert!(err.to_string().contains("summary"), "{err}");

        let whole = IntentOverride {
            summary: Some("Schema migrations.".into()),
            ..half
        };
        let source = whole.define("migration").expect("defined");
        assert_eq!(source.kind, IntentKind::Locate);
    }

    #[test]
    fn an_override_with_an_unknown_key_is_refused() {
        for src in [
            "instructions = \"typo\"",
            "[[legs]]\nlabel = \"a\"\nlang = [\"rust\"]",
        ] {
            let err = toml::from_str::<IntentOverride>(src).expect_err(src);
            assert!(err.to_string().contains("unknown field"), "{err}");
        }
    }
}
