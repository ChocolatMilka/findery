//! Item boundaries: where a top-level item starts, where it ends, and which
//! line it sits on.
//!
//! INTENT: the boundary rules are lexical, not syntactic — a brace counts only
//! outside a comment, a string or a raw literal ([`crate::scanner`]), and a
//! body-less item ends at its `;` — or, in Go and Kotlin, at the newline their
//! lexers turn into one. That is enough for every brace-delimited language in
//! the registry, which is why there is one splitter and not eight.

use crate::header::{decorator_end, split_trivia};
use crate::scanner::{Lexicon, Statements, is_comment_at, skip_opaque};

/// Byte span of one top-level item, leading trivia included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    /// Offset of the first byte of the item, leading trivia included.
    pub start: usize,
    /// Offset just past the last byte of the item.
    pub end: usize,
}

impl Span {
    pub fn slice<'a>(&self, src: &'a str) -> &'a str {
        &src[self.start..self.end]
    }
}

/// The item the splitter is reading, from its first byte to whatever ends it.
#[derive(Default)]
struct Open {
    /// Its first non-blank byte, once one was seen.
    start: Option<usize>,
    /// It holds code: a comment or an annotation alone is trivia waiting for
    /// its item, and a newline does not end trivia.
    has_code: bool,
    /// Its last token is one a statement can end on.
    can_end: bool,
    /// A `{` was seen, so a `}` back at depth zero may close it.
    saw_brace: bool,
}

impl Open {
    /// Ends the item at `end` and starts reading the next one.
    fn close(&mut self, end: usize, items: &mut Vec<Span>) {
        if let Some(start) = self.start {
            items.push(Span { start, end });
        }
        *self = Self::default();
    }

    /// Records a token of code; `can_end` says whether a statement may end on it.
    fn code(&mut self, can_end: bool) {
        self.has_code = true;
        self.can_end = can_end;
    }

    /// Whether the newline at `i` ends this item, as the Go and Kotlin lexers
    /// would insert a `;` there.
    fn ends_at_newline(&self, bytes: &[u8], i: usize, end: usize, lex: &Lexicon) -> bool {
        lex.ends_statements_at_newline()
            && self.has_code
            && self.can_end
            && !continues_on_next_line(bytes, i + 1, end, lex)
    }
}

/// How deep the splitter sits in each kind of bracket.
#[derive(Default)]
struct Depth {
    brace: usize,
    paren: usize,
    bracket: usize,
}

impl Depth {
    fn is_top(&self) -> bool {
        self.brace == 0 && self.paren == 0 && self.bracket == 0
    }
}

/// Splits a source range into top-level items.
///
/// An item runs from the first non-whitespace byte after the previous item —
/// which naturally absorbs its doc comments and attributes — up to the closing
/// brace of its body, or to the `;` that terminates a body-less item — or, for
/// a language whose [`Statements`] are newline-ended, to the newline after a
/// token a statement can end on.
pub fn split_items(src: &str, range: Span, lex: &Lexicon) -> Vec<Span> {
    let bytes = &src.as_bytes()[..range.end];
    let mut items = Vec::new();
    let mut open = Open::default();
    let mut depth = Depth::default();
    let mut i = range.start;

    while i < range.end {
        let b = bytes[i];
        // Marking the start BEFORE skipping opaque spans is what keeps
        // `/// docs` attached to the item they document.
        if open.start.is_none() && !b.is_ascii_whitespace() {
            open.start = Some(i);
        }
        if b == b'\n' && depth.is_top() && open.ends_at_newline(bytes, i, range.end, lex) {
            open.close(i, &mut items);
            i += 1;
            continue;
        }
        if let Some(next) = skip_opaque(bytes, i, lex) {
            if !is_comment_at(bytes, i, lex) {
                open.code(true);
            }
            i = next.max(i + 1);
            continue;
        }
        // An annotation alone on its line is the item's trivia, not a
        // statement of its own: `@Test` above `fun works()`.
        if b == b'@' && lex.decorators && lex.ends_statements_at_newline() && depth.is_top() {
            i = decorator_end(bytes, i, lex);
            continue;
        }
        if !b.is_ascii_whitespace() {
            open.code(ends_a_statement(bytes, i));
        }
        match b {
            b'{' => {
                depth.brace += 1;
                open.saw_brace = true;
            }
            b'}' => {
                depth.brace = depth.brace.saturating_sub(1);
                // A brace back at depth zero closes the item — unless a `(`/`[`
                // is still open, as in `Lazy::new(|| { … });`, or the statement
                // runs on after it: `import { a } from 'x'`, `<const N: usize
                // = { 3 }>`.
                if depth.is_top() && open.saw_brace && !continues_after_brace(bytes, i + 1) {
                    let end = if bytes.get(i + 1) == Some(&b';') {
                        i + 2
                    } else {
                        i + 1
                    };
                    open.close(end, &mut items);
                    i = end;
                    continue;
                }
            }
            b'(' => depth.paren += 1,
            b')' => depth.paren = depth.paren.saturating_sub(1),
            b'[' => depth.bracket += 1,
            b']' => depth.bracket = depth.bracket.saturating_sub(1),
            b';' if depth.is_top() => open.close(i + 1, &mut items),
            _ => {}
        }
        i += 1;
    }
    // Trailing text with no terminator: a truncated file, trailing comments.
    open.close(range.end, &mut items);
    items
}

/// Whether the statement goes on after a `}` that closed at depth zero.
///
/// INTENT: a `}` ends an item in Rust, but in JavaScript and PHP a brace is
/// often part of an expression — the bound name of a destructuring `import`, a
/// class or object literal — and closing there cuts the statement in two.
fn continues_after_brace(bytes: &[u8], mut i: usize) -> bool {
    const RUNS_ON: &[&str] = &[
        ">",
        ",",
        "=",
        ")",
        "]",
        ".",
        "from",
        "else",
        "while",
        "catch",
        "finally",
        "in",
        "of",
        "instanceof",
        "as",
        "satisfies",
    ];
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    RUNS_ON.iter().any(|token| opens_on(&bytes[i..], token))
}

/// Whether the token ending at `i` is one a statement can end on, so that the
/// newline after it ends the statement.
///
/// INTENT: Go's semicolon-insertion rule, which Kotlin's newline rule agrees
/// with at the top of a file: an identifier, a literal, a closing bracket,
/// `++`/`--`, and — for Kotlin — a type's closing `>`, `?` and `!!`, or a
/// `.*` import. Anything else is an operator waiting for its right side.
fn ends_a_statement(bytes: &[u8], i: usize) -> bool {
    let prev = i.checked_sub(1).and_then(|p| bytes.get(p)).copied();
    match bytes[i] {
        c if c.is_ascii_alphanumeric() || c == b'_' => true,
        b')' | b']' | b'}' | b'?' | b'!' => true,
        b'>' => prev != Some(b'-'),
        b'+' | b'-' => prev == Some(bytes[i]),
        b'*' => prev == Some(b'.'),
        _ => false,
    }
}

/// Whether the line that opens at `i` continues the statement above it:
/// `.map { … }` under a call, `: Base()` under a class header, `private set`
/// under a property, `@Inject constructor(…)` under a class name.
fn continues_on_next_line(bytes: &[u8], mut i: usize, end: usize, lex: &Lexicon) -> bool {
    let Statements::Newline {
        continuations,
        modifiers,
        guarded,
    } = &lex.statements
    else {
        return false;
    };
    let bytes = &bytes[..end];
    let mut behind_a_prefix = false;
    loop {
        while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        let rest = &bytes[i..];
        if lex.decorators && rest.first() == Some(&b'@') {
            i = decorator_end(bytes, i, lex);
        } else if let Some(word) = modifiers.iter().find(|m| opens_on(rest, m)) {
            i += word.len();
        } else {
            break;
        }
        behind_a_prefix = true;
    }
    let rest = &bytes[i..];
    continuations.iter().any(|c| opens_on(rest, c))
        || (behind_a_prefix && guarded.iter().any(|g| opens_on(rest, g)))
}

/// Whether `rest` opens on `token` — on the whole word, when `token` is a word.
fn opens_on(rest: &[u8], token: &str) -> bool {
    let token = token.as_bytes();
    let is_word = token.first().is_some_and(u8::is_ascii_alphabetic);
    let glued = rest
        .get(token.len())
        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_');
    rest.starts_with(token) && !(is_word && glued)
}

/// Returns the span between the `{` that opens `item`'s body and its matching
/// `}`, both exclusive, or `None` when the item has no body.
///
/// INTENT: the body opens on the first `{` past the item's trivia that sits
/// outside every `(…)` and `[…]`. The braces of `@SuppressWarnings({"a"})`, of
/// a decorator's `({ selector })` or of a default lambda `onClick: () -> Unit
/// = { … }` are not a body — and recursing into one of them as if it were
/// silently drops the real one, members and all.
pub fn body_span(src: &str, item: Span, lex: &Lexicon) -> Option<Span> {
    let bytes = &src.as_bytes()[..item.end];
    let (trivia, _) = split_trivia(item.slice(src), lex);
    let open = header_brace(bytes, item.start + trivia.len(), lex)?;
    let close = matching_brace(bytes, open, lex)?;
    Some(Span {
        start: open + 1,
        end: close,
    })
}

/// The offset of the `{` that ends the header starting at `i`.
///
/// INTENT: a type argument may hold a brace too — `impl Foo<{ N }>` — so `<…>`
/// nests like `(…)`, the `>` of `->` and `=>` aside. A `<` that never closes
/// finds no body, and the item is windowed whole: a misread costs the split,
/// never the text.
fn header_brace(bytes: &[u8], mut i: usize, lex: &Lexicon) -> Option<usize> {
    let mut nesting = 0usize;
    while i < bytes.len() {
        if let Some(next) = skip_opaque(bytes, i, lex) {
            i = next.max(i + 1);
            continue;
        }
        let arrow = i > 0 && matches!(bytes[i - 1], b'-' | b'=');
        match bytes[i] {
            b'(' | b'[' | b'<' => nesting += 1,
            b')' | b']' => nesting = nesting.saturating_sub(1),
            b'>' if !arrow => nesting = nesting.saturating_sub(1),
            b'{' if nesting == 0 => return Some(i),
            // A `}` with nothing open is a stray brace, not a body boundary:
            // it cannot close a body that never opened.
            b'}' if nesting == 0 => return None,
            _ => {}
        }
        i += 1;
    }
    None
}

/// The offset of the `}` matching the `{` at `open`.
fn matching_brace(bytes: &[u8], open: usize, lex: &Lexicon) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        if let Some(next) = skip_opaque(bytes, i, lex) {
            i = next.max(i + 1);
            continue;
        }
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Precomputed line index, so line numbers cost a binary search instead of a
/// rescan per chunk.
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(src: &str) -> Self {
        let mut starts = vec![0usize];
        starts.extend(
            src.bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self { starts }
    }

    /// 1-indexed line number containing `offset`.
    pub fn line_of(&self, offset: usize) -> u32 {
        match self.starts.binary_search(&offset) {
            Ok(i) => (i + 1) as u32,
            Err(i) => i.max(1) as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST: &Lexicon = &Lexicon::RUST;

    fn split(src: &str) -> Vec<Span> {
        split_items(
            src,
            Span {
                start: 0,
                end: src.len(),
            },
            RUST,
        )
    }

    #[test]
    fn braces_inside_strings_do_not_close_an_item() {
        // `\u{7d}` is a closing brace, spelled as an escape so this file's
        // own braces balance for `check_file_size.sh`, which finds the end of
        // a test module by counting them.
        let src = "fn a() { let s = \"\u{7d}\"; let t = '\u{7d}'; }\nfn b() {}\n";
        let items = split(src);
        assert_eq!(items.len(), 2, "got {items:?}");
        assert!(items[0].slice(src).starts_with("fn a"));
        assert!(items[1].slice(src).starts_with("fn b"));
    }

    #[test]
    fn a_non_ascii_char_literal_does_not_fuse_two_items() {
        // The literal is 4 bytes: read as 3, its closing quote becomes the
        // opening quote of the next one and the `{` of `fn b` stops counting.
        let src = "fn a(c: char) -> bool { matches!(c, 'é'|'\u{7b}') }\nfn b() {}\n";
        let items = split(src);
        assert_eq!(items.len(), 2, "got {items:?}");
        assert!(items[1].slice(src).starts_with("fn b"));
    }

    #[test]
    fn an_escaped_quote_in_a_literal_does_not_fuse_two_items() {
        let src = "fn a(c: char) -> bool { matches!(c, '\\''|'\u{7b}') }\nfn b() {}\n";
        let items = split(src);
        assert_eq!(items.len(), 2, "got {items:?}");
        assert!(items[1].slice(src).starts_with("fn b"));
    }

    #[test]
    fn raw_strings_and_nested_block_comments_are_opaque() {
        let src =
            "fn a() { let r = r#\"\u{7d} not a brace \"#; /* /* \u{7d}\u{7d} */ */ }\nfn b() {}\n";
        assert_eq!(split(src).len(), 2);
    }

    #[test]
    fn lifetimes_are_not_char_literals() {
        let src = "fn a<'x>(v: &'x str) -> &'x str { v }\nfn b() {}\n";
        assert_eq!(split(src).len(), 2);
    }

    #[test]
    fn semicolon_items_and_attributes_ride_along() {
        let src = "use std::io;\n#[derive(Debug)]\npub struct S { a: u8 }\n";
        let items = split(src);
        assert_eq!(items.len(), 2);
        assert!(items[1].slice(src).starts_with("#[derive"));
    }

    #[test]
    fn a_body_inside_a_call_does_not_end_the_item() {
        // The `}` closes the closure, not the statement: the item runs to `;`.
        let src = "static X: Lazy<u8> = Lazy::new(|| { 1 });\nfn b() {}\n";
        let items = split(src);
        assert_eq!(items.len(), 2, "got {items:?}");
        assert!(items[0].slice(src).trim_end().ends_with(");"));
        assert!(items[1].slice(src).starts_with("fn b"));
    }

    #[test]
    fn a_const_generic_default_with_braces_stays_in_one_item() {
        let src = "struct A<const N: usize = { 3 }>;\nfn b() {}\n";
        let items = split(src);
        assert_eq!(items.len(), 2, "got {items:?}");
        assert!(items[0].slice(src).starts_with("struct A"));
    }

    #[test]
    fn an_array_of_objects_is_one_statement() {
        let src = "const A = [{ a: 1 }, { b: 2 }];\n";
        let items = split(src);
        assert_eq!(items.len(), 1, "got {items:?}");
    }

    #[test]
    fn a_stray_closing_brace_has_no_body_to_isolate() {
        // A malformed file can put a closing brace before the item's own. It
        // must answer "no body", not subtract past zero.
        let src = "\u{7d}\nfn a() { 1 }\n";
        let item = Span {
            start: 0,
            end: src.len(),
        };
        assert_eq!(body_span(src, item, RUST), None);
    }

    #[test]
    fn body_span_isolates_the_block() {
        let src = "impl S { fn a() {} }";
        let item = Span {
            start: 0,
            end: src.len(),
        };
        let body = body_span(src, item, RUST).expect("has a body");
        assert_eq!(body.slice(src).trim(), "fn a() {}");
    }

    fn split_in<'a>(src: &'a str, lex: &Lexicon) -> Vec<&'a str> {
        split_items(
            src,
            Span {
                start: 0,
                end: src.len(),
            },
            lex,
        )
        .iter()
        .map(|s| s.slice(src))
        .collect()
    }

    #[test]
    fn a_go_newline_ends_a_statement_where_the_go_lexer_would() {
        let src =
            "package main\n\nimport \"fmt\"\n\nconst Max = 4\n\n// Run runs.\nfunc Run() {\n}\n";
        let items = split_in(src, &Lexicon::GO);
        assert_eq!(
            items,
            vec![
                "package main",
                "import \"fmt\"",
                "const Max = 4",
                "// Run runs.\nfunc Run() {\n}"
            ]
        );
    }

    #[test]
    fn a_newline_inside_brackets_or_after_an_operator_does_not_end_a_statement() {
        let src = "import (\n\t\"fmt\"\n\t\"os\"\n)\nvar x = 1 +\n\t2\nvar y = 3\n";
        let items = split_in(src, &Lexicon::GO);
        assert_eq!(items.len(), 3, "got {items:?}");
        assert!(items[0].ends_with(')'));
        assert!(items[1].ends_with('2'));
    }

    #[test]
    fn an_annotation_or_a_comment_alone_on_its_line_waits_for_its_item() {
        let src = "@Test\n// why\n@Suppress(\"x\")\nfun works() {\n}\nval after = 1\n";
        let items = split_in(src, &Lexicon::KOTLIN);
        assert_eq!(items.len(), 2, "got {items:?}");
        assert!(items[0].starts_with("@Test") && items[0].ends_with('\u{7d}'));
        assert_eq!(items[1], "val after = 1");
    }

    #[test]
    fn a_kotlin_line_opening_on_a_continuation_continues_the_statement() {
        let src = "class A(x: Int)\n    : Base(x)\nval y = listOf(1)\n    .map { it }\nimport a.b.*\nfun f() = 1\n";
        let items = split_in(src, &Lexicon::KOTLIN);
        assert_eq!(items.len(), 4, "got {items:?}");
        assert!(items[0].contains(": Base"));
        assert!(items[1].contains(".map"));
        assert_eq!(items[2], "import a.b.*");
    }

    #[test]
    fn a_semicolon_language_is_not_cut_at_a_newline() {
        // Java keeps the semicolon rule: a field split across lines is one item.
        let src = "int x =\n    4;\nint y = 5;\n";
        let items = split_in(src, &Lexicon::JAVA);
        assert_eq!(items, vec!["int x =\n    4;", "int y = 5;"]);
    }

    fn whole(src: &str) -> Span {
        Span {
            start: 0,
            end: src.len(),
        }
    }

    #[test]
    fn a_brace_inside_an_annotation_or_a_parameter_list_is_not_the_body() {
        // Recursing into the first `{` of these items isolated the annotation's
        // argument — or the default lambda — and dropped the real body whole.
        let cases: &[(&str, &Lexicon, &str)] = &[
            (
                "@SuppressWarnings({\"unchecked\"})\npublic class Client {\n    void a() {}\n}",
                &Lexicon::JAVA,
                "void a() {}",
            ),
            (
                "class Button(val onClick: () -> Unit = { log(\"x\") }) {\n    fun a() {}\n}",
                &Lexicon::KOTLIN,
                "fun a() {}",
            ),
            (
                "@Component({\n  selector: 'x'\n})\nexport class W {\n  a() {}\n}",
                &Lexicon::JAVASCRIPT,
                "a() {}",
            ),
            ("impl Foo<{ N }> {\n    fn a() {}\n}", RUST, "fn a() {}"),
        ];
        for (src, lex, expected) in cases {
            let body = body_span(src, whole(src), lex).expect("has a body");
            assert_eq!(body.slice(src).trim(), *expected, "{src}");
        }
    }

    #[test]
    fn a_kotlin_annotated_constructor_under_its_class_continues_the_class() {
        let src =
            "class Foo\n    @Inject constructor(val a: A) {\n    fun x() = 1\n}\nval after = 1\n";
        let items = split_in(src, &Lexicon::KOTLIN);
        assert_eq!(items.len(), 2, "got {items:?}");
        assert!(items[0].contains("@Inject constructor"));
        // Bare, a `constructor(…)` line opens a secondary constructor of its own.
        let src = "val a = 1\nconstructor(x: Int) : this()\n";
        assert_eq!(split_in(src, &Lexicon::KOTLIN).len(), 2);
    }

    #[test]
    fn a_kotlin_accessor_behind_a_modifier_continues_its_property() {
        let src = "var count = 0\n    private set\nval after = 1\n";
        let items = split_in(src, &Lexicon::KOTLIN);
        assert_eq!(
            items,
            vec!["var count = 0\n    private set", "val after = 1"]
        );
    }

    #[test]
    fn a_kotlin_else_on_its_own_line_continues_the_if() {
        let src = "val x = if (a) 1\nelse 2\nfun y() = 2\n";
        let items = split_in(src, &Lexicon::KOTLIN);
        assert_eq!(items, vec!["val x = if (a) 1\nelse 2", "fun y() = 2"]);
    }

    #[test]
    fn line_index_is_one_based() {
        let idx = LineIndex::new("a\nb\nc");
        assert_eq!(idx.line_of(0), 1);
        assert_eq!(idx.line_of(2), 2);
        assert_eq!(idx.line_of(4), 3);
    }
}
