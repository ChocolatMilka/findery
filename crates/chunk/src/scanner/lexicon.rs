//! How each language spells its trivia, as data.
//!
//! INTENT: the brace-delimited family (Rust, JavaScript, PHP, JSON, Axle, Java,
//! Kotlin, Go) shares one item grammar and differs only in its comments, its
//! strings and its literals. One table per language is what keeps ONE scanner
//! behind them, and one place to fix when a literal is read wrong.

/// How one language spells its comments, its strings and its literals.
pub struct Lexicon {
    /// Openers of a comment that ends at the newline: `["//"]`, `["//", "#"]`.
    pub line_comments: &'static [&'static str],
    /// The block-comment pair, when the language has one.
    pub block_comment: Option<BlockComment>,
    /// Quote bytes that open an escaped string: `"`, `'`, `` ` ``.
    pub strings: &'static [u8],
    /// An escaped string cannot span a line (Java, Kotlin, Go): an unterminated
    /// one ends at the newline instead of swallowing the rest of the file.
    pub single_line_strings: bool,
    /// Quote bytes that open a verbatim string — no escape, may span lines:
    /// Go's `` `…` ``.
    pub verbatim_strings: &'static [u8],
    /// `"""…"""` is one literal (Java text blocks, Kotlin raw strings).
    pub text_blocks: TextBlocks,
    /// `${…}` inside a `"` string or a text block is code that may itself hold
    /// strings and braces (Kotlin), so it is followed to its matching `}`.
    pub templates: bool,
    /// `'a'` is a character literal rather than a string (Rust, Java, Go…).
    pub char_literals: bool,
    /// `r"…"`, `r#"…"#`, `b"…"` (Rust).
    pub raw_strings: bool,
    /// `/…/flags` is a regular expression when it follows an operator (JS).
    pub regex_literals: bool,
    /// `<<<EOT … EOT;` (PHP).
    pub heredocs: bool,
    /// `#[…]` is an attribute riding along with the item it annotates.
    pub attributes: bool,
    /// `@Decorator(…)` rides along with the item it annotates (TypeScript,
    /// Java, Kotlin, Axle).
    pub decorators: bool,
    /// The `@word`s that are syntax, not annotations: Java's `@interface`.
    pub at_keywords: &'static [&'static str],
    /// What ends a statement that has no body.
    pub statements: Statements,
}

/// A block comment's delimiters. Rust and Kotlin nest them, C-like languages
/// do not.
pub struct BlockComment {
    /// The opening delimiter.
    pub open: &'static str,
    /// The closing delimiter.
    pub close: &'static str,
    /// Whether an opening delimiter inside the comment opens another one.
    pub nested: bool,
}

/// Whether, and how, `"""` opens a literal of its own.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TextBlocks {
    /// `"""` is an empty string followed by a quote.
    No,
    /// A Java text block: `\` escapes, so `\"""` does not close it.
    Escaped,
    /// A Kotlin raw string: no escape at all.
    Raw,
}

/// What ends a body-less statement.
///
/// INTENT: Go and Kotlin insert their semicolons in the LEXER — a newline ends
/// a statement when the token before it can end one. That is a lexical rule,
/// which is why it lives with the comments and the strings and not in a
/// keyword table.
pub enum Statements {
    /// Only a `;` (or a closing brace) ends an item.
    Semicolon,
    /// A newline at depth zero ends an item too, unless the token before it is
    /// an operator or the next line opens on one of `continuations`.
    Newline {
        /// What a line may open on and still continue the statement above it:
        /// `.` for a call chain, `:` for a supertype list, `where` for bounds.
        continuations: &'static [&'static str],
        /// Words a continuation may sit behind, as an annotation may:
        /// `private set`, `@JvmName("x") get`.
        modifiers: &'static [&'static str],
        /// What continues the statement only behind an annotation or a
        /// modifier: `@Inject constructor(…)` under `class Foo`. Bare, it opens
        /// a statement of its own — a secondary `constructor(…)`.
        guarded: &'static [&'static str],
    },
}

/// `/* */` that does not nest: the C comment family.
const C_COMMENTS: Option<BlockComment> = Some(BlockComment {
    open: "/*",
    close: "*/",
    nested: false,
});

/// `/* */` that nests: `/* /* */ still a comment */`.
const NESTED_COMMENTS: Option<BlockComment> = Some(BlockComment {
    open: "/*",
    close: "*/",
    nested: true,
});

impl Lexicon {
    /// What every language below starts from: `//`, a flat `/* */`, `"…"`
    /// strings, `;`-ended statements, and nothing else.
    const C_LIKE: Self = Self {
        line_comments: &["//"],
        block_comment: C_COMMENTS,
        strings: b"\"",
        single_line_strings: false,
        verbatim_strings: b"",
        text_blocks: TextBlocks::No,
        templates: false,
        char_literals: false,
        raw_strings: false,
        regex_literals: false,
        heredocs: false,
        attributes: false,
        decorators: false,
        at_keywords: &[],
        statements: Statements::Semicolon,
    };

    /// Rust: nested block comments, raw strings, char literals, `#[…]`.
    pub const RUST: Self = Self {
        block_comment: NESTED_COMMENTS,
        char_literals: true,
        raw_strings: true,
        attributes: true,
        ..Self::C_LIKE
    };

    /// JavaScript and TypeScript: three quotes, regular expression literals,
    /// `@Decorator(…)`.
    pub const JAVASCRIPT: Self = Self {
        strings: b"\"'`",
        regex_literals: true,
        decorators: true,
        ..Self::C_LIKE
    };

    /// PHP: `#` line comments too, heredocs and nowdocs, `#[attributes]`.
    pub const PHP: Self = Self {
        line_comments: &["//", "#"],
        strings: b"\"'",
        heredocs: true,
        attributes: true,
        ..Self::C_LIKE
    };

    /// JSON, plus the `//` and `/* */` of its jsonc dialect.
    pub const JSON: Self = Self::C_LIKE;

    /// Axle: `"…"` and `` `…` `` strings, `'c'` characters, and `@link(…)`
    /// annotations riding along with the item below.
    pub const AXLE: Self = Self {
        strings: b"\"`",
        char_literals: true,
        decorators: true,
        ..Self::C_LIKE
    };

    /// Java: `"…"` on one line, `"""…"""` text blocks, `'c'` characters, and
    /// `@Annotation(…)` riding along with the declaration below.
    pub const JAVA: Self = Self {
        single_line_strings: true,
        text_blocks: TextBlocks::Escaped,
        char_literals: true,
        decorators: true,
        at_keywords: &["interface"],
        ..Self::C_LIKE
    };

    /// Kotlin: nested block comments, `"…"` with `${…}` templates, `"""…"""`
    /// raw strings, `` `quoted identifiers` ``, and statements ended by a
    /// newline.
    pub const KOTLIN: Self = Self {
        block_comment: NESTED_COMMENTS,
        // A backticked identifier — `` fun `it's done`() `` — is opaque on one
        // line, so an apostrophe inside it opens nothing.
        strings: b"\"`",
        single_line_strings: true,
        text_blocks: TextBlocks::Raw,
        templates: true,
        char_literals: true,
        decorators: true,
        statements: Statements::Newline {
            continuations: &[
                ".", "?", ":", "=", "{", "&", "|", "where", "get", "set", "else",
            ],
            modifiers: &["private", "protected", "internal", "public"],
            guarded: &["constructor"],
        },
        ..Self::C_LIKE
    };

    /// Go: `"…"` on one line, `` `…` `` verbatim strings, `'r'` runes, and
    /// statements ended by a newline exactly where the Go lexer inserts its
    /// semicolons.
    pub const GO: Self = Self {
        single_line_strings: true,
        verbatim_strings: b"`",
        char_literals: true,
        statements: Statements::Newline {
            continuations: &[],
            modifiers: &[],
            guarded: &[],
        },
        ..Self::C_LIKE
    };

    /// Whether a newline can end a statement, as in Go and Kotlin.
    pub fn ends_statements_at_newline(&self) -> bool {
        matches!(self.statements, Statements::Newline { .. })
    }
}
