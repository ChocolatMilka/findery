//! An instruction sentence and its rendering for the languages a search touches.
//!
//! `{codebase}` renders "a Rust and PHP codebase", or "a codebase" when no
//! language is known; `{project}` renders the same with "project". Hints are
//! appended as `; Rust: a, b and c` clauses while the whole fits the cap.

use crate::model::Lang;
use crate::{Error, Result};

/// The words a placeholder may name, and the noun each renders.
const SLOTS: &[(&str, &str)] = &[("codebase", "codebase"), ("project", "project")];

#[derive(Clone, Debug, PartialEq, Eq)]
enum Piece {
    Literal(String),
    /// "a Rust codebase", "an Axle project", "a codebase".
    Noun(&'static str),
}

/// A parsed instruction sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstructionTemplate {
    source: String,
    pieces: Vec<Piece>,
}

impl InstructionTemplate {
    /// Parses one sentence. `field` names where it came from, for the message.
    ///
    /// Refused: an empty sentence, a line break, and a `{word}` that is not a
    /// slot. Other braces stay literal, so `{ }` can be written as it is.
    pub fn parse(field: &str, raw: &str) -> Result<Self> {
        let source = raw.trim();
        if source.is_empty() {
            return Err(Error::Config(format!(
                "{field}: an instruction cannot be empty"
            )));
        }
        if source.contains(['\n', '\r']) {
            return Err(Error::Config(format!(
                "{field}: an instruction is one sentence on one line, got `{source}`"
            )));
        }
        Ok(Self {
            source: source.to_string(),
            pieces: compile(field, source)?,
        })
    }

    /// The sentence as written.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The sentence naming `langs`, without hints.
    pub fn sentence(&self, langs: &[Lang]) -> String {
        let named: Vec<Lang> = dedup(langs)
            .into_iter()
            .filter(|l| names_a_codebase(*l))
            .collect();
        let mut out = String::with_capacity(self.source.len() + 32);
        for piece in &self.pieces {
            match piece {
                Piece::Literal(text) => out.push_str(text),
                Piece::Noun(noun) => out.push_str(&noun_phrase(&named, noun)),
            }
        }
        out
    }

    /// The sentence for `langs` (priority order), then one hint clause per
    /// language that has one, while the whole stays within `max_chars`. The
    /// sentence itself is never cut; `0` appends no hint.
    pub fn render(&self, langs: &[Lang], hints: &[Hint], max_chars: usize) -> String {
        let langs = dedup(langs);
        let sentence = self.sentence(&langs);
        let (mut out, close) = match sentence.strip_suffix('.') {
            Some(body) => (body.to_string(), "."),
            None => (sentence.clone(), ""),
        };
        for lang in langs {
            let Some(hint) = hints.iter().find(|h| h.lang == lang) else {
                continue;
            };
            let clause = hint.clause();
            if out.len() + clause.len() + close.len() > max_chars {
                break;
            }
            out.push_str(&clause);
        }
        out.push_str(close);
        out
    }
}

/// The constructs one language uses for one task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hint {
    pub lang: Lang,
    pub constructs: Vec<String>,
}

impl Hint {
    /// `; Rust: call sites, generic bounds and `use` paths`.
    fn clause(&self) -> String {
        format!(
            "; {}: {}",
            display_name(self.lang),
            join_and(&self.constructs)
        )
    }
}

/// Splits the sentence into literals and slots.
fn compile(field: &str, raw: &str) -> Result<Vec<Piece>> {
    let mut pieces = Vec::new();
    let mut literal = String::new();
    let mut rest = raw;
    while let Some(open) = rest.find('{') {
        literal.push_str(&rest[..open]);
        let tail = &rest[open..];
        let Some(word) = placeholder(tail) else {
            literal.push('{');
            rest = &tail[1..];
            continue;
        };
        let noun = slot_noun(word).ok_or_else(|| unknown_slot(field, raw, word))?;
        if !literal.is_empty() {
            pieces.push(Piece::Literal(std::mem::take(&mut literal)));
        }
        pieces.push(Piece::Noun(noun));
        rest = &tail[word.len() + 2..];
    }
    literal.push_str(rest);
    if !literal.is_empty() {
        pieces.push(Piece::Literal(literal));
    }
    Ok(pieces)
}

/// The word of a `{word}` at the start of `text`: lowercase letters and `_`
/// between braces. Anything else is literal text.
fn placeholder(text: &str) -> Option<&str> {
    let after = text.strip_prefix('{')?;
    let len = after
        .bytes()
        .take_while(|b| b.is_ascii_lowercase() || *b == b'_')
        .count();
    let closed = after[len..].starts_with('}');
    (len > 0 && closed).then(|| &after[..len])
}

fn slot_noun(word: &str) -> Option<&'static str> {
    SLOTS
        .iter()
        .find(|(name, _)| *name == word)
        .map(|(_, noun)| *noun)
}

fn unknown_slot(field: &str, raw: &str, word: &str) -> Error {
    Error::Config(format!(
        "{field}: unknown placeholder `{{{word}}}` in `{raw}`; the slots are `{{codebase}}` \
         and `{{project}}`"
    ))
}

/// "a codebase", "a Rust and PHP codebase", "an Axle codebase".
fn noun_phrase(langs: &[Lang], noun: &str) -> String {
    let names: Vec<&str> = langs.iter().map(|l| display_name(*l)).collect();
    if names.is_empty() {
        return format!("a {noun}");
    }
    let list = join_and(&names);
    let article = if list.starts_with(['A', 'E', 'I', 'O', 'U']) {
        "an"
    } else {
        "a"
    };
    format!("{article} {list} {noun}")
}

/// `langs` without repeats and without `Other`, first occurrence kept: the
/// caller's order is the priority order.
fn dedup(langs: &[Lang]) -> Vec<Lang> {
    let mut out: Vec<Lang> = Vec::with_capacity(langs.len());
    for lang in langs {
        if *lang != Lang::Other && !out.contains(lang) {
            out.push(*lang);
        }
    }
    out
}

/// Markup and data formats sit in every repository and say nothing about it,
/// so they never name the codebase — they still get their hint.
fn names_a_codebase(lang: Lang) -> bool {
    !matches!(lang, Lang::Markdown | Lang::Toml | Lang::Json | Lang::Other)
}

/// How a language is spelled in a sentence.
pub(crate) fn display_name(lang: Lang) -> &'static str {
    match lang {
        Lang::Rust => "Rust",
        Lang::Markdown => "Markdown",
        Lang::Toml => "TOML",
        Lang::JavaScript => "JavaScript/TypeScript",
        Lang::Python => "Python",
        Lang::Php => "PHP",
        Lang::Json => "JSON",
        Lang::Axle => "Axle",
        Lang::Java => "Java",
        Lang::Kotlin => "Kotlin",
        Lang::Go => "Go",
        Lang::Other => "other",
    }
}

/// "a", "a and b", "a, b and c".
fn join_and<S: AsRef<str>>(items: &[S]) -> String {
    match items {
        [] => String::new(),
        [one] => one.as_ref().to_string(),
        [rest @ .., last] => {
            let head: Vec<&str> = rest.iter().map(AsRef::as_ref).collect();
            format!("{} and {}", head.join(", "), last.as_ref())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(raw: &str) -> InstructionTemplate {
        InstructionTemplate::parse("test", raw).expect("parses")
    }

    fn hint(lang: Lang, constructs: &[&str]) -> Hint {
        Hint {
            lang,
            constructs: constructs.iter().map(|c| c.to_string()).collect(),
        }
    }

    #[test]
    fn the_codebase_slot_names_the_languages_or_none() {
        let t = template("Given an API of {codebase}, retrieve its callers.");
        assert_eq!(
            t.sentence(&[]),
            "Given an API of a codebase, retrieve its callers."
        );
        assert_eq!(
            t.sentence(&[Lang::Rust, Lang::Php]),
            "Given an API of a Rust and PHP codebase, retrieve its callers."
        );
        assert_eq!(
            t.sentence(&[Lang::Rust, Lang::Php, Lang::Go]),
            "Given an API of a Rust, PHP and Go codebase, retrieve its callers."
        );
    }

    #[test]
    fn a_vowel_takes_an_and_the_project_slot_says_project() {
        let t = template("Given a question about {project}, retrieve the docs.");
        assert_eq!(
            t.sentence(&[Lang::Axle]),
            "Given a question about an Axle project, retrieve the docs."
        );
        assert_eq!(
            t.sentence(&[Lang::Markdown]),
            "Given a question about a project, retrieve the docs."
        );
    }

    #[test]
    fn hints_are_joined_and_follow_the_priority_order_in_one_sentence() {
        let t = template("Given X in {codebase}, retrieve Y.");
        let hints = [
            hint(Lang::Php, &["`new X`"]),
            hint(Lang::Rust, &["call sites", "generic bounds", "`use` paths"]),
        ];
        let out = t.render(&[Lang::Rust, Lang::Php, Lang::Rust], &hints, 1_000);
        assert_eq!(
            out,
            "Given X in a Rust and PHP codebase, retrieve Y; Rust: call sites, generic bounds \
             and `use` paths; PHP: `new X`."
        );
    }

    #[test]
    fn a_language_without_a_hint_is_named_but_adds_no_clause() {
        let t = template("Given X in {codebase}, retrieve Y.");
        let out = t.render(&[Lang::Go], &[hint(Lang::Rust, &["r"])], 1_000);
        assert_eq!(out, "Given X in a Go codebase, retrieve Y.");
    }

    #[test]
    fn markup_gets_its_hint_but_never_names_the_codebase() {
        let t = template("Given X in {codebase}, retrieve Y.");
        let hints = [hint(Lang::Markdown, &["READMEs"])];
        let out = t.render(&[Lang::Markdown, Lang::Other], &hints, 1_000);
        assert_eq!(out, "Given X in a codebase, retrieve Y; Markdown: READMEs.");
    }

    #[test]
    fn the_cap_drops_hints_from_the_lowest_priority_never_the_sentence() {
        let t = template("Given X, retrieve Y.");
        let long = "a".repeat(20);
        let hints = [hint(Lang::Rust, &[&long]), hint(Lang::Php, &["b"])];
        let core = "Given X, retrieve Y.".len();
        // Room for the first clause only: the kept hints are always a prefix of
        // the priority order, even when a later one is shorter.
        let first = "; Rust: ".len() + 20;
        let out = t.render(&[Lang::Rust, Lang::Php], &hints, core + first);
        assert!(out.contains("Rust:") && !out.contains("PHP:"), "{out}");
        assert!(out.len() <= core + first, "{out}");
        assert_eq!(t.render(&[Lang::Rust], &hints, 0), "Given X, retrieve Y.");
    }

    #[test]
    fn braces_that_are_not_a_slot_stay_literal() {
        let t = template("Given `#[cfg(test)]` items and `{ }` blocks, retrieve {Them}.");
        assert_eq!(
            t.sentence(&[Lang::Rust]),
            "Given `#[cfg(test)]` items and `{ }` blocks, retrieve {Them}."
        );
    }

    #[test]
    fn an_unknown_slot_a_line_break_or_nothing_is_refused() {
        for raw in [
            "Given {query}, retrieve it.",
            "Given {langs} code, retrieve it.",
            "Given x,\nretrieve it.",
            "   ",
        ] {
            let err = InstructionTemplate::parse("t", raw).expect_err(raw);
            assert_eq!(err.code(), "config", "{raw}");
        }
    }
}
