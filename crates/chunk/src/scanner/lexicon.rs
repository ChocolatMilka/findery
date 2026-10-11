//! How each language spells its trivia, as data.
//!
//! INTENT: the brace-delimited family shares one item grammar and differs only
//! in its comments, its strings and its literals. One table per language is
//! what keeps ONE scanner behind them, and one place to fix when a literal is
//! read wrong.

//! A minimal lexical scanner: what a language's punctuation can be trusted for.
//!
//! INTENT: chunking needs item boundaries, not a syntax tree. A full parser
//! (tree-sitter, syn) would drag a C toolchain or a proc-macro dependency into
//! the image for information we throw away immediately. What we do need is to
//! never mistake a brace inside a string, a comment or a raw literal for a
//! structural brace — that is exactly what this scanner guarantees.

/// How one language spells its comments, its strings and its literals.
///
/// INTENT: the brace-delimited family (Rust, JavaScript, PHP, JSON) shares one
/// item grammar and differs only in its trivia. Describing that trivia as data
/// keeps ONE scanner instead of one per language — and one place to fix when a
/// literal is read wrong.
pub struct Lexicon {
    /// Openers of a comment that ends at the newline: `["//"]`, `["//", "#"]`.
    pub line_comments: &'static [&'static str],
    /// The block-comment pair, when the language has one.
    pub block_comment: Option<BlockComment>,
    /// Quote bytes that open an escaped string: `"`, `'`, `` ` ``.
    pub strings: &'static [u8],

    /// `'a'` is a character literal rather than a string (Rust).
    pub char_literals: bool,
    /// `r"…"`, `r#"…"#`, `b"…"` (Rust).
    pub raw_strings: bool,
    /// `/…/flags` is a regular expression when it follows an operator (JS).
    pub regex_literals: bool,
    /// `<<<EOT … EOT;` (PHP).
    pub heredocs: bool,
    /// `#[…]` is an attribute riding along with the item it annotates.
    pub attributes: bool,
    /// `@Decorator(…)` rides along with the item it annotates (TypeScript).
    pub decorators: bool,
}

/// A block comment's delimiters. Rust nests them, C-like languages do not.
pub struct BlockComment {
    /// The opening delimiter.
    pub open: &'static str,
    /// The closing delimiter.
    pub close: &'static str,
    /// Whether an opening delimiter inside the comment opens another one.
    pub nested: bool,
}

impl Lexicon {
    /// Rust: nested block comments, raw strings, char literals.
    pub const RUST: Self = Self {
        line_comments: &["//"],
        block_comment: Some(BlockComment {
            open: "/*",
            close: "*/",
            nested: true,
        }),
        strings: b"\"",
        char_literals: true,
        raw_strings: true,
        regex_literals: false,
        heredocs: false,
        attributes: true,
        decorators: false,
    };

    /// JavaScript and TypeScript: template literals, regular expression
    /// literals, `//` and `/* */`.
    pub const JAVASCRIPT: Self = Self {
        line_comments: &["//"],
        block_comment: Some(BlockComment {
            open: "/*",
            close: "*/",
            nested: false,
        }),
        strings: b"\"'`",
        char_literals: false,
        raw_strings: false,
        regex_literals: true,
        heredocs: false,
        attributes: false,
        decorators: true,
    };

    /// PHP: `#` and `//` line comments, heredocs and nowdocs, `#[attributes]`.
    pub const PHP: Self = Self {
        line_comments: &["//", "#"],
        block_comment: Some(BlockComment {
            open: "/*",
            close: "*/",
            nested: false,
        }),
        strings: b"\"'",
        char_literals: false,
        raw_strings: false,
        regex_literals: false,
        heredocs: true,
        attributes: true,
        decorators: false,
    };

    /// JSON, plus the `//` and `/* */` of its jsonc dialect.
    pub const JSON: Self = Self {
        line_comments: &["//"],
        block_comment: Some(BlockComment {
            open: "/*",
            close: "*/",
            nested: false,
        }),
        strings: b"\"",
        char_literals: false,
        raw_strings: false,
        regex_literals: false,
        heredocs: false,
        attributes: false,
        decorators: false,
    };
}
