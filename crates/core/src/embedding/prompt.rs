//! Prompt formats: the default instruction, and the two templates that turn a
//! query or a document into the text a model is actually handed. The named
//! instructions are intents, in [`crate::intent`].
//!
//! Instruction-aware models (Qwen3-Embedding, Harrier, qwen3.7-text-embedding)
//! are **asymmetric**: the QUERY carries an instruction, the DOCUMENT never
//! does. Qwen measures a 1-5% retrieval gain from a task-tailored instruction,
//! which is free — it costs one prefix on a string that was going to be
//! embedded anyway. Prefixing documents too would cost accuracy AND double the
//! blast radius of a prompt change, since every stored vector would have to be
//! recomputed.
//!
//! Both sides are data on the model profile, because the wire format is a fact
//! about the weights, not about findery:
//!
//! ```text
//! query_template    = "Instruct: {instruction}\nQuery: {query}"   (the default)
//! document_template = "{text}"                                    (the default)
//! query_template    = "search_query: {query}"                     (nomic-style)
//! document_template = "search_document: {text}"                   (nomic-style)
//! ```
//!
//! INTENT: this module is the single source of truth for those formats
//! (CONV-012). Nobody concatenates `"Instruct: "` anywhere else; an embedder
//! holds a parsed template and asks this module to render it.

use std::borrow::Cow;

use crate::{Error, Result};

/// Default task description, used when a caller names nothing.
///
/// Language-neutral on purpose: a daemon indexes Rust, Axle, PHP, TypeScript and
/// prose side by side, and an instruction naming one language skews every
/// query towards it. Changing it moves query vectors only, never a stored one.
pub const DEFAULT_QUERY_INSTRUCTION: &str = "Given a developer question about a codebase, retrieve the source code or \
     documentation passages that answer it.";

/// The legacy intent names, kept here so `findery_core::embedding::prompt::QueryTask`
/// still names them. Their sentences live in the intent library.
pub use crate::intent::QueryTask;

/// The query template every profile gets unless it names another — the
/// Qwen3 / Harrier format.
pub const DEFAULT_QUERY_TEMPLATE: &str = "Instruct: {instruction}\nQuery: {query}";

/// The document template every profile gets unless it names another: the
/// document as it is.
pub const DEFAULT_DOCUMENT_TEMPLATE: &str = "{text}";

/// One piece of a compiled template.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Piece {
    Literal(String),
    Query,
    Instruction,
    Text,
}

/// Which side a template renders, and therefore which slots it may name.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Query,
    Document,
}

/// Compiles a template into pieces, refusing unknown placeholders.
///
/// INTENT: a `{word}` that is not a slot is almost always a typo
/// (`{qurey}`), and a typo left literal would be embedded verbatim into every
/// query — accuracy lost with no error anywhere. A brace that does not enclose
/// a bare lowercase word stays literal text.
fn compile(field: &str, raw: &str, side: Side) -> Result<Vec<Piece>> {
    let mut pieces = Vec::new();
    let mut literal = String::new();
    let mut rest = raw;
    while let Some(open) = rest.find('{') {
        literal.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let word_len = after
            .bytes()
            .take_while(|b| b.is_ascii_lowercase() || *b == b'_')
            .count();
        if word_len == 0 || !after[word_len..].starts_with('}') {
            literal.push('{');
            rest = after;
            continue;
        }
        let word = &after[..word_len];
        let slot = match (side, word) {
            (Side::Query, "query") => Piece::Query,
            // `{task}` is how the Qwen3 and Harrier model cards spell it.
            (Side::Query, "instruction" | "task") => Piece::Instruction,
            (Side::Document, "text") => Piece::Text,
            _ => {
                let known = match side {
                    Side::Query => "`{query}` and `{instruction}`",
                    Side::Document => "`{text}`",
                };
                return Err(Error::Config(format!(
                    "{field}: unknown placeholder `{{{word}}}` in `{raw}`; the slots are {known}"
                )));
            }
        };
        if !literal.is_empty() {
            pieces.push(Piece::Literal(std::mem::take(&mut literal)));
        }
        pieces.push(slot);
        rest = &after[word_len + 1..];
    }
    literal.push_str(rest);
    if !literal.is_empty() {
        pieces.push(Piece::Literal(literal));
    }
    Ok(pieces)
}

/// How a profile's model wants a query: where the question goes, and where
/// the instruction does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryTemplate {
    pieces: Vec<Piece>,
}

impl QueryTemplate {
    /// Parses a query template. It must contain `{query}`; `{instruction}`
    /// (or its synonym `{task}`) is optional — a model with a fixed prefix has
    /// no slot for one.
    pub fn parse(raw: &str) -> Result<Self> {
        let pieces = compile("query_template", raw, Side::Query)?;
        if !pieces.contains(&Piece::Query) {
            return Err(Error::Config(format!(
                "query_template `{raw}` has no `{{query}}`: the question would never \
                 reach the model"
            )));
        }
        Ok(Self { pieces })
    }

    fn has_instruction(&self) -> bool {
        self.pieces.contains(&Piece::Instruction)
    }
}

impl Default for QueryTemplate {
    /// [`DEFAULT_QUERY_TEMPLATE`], compiled.
    fn default() -> Self {
        Self {
            pieces: vec![
                Piece::Literal("Instruct: ".into()),
                Piece::Instruction,
                Piece::Literal("\nQuery: ".into()),
                Piece::Query,
            ],
        }
    }
}

/// How a profile's model wants a document. The default is the identity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocumentTemplate {
    /// `None` is the identity template.
    pieces: Option<Vec<Piece>>,
}

impl DocumentTemplate {
    /// Parses a document template, which must contain `{text}`.
    pub fn parse(raw: &str) -> Result<Self> {
        if Self::is_identity_source(raw) {
            return Ok(Self::default());
        }
        let pieces = compile("document_template", raw, Side::Document)?;
        if !pieces.contains(&Piece::Text) {
            return Err(Error::Config(format!(
                "document_template `{raw}` has no `{{text}}`: every document would embed \
                 as the same string"
            )));
        }
        Ok(Self {
            pieces: Some(pieces),
        })
    }

    /// Whether this template leaves documents as they are.
    pub fn is_identity(&self) -> bool {
        self.pieces.is_none()
    }

    /// Whether `raw` spells the identity template. Shared with the recipe
    /// builder, so "renders documents unchanged" and "adds no fingerprint to
    /// the recipe" are one decision.
    pub fn is_identity_source(raw: &str) -> bool {
        raw.trim() == DEFAULT_DOCUMENT_TEMPLATE
    }
}

/// How one query is to be prompted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryPrompt<'a> {
    /// The profile's configured `query_instruction`.
    Default,
    /// This instruction, in the profile's template.
    Instruct(&'a str),
    /// The query exactly as written: no instruction, no template.
    Raw,
}

/// Renders a query the way `prompt` says, `default_instruction` being what
/// [`QueryPrompt::Default`] means to the embedder calling it.
pub fn render_query(
    template: &QueryTemplate,
    default_instruction: &str,
    query: &str,
    prompt: QueryPrompt<'_>,
) -> String {
    match prompt {
        QueryPrompt::Raw => query.to_string(),
        QueryPrompt::Default => format_query(template, default_instruction, query),
        QueryPrompt::Instruct(instruction) => format_query(template, instruction, query),
    }
}

/// Renders the query side of the asymmetric pair.
///
/// An empty instruction degrades to the raw query when the template has an
/// instruction slot: `Instruct: \nQuery: …` is a malformed prompt, the bare
/// question is not. A template without the slot is always rendered.
pub fn format_query(template: &QueryTemplate, instruction: &str, query: &str) -> String {
    let task = instruction.trim();
    if task.is_empty() && template.has_instruction() {
        return query.to_string();
    }
    let mut out = String::with_capacity(query.len() + task.len() + 32);
    for piece in &template.pieces {
        match piece {
            Piece::Literal(text) => out.push_str(text),
            Piece::Query => out.push_str(query),
            Piece::Instruction => out.push_str(task),
            Piece::Text => {}
        }
    }
    out
}

/// Renders the document side: what the model is handed for one
/// `Chunk::embed_input()`. Borrowed, at no cost, for the identity template.
pub fn format_document<'a>(template: &DocumentTemplate, text: &'a str) -> Cow<'a, str> {
    let Some(pieces) = &template.pieces else {
        return Cow::Borrowed(text);
    };
    let mut out = String::with_capacity(text.len() + 32);
    for piece in pieces {
        match piece {
            Piece::Literal(literal) => out.push_str(literal),
            Piece::Text => out.push_str(text),
            Piece::Query | Piece::Instruction => {}
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `format_query` rendered before templates existed, kept verbatim:
    /// the default template must reproduce it byte for byte.
    fn legacy_format_query(instruction: &str, query: &str) -> String {
        let task = instruction.trim();
        if task.is_empty() {
            return query.to_string();
        }
        format!("Instruct: {task}\nQuery: {query}")
    }

    #[test]
    fn the_default_query_template_renders_todays_bytes() {
        let parsed = QueryTemplate::parse(DEFAULT_QUERY_TEMPLATE).expect("default parses");
        assert_eq!(parsed, QueryTemplate::default());
        for (instruction, query) in [
            (DEFAULT_QUERY_INSTRUCTION, "where is retry handled"),
            ("  padded task  ", "q"),
            ("", "raw"),
            ("   ", "abc"),
            ("t", "a query with {query} and {instruction} inside"),
            ("multi\nline", "line one\nline two"),
        ] {
            assert_eq!(
                format_query(&parsed, instruction, query),
                legacy_format_query(instruction, query),
                "{instruction:?} / {query:?}"
            );
        }
    }

    #[test]
    fn query_carries_the_instruction() {
        let q = format_query(
            &QueryTemplate::default(),
            DEFAULT_QUERY_INSTRUCTION,
            "where is retry handled",
        );
        assert!(q.starts_with("Instruct: "));
        assert!(q.contains("\nQuery: where is retry handled"));
    }

    #[test]
    fn empty_instruction_degrades_to_the_raw_query() {
        assert_eq!(format_query(&QueryTemplate::default(), "  ", "abc"), "abc");
    }

    #[test]
    fn a_template_without_an_instruction_slot_is_always_rendered() {
        let nomic = QueryTemplate::parse("search_query: {query}").expect("parses");
        assert_eq!(format_query(&nomic, "", "retry"), "search_query: retry");
        assert_eq!(
            format_query(&nomic, "ignored task", "retry"),
            "search_query: retry"
        );
    }

    #[test]
    fn the_model_card_spelling_task_is_accepted() {
        let card = QueryTemplate::parse("Instruct: {task}\nQuery: {query}").expect("parses");
        assert_eq!(card, QueryTemplate::default());
    }

    #[test]
    fn a_query_template_without_the_query_is_refused() {
        let err = QueryTemplate::parse("Instruct: {instruction}").expect_err("no query");
        assert_eq!(err.code(), "config");
        assert!(err.to_string().contains("{query}"), "{err}");
    }

    #[test]
    fn a_misspelled_placeholder_is_refused_not_embedded() {
        assert!(QueryTemplate::parse("Instruct: {instrution}\nQuery: {query}").is_err());
        assert!(QueryTemplate::parse("{text} {query}").is_err());
        assert!(DocumentTemplate::parse("{query}: {text}").is_err());
    }

    #[test]
    fn braces_that_are_not_placeholders_stay_literal() {
        let t = QueryTemplate::parse("{ json: {} } {Query} {query}").expect("parses");
        assert_eq!(format_query(&t, "", "x"), "{ json: {} } {Query} x");
        let d = DocumentTemplate::parse("{{text}}").expect("parses");
        assert_eq!(format_document(&d, "body"), "{body}");
    }

    #[test]
    fn documents_stay_untouched_by_default() {
        let identity = DocumentTemplate::default();
        assert!(identity.is_identity());
        assert!(matches!(
            format_document(&identity, "fn main() {}"),
            Cow::Borrowed("fn main() {}")
        ));
        let spelled = DocumentTemplate::parse("  {text}\n").expect("parses");
        assert!(spelled.is_identity());
    }

    #[test]
    fn a_document_template_wraps_the_embed_input() {
        let nomic = DocumentTemplate::parse("search_document: {text}").expect("parses");
        assert!(!nomic.is_identity());
        assert_eq!(
            format_document(&nomic, "path: a.rs\n---\nfn a() {}"),
            "search_document: path: a.rs\n---\nfn a() {}"
        );
    }

    #[test]
    fn a_document_template_without_the_text_is_refused() {
        let err = DocumentTemplate::parse("search_document:").expect_err("no text");
        assert_eq!(err.code(), "config");
        assert!(err.to_string().contains("{text}"), "{err}");
    }

    #[test]
    fn a_raw_query_bypasses_every_template_and_the_others_fill_it() {
        let nomic = QueryTemplate::parse("search_query: {query}").expect("parses");
        assert_eq!(render_query(&nomic, "d", "q", QueryPrompt::Raw), "q");
        let qwen = QueryTemplate::default();
        assert_eq!(
            render_query(&qwen, "d", "q", QueryPrompt::Default),
            "Instruct: d\nQuery: q"
        );
        assert_eq!(
            render_query(&qwen, "d", "q", QueryPrompt::Instruct("t")),
            "Instruct: t\nQuery: q"
        );
    }

    #[test]
    fn the_default_instruction_names_no_language() {
        assert!(!DEFAULT_QUERY_INSTRUCTION.contains("Rust"));
        assert!(DEFAULT_QUERY_INSTRUCTION.starts_with("Given "));
    }
}
