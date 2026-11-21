//! Item boundaries: where a top-level item starts, where it ends, and which
//! line it sits on.
//!
//! INTENT: the boundary rules are lexical, not syntactic — a brace counts only
//! outside a comment, a string or a raw literal ([`crate::scanner`]), and a
//! body-less item ends at its `;` — or, in Go and Kotlin, at the newline their
//! lexers turn into one. That is enough for every brace-delimited language in
//! the registry, which is why there is one splitter and not eight.

use crate::header::{BRACES, balanced_end, decorator_end, opens_on, split_trivia};
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
    /// The text of the span in `src`.
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
    /// Offset just past its last token of code.
    code_end: usize,
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

    /// Records a token of code ending at `end`; `can_end` says whether a
    /// statement may end on it.
    fn code(&mut self, can_end: bool, end: usize) {
        self.has_code = true;
        self.can_end = can_end;
        self.code_end = end;
    }

    /// Whether the newline at `i` ends this item, as the Go and Kotlin lexers
    /// would insert a `;` there.
    fn ends_at_newline(&self, bytes: &[u8], i: usize, end: usize, lex: &Lexicon) -> bool {
        let class_header = || self.is_class_header(&bytes[..i]);
        lex.ends_statements_at_newline()
            && self.has_code
            && self.can_end
            && !ends_on_dangling_word(bytes, self.code_end, lex)
            && !continues_on_next_line(bytes, i + 1, end, lex, class_header)
    }

    /// Whether this item, read up to the end of `bytes`, is a class header
    /// still waiting for its body: what a primary constructor may continue.
    fn is_class_header(&self, bytes: &[u8]) -> bool {
        let Some(start) = self.start else {
            return false;
        };
        let mut words = bytes[start..].split(|b| !is_word_byte(*b));
        !self.saw_brace && words.any(|word| word == b"class")
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
            let next = next.max(i + 1);
            if !is_comment_at(bytes, i, lex) {
                open.code(true, next);
            }
            i = next;
            continue;
        }
        if b == b'@' && lex.decorators && lex.ends_statements_at_newline() && depth.is_top() {
            i = skip_annotation(bytes, i, lex, &mut open);
            continue;
        }
        if !b.is_ascii_whitespace() {
            open.code(ends_a_statement(bytes, i, lex), i + 1);
        }
        match b {
            b'{' => {
                depth.brace += 1;
                open.saw_brace = true;
            }
            b'}' => {
                depth.brace = depth.brace.saturating_sub(1);
                if let Some(end) = brace_closes_item(bytes, i, &open, &depth, lex) {
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

/// Skips the annotation whose `@` sits at `i`, as trivia of the item it
/// annotates: `@Test` alone above `fun works()` is no statement of its own.
///
/// INTENT: an annotation annotates what FOLLOWS it, so no statement ends on
/// one — `class A` / `@Inject` / `constructor(…)` is one class. A label glued
/// to a word (`this@A`, `return@forEach`) is no annotation and changes nothing.
fn skip_annotation(bytes: &[u8], i: usize, lex: &Lexicon, open: &mut Open) -> usize {
    let glued = i > 0 && is_word_byte(bytes[i - 1]);
    if open.has_code && !glued {
        open.can_end = false;
    }
    decorator_end(bytes, i, lex)
}

/// The end of the item a `}` at `i` closes, if it closes one.
///
/// INTENT: a brace back at depth zero closes the item — unless a `(`/`[` is
/// still open, as in `Lazy::new(|| { … });`, or the statement runs on after it:
/// `import { a } from 'x'`, `<const N: usize = { 3 }>`. In Go and Kotlin a `}`
/// closes nothing: it may close a type literal (`interface{} {`,
/// `struct{}{…}`), and the newline after it ends the statement anyway.
fn brace_closes_item(
    bytes: &[u8],
    i: usize,
    open: &Open,
    depth: &Depth,
    lex: &Lexicon,
) -> Option<usize> {
    let closes = !lex.ends_statements_at_newline()
        && depth.is_top()
        && open.saw_brace
        && !continues_after_brace(bytes, i + 1);
    match (closes, bytes.get(i + 1)) {
        (false, _) => None,
        (true, Some(b';')) => Some(i + 2),
        (true, _) => Some(i + 1),
    }
}

/// Whether `b` can be part of an identifier.
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
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
/// `++`/`--`. Kotlin adds a type's closing `>`, `?` and `!!`, and a `.*`
/// import — in Go those are operators. Anything else is an operator waiting
/// for its right side.
fn ends_a_statement(bytes: &[u8], i: usize, lex: &Lexicon) -> bool {
    let prev = i.checked_sub(1).and_then(|p| bytes.get(p)).copied();
    let type_suffixes = matches!(
        lex.statements,
        Statements::Newline {
            type_suffixes: true,
            ..
        }
    );
    match bytes[i] {
        c if is_word_byte(c) => true,
        b')' | b']' | b'}' => true,
        b'?' | b'!' => type_suffixes,
        b'>' => type_suffixes && prev != Some(b'-'),
        b'+' | b'-' => prev == Some(bytes[i]),
        b'*' => type_suffixes && prev == Some(b'.'),
        _ => false,
    }
}

/// Whether the code ends, at `end`, on a word no statement ends on: a
/// modifier waiting for its declaration (`class A private` over
/// `constructor(…)`), an `else` waiting for its branch. A word after a `.` is
/// part of a name: `package okio.internal` ends where it stands.
fn ends_on_dangling_word(bytes: &[u8], end: usize, lex: &Lexicon) -> bool {
    let Statements::Newline {
        modifiers,
        dangling,
        ..
    } = &lex.statements
    else {
        return false;
    };
    let start = bytes[..end]
        .iter()
        .rposition(|b| !is_word_byte(*b))
        .map_or(0, |p| p + 1);
    let qualified = start > 0 && bytes[start - 1] == b'.';
    match (qualified, std::str::from_utf8(&bytes[start..end])) {
        (false, Ok(word)) => modifiers.contains(&word) || dangling.contains(&word),
        _ => false,
    }
}

/// Whether the line that opens at `i` continues the statement above it:
/// `.map { … }` under a call, `: Base()` under a class header, `private set`
/// under a property, `@Inject constructor(…)` under a class name — and only
/// there: `class_header` says the statement is one, since a `constructor` under
/// anything else opens a secondary constructor of its own. A comment between
/// the two lines is read past, and is no prefix.
fn continues_on_next_line(
    bytes: &[u8],
    mut i: usize,
    end: usize,
    lex: &Lexicon,
    class_header: impl Fn() -> bool,
) -> bool {
    let Statements::Newline {
        continuations,
        modifiers,
        guarded,
        ..
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
        if is_comment_at(bytes, i, lex) {
            i = skip_opaque(bytes, i, lex).map_or(end, |next| next.max(i + 1));
            continue;
        }
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
    let guarded_continues =
        || behind_a_prefix && guarded.iter().any(|g| opens_on(rest, g)) && class_header();
    continuations.iter().any(|c| opens_on(rest, c)) || guarded_continues()
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
    let past_close = balanced_end(bytes, open, lex, BRACES)?;
    Some(Span {
        start: open + 1,
        end: past_close - 1,
    })
}

/// The offset of the `{` that ends the header starting at `i`.
///
/// INTENT: a type argument may hold a brace too — `impl Foo<{ N }>` — so `<…>`
/// nests like `(…)`, the `>` of `->` and `=>` aside. Only a `<` glued to a
/// name opens type arguments: the `<` of `x < 1` or of `1 << 4` compares, and
/// counting it hid the body of every class it sat in. A `<` that never closes
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
            b'(' | b'[' => nesting += 1,
            b'<' if opens_type_arguments(bytes, i) => nesting += 1,
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

/// Whether the `<` at `i` follows a name (`Vec<`, `impl<`) or a turbofish
/// (`::<`), rather than an operand it compares.
fn opens_type_arguments(bytes: &[u8], i: usize) -> bool {
    let word_start = bytes[..i]
        .iter()
        .rposition(|b| !is_word_byte(*b))
        .map_or(0, |p| p + 1);
    match bytes[..i].last() {
        Some(b':') => true,
        Some(_) if word_start < i => !bytes[word_start].is_ascii_digit(),
        _ => false,
    }
}

/// Precomputed line index, so line numbers cost a binary search instead of a
/// rescan per chunk.
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    /// Indexes the line starts of `src`.
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
        let src = "fn a() { let s = \"}\"; let t = '}'; }\nfn b() {}\n";
        let items = split(src);
        assert_eq!(items.len(), 2, "got {items:?}");
        assert!(items[0].slice(src).starts_with("fn a"));
        assert!(items[1].slice(src).starts_with("fn b"));
    }

    #[test]
    fn a_non_ascii_char_literal_does_not_fuse_two_items() {
        // The literal is 4 bytes: read as 3, its closing quote becomes the
        // opening quote of the next one and the `{` of `fn b` stops counting.
        let src = "fn a(c: char) -> bool { matches!(c, 'é'|'{') }\nfn b() {}\n";
        let items = split(src);
        assert_eq!(items.len(), 2, "got {items:?}");
        assert!(items[1].slice(src).starts_with("fn b"));
    }

    #[test]
    fn an_escaped_quote_in_a_literal_does_not_fuse_two_items() {
        let src = "fn a(c: char) -> bool { matches!(c, '\\''|'{') }\nfn b() {}\n";
        let items = split(src);
        assert_eq!(items.len(), 2, "got {items:?}");
        assert!(items[1].slice(src).starts_with("fn b"));
    }

    #[test]
    fn raw_strings_and_nested_block_comments_are_opaque() {
        let src = "fn a() { let r = r#\"} not a brace \"#; /* /* }} */ */ }\nfn b() {}\n";
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
        let src = "}\nfn a() { 1 }\n";
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
        assert!(items[0].starts_with("@Test") && items[0].ends_with('}'));
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
    fn a_comparison_in_a_parameter_list_does_not_hide_the_body() {
        let cases: &[(&str, &Lexicon)] = &[
            (
                "class A(val ok: Boolean = x < 1) {\n    fun a() {}\n}",
                &Lexicon::KOTLIN,
            ),
            ("impl Foo for [u8; 1 << 4] {\n    fn a() {}\n}", RUST),
        ];
        for (src, lex) in cases {
            let body = body_span(src, whole(src), lex).expect("has a body");
            assert_eq!(
                body.slice(src).trim(),
                src.lines().nth(1).expect("member").trim()
            );
        }
    }

    #[test]
    fn a_brace_ends_no_newline_statement_before_its_line_does() {
        let src = "class A {\n} // end\nval b = 1\n";
        assert_eq!(
            split_in(src, &Lexicon::KOTLIN),
            vec!["class A {\n} // end", "val b = 1"]
        );
        let src = "func Get() interface{} {\n\treturn nil\n}\nfunc b() {}\n";
        assert_eq!(split_in(src, &Lexicon::GO).len(), 2);
    }

    #[test]
    fn line_index_is_one_based() {
        let idx = LineIndex::new("a\nb\nc");
        assert_eq!(idx.line_of(0), 1);
        assert_eq!(idx.line_of(2), 2);
        assert_eq!(idx.line_of(4), 3);
    }
}
