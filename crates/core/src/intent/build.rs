//! From written sources to validated intents: names, sentences, hints, legs,
//! and the borrowing of a workflow leg from a locate intent.

use std::collections::BTreeMap;

use super::source::{HintTable, IntentSource, LegSpec};
use super::template::{Hint, InstructionTemplate};
use super::{Intent, IntentKind, Leg, normalize};
use crate::model::Lang;
use crate::{Error, Result};

/// Builds every intent, in the order the sources were written.
///
/// Locate intents first, because a workflow leg borrows from them.
pub(super) fn build_all(sources: &[IntentSource]) -> Result<Vec<Intent>> {
    check_unique_names(sources)?;
    let mut built: Vec<(usize, Intent)> = Vec::with_capacity(sources.len());
    for (at, source) in sources.iter().enumerate() {
        if source.kind == IntentKind::Locate {
            built.push((at, build_intent(source, &[])?));
        }
    }
    let locate: Vec<&Intent> = built.iter().map(|(_, intent)| intent).collect();
    let mut workflows = Vec::new();
    for (at, source) in sources.iter().enumerate() {
        if source.kind == IntentKind::Workflow {
            workflows.push((at, build_intent(source, &locate)?));
        }
    }
    built.extend(workflows);
    built.sort_by_key(|(at, _)| *at);
    Ok(built.into_iter().map(|(_, intent)| intent).collect())
}

fn build_intent(source: &IntentSource, locate: &[&Intent]) -> Result<Intent> {
    let field = format!("intents.{}", source.name);
    let summary = source.summary.trim();
    if summary.is_empty() || summary.contains('\n') {
        return Err(Error::Config(format!(
            "{field}.summary must be one non-empty line"
        )));
    }
    let instruction =
        InstructionTemplate::parse(&format!("{field}.instruction"), &source.instruction)?;
    let hints = parse_hints(&field, &source.hints)?;
    let specs = leg_specs(&field, source)?;
    check_extras(&field, source, specs.len())?;

    let parent = Parent {
        field: &field,
        kind: source.kind,
        instruction: &instruction,
        hints: &hints,
    };
    let mut legs: Vec<Leg> = Vec::with_capacity(specs.len());
    for spec in &specs {
        let leg = build_leg(&parent, spec, locate)?;
        if legs.iter().any(|l| l.label == leg.label) {
            return Err(Error::Config(format!(
                "{field}: two legs are labelled `{}`",
                leg.label
            )));
        }
        legs.push(leg);
    }
    Ok(Intent {
        name: source.name.clone(),
        aliases: source.aliases.clone(),
        summary: summary.to_string(),
        kind: source.kind,
        neutral: instruction.sentence(&[]),
        instruction,
        hints,
        legs,
        pairing: source.pairing,
        duplicates: source.duplicates,
    })
}

/// The legs written, or the one implicit leg of a locate intent.
fn leg_specs(field: &str, source: &IntentSource) -> Result<Vec<LegSpec>> {
    match (source.kind, source.legs.len()) {
        (IntentKind::Locate, 0) => Ok(vec![LegSpec {
            label: source.name.clone(),
            ..LegSpec::default()
        }]),
        (IntentKind::Locate, 1) => Ok(source.legs.clone()),
        (IntentKind::Locate, _) => Err(Error::Config(format!(
            "{field}: a locate intent has one leg; give it `kind = \"workflow\"` to run several"
        ))),
        (IntentKind::Workflow, 0) => Err(Error::Config(format!(
            "{field}: a workflow intent needs its `legs`"
        ))),
        (IntentKind::Workflow, _) => Ok(source.legs.clone()),
    }
}

/// `pairing` needs two legs; `duplicates` reads the hits of several.
fn check_extras(field: &str, source: &IntentSource, legs: usize) -> Result<()> {
    if source.pairing && legs < 2 {
        return Err(Error::Config(format!(
            "{field}: `pairing` matches two legs; this intent has one"
        )));
    }
    if source.duplicates && source.kind != IntentKind::Workflow {
        return Err(Error::Config(format!(
            "{field}: `duplicates` belongs to a workflow intent"
        )));
    }
    Ok(())
}

/// What a leg falls back on when it neither writes nor borrows a field.
struct Parent<'a> {
    field: &'a str,
    kind: IntentKind,
    instruction: &'a InstructionTemplate,
    hints: &'a [Hint],
}

fn build_leg(parent: &Parent<'_>, spec: &LegSpec, locate: &[&Intent]) -> Result<Leg> {
    let field = format!("{}.legs.{}", parent.field, spec.label);
    check_name("leg label", &spec.label)?;
    if !spec.langs.is_empty() && !spec.exclude_langs.is_empty() {
        return Err(Error::Config(format!(
            "{field}: `langs` and `exclude_langs` say the same thing twice; keep one"
        )));
    }
    if spec.k == Some(0) {
        return Err(Error::Config(format!("{field}: `k` must be > 0")));
    }
    let borrowed = borrowed_intent(&field, parent.kind, spec, locate)?;
    let instruction = match (&spec.instruction, borrowed) {
        (Some(own), _) => InstructionTemplate::parse(&format!("{field}.instruction"), own)?,
        (None, Some(intent)) => intent.instruction.clone(),
        (None, None) => parent.instruction.clone(),
    };
    let hints = match borrowed {
        Some(intent) => intent.hints.clone(),
        None => parent.hints.to_vec(),
    };
    let mut leg = Leg {
        label: spec.label.clone(),
        borrows: borrowed.map(|intent| intent.name.clone()),
        instruction,
        hints,
        kinds: spec.kinds.clone(),
        langs: spec.langs.clone(),
        exclude_langs: spec.exclude_langs.clone(),
        mode: spec.mode,
        k: spec.k,
    };
    if let Some(from) = borrowed.and_then(|intent| intent.legs.first()) {
        leg.inherit(from);
    }
    Ok(leg)
}

/// The locate intent a leg's `intent = "…"` names.
fn borrowed_intent<'a>(
    field: &str,
    kind: IntentKind,
    spec: &LegSpec,
    locate: &[&'a Intent],
) -> Result<Option<&'a Intent>> {
    let Some(name) = &spec.intent else {
        return Ok(None);
    };
    if kind == IntentKind::Locate {
        return Err(Error::Config(format!(
            "{field}: only a workflow leg borrows another intent"
        )));
    }
    let wanted = normalize(name);
    let found = locate.iter().find(|intent| intent.answers_to(&wanted));
    let Some(intent) = found else {
        let known: Vec<&str> = locate.iter().map(|i| i.name.as_str()).collect();
        return Err(Error::Config(format!(
            "{field}: `intent = \"{name}\"` names no locate intent; known: {}",
            known.join(", ")
        )));
    };
    Ok(Some(*intent))
}

/// Keys are canonical language names; each construct is one short phrase.
fn parse_hints(field: &str, table: &HintTable) -> Result<Vec<Hint>> {
    let mut out = Vec::with_capacity(table.len());
    for (key, constructs) in table {
        let at = format!("{field}.hints.{key}");
        let lang = hint_lang(&at, key)?;
        for construct in constructs {
            check_construct(&at, construct)?;
        }
        let constructs = constructs.iter().map(|c| c.trim().to_string()).collect();
        out.push(Hint { lang, constructs });
    }
    Ok(out)
}

fn hint_lang(at: &str, key: &str) -> Result<Lang> {
    let lang = Lang::parse(key).filter(|l| l.as_str() == key && *l != Lang::Other);
    lang.ok_or_else(|| {
        let names: Vec<&str> = Lang::ALL
            .iter()
            .filter(|l| **l != Lang::Other)
            .map(Lang::as_str)
            .collect();
        Error::Config(format!(
            "{at}: not a language name; one of {}",
            names.join(", ")
        ))
    })
}

fn check_construct(at: &str, construct: &str) -> Result<()> {
    let text = construct.trim();
    if text.is_empty() || text.contains('\n') || text.ends_with('.') {
        return Err(Error::Config(format!(
            "{at}: `{construct}`: a construct is a short phrase on one line, with no closing \
             period"
        )));
    }
    Ok(())
}

fn check_unique_names(sources: &[IntentSource]) -> Result<()> {
    let mut owner: BTreeMap<&str, &str> = BTreeMap::new();
    for source in sources {
        check_name("intent name", &source.name)?;
        for word in std::iter::once(&source.name).chain(&source.aliases) {
            check_name(&format!("alias of `{}`", source.name), word)?;
            if let Some(first) = owner.insert(word, &source.name) {
                return Err(Error::Config(format!(
                    "`{word}` names both `{first}` and `{}`; a name or alias belongs to one intent",
                    source.name
                )));
            }
        }
    }
    Ok(())
}

/// `[a-z][a-z0-9-]*`, at most 32 characters: it ends up on a command line and
/// in an MCP enum.
fn check_name(what: &str, raw: &str) -> Result<()> {
    let starts_with_letter = raw.starts_with(|c: char| c.is_ascii_lowercase());
    let allowed = raw
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if starts_with_letter && allowed && raw.len() <= 32 {
        return Ok(());
    }
    Err(Error::Config(format!(
        "{what} `{raw}`: lowercase letters, digits and `-`, starting with a letter, at most 32 \
         characters"
    )))
}
