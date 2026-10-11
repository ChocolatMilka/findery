//! Item boundaries: where a top-level item starts, where it ends, and which
//! line it sits on.
//!
//! INTENT: the boundary rules are lexical, not syntactic — a brace counts only
//! outside a comment, a string or a raw literal ([`crate::scanner`]), and a
//! body-less item ends at its `;`. That is enough for every brace-delimited
//! language in the registry, which is why there is one splitter and not four.

use crate::scanner::{Lexicon, skip_opaque};

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

/// Splits a source range into top-level items.
///
/// An item runs from the first non-whitespace byte after the previous item —
/// which naturally absorbs its doc comments and attributes — up to the closing
/// brace of its body, or to the `;` that terminates a body-less item.
pub fn split_items(src: &str, range: Span, lex: &Lexicon) -> Vec<Span> {
    let bytes = src.as_bytes();
    let mut items = Vec::new();
    let mut i = range.start;
    let mut item_start = None::<usize>;
    let (mut brace, mut paren, mut bracket) = (0usize, 0usize, 0usize);
    let mut saw_brace = false;

    while i < range.end {
        let b = bytes[i];
        // An item begins at its first non-whitespace byte, which is usually a
        // doc comment or an attribute. Marking the start BEFORE skipping opaque
        // spans is what keeps `/// docs` attached to the item they document.
        if item_start.is_none() && !b.is_ascii_whitespace() {
            item_start = Some(i);
        }
        if let Some(next) = skip_opaque(bytes, i, lex) {
            i = next.min(range.end);
            continue;
        }
        match b {
            b'{' => {
                brace += 1;
                saw_brace = true;
            }
            b'}' => {
                brace = brace.saturating_sub(1);
                // A brace that closes at depth zero closes the item — unless
                // something says the statement runs on: a `(…)`/`[…]` still
                // open, as in `static X: Lazy<u8> = Lazy::new(|| { … });`, or
                // a continuation after the brace, as in `import { a } from 'x'`
                // or `const { a } = x`. What is INSIDE a brace is not a body
                // either: `struct A<const N: usize = { 3 }>;`.
                let closes_item = brace == 0
                    && paren == 0
                    && bracket == 0
                    && saw_brace
                    && item_start.is_some()
                    && !continues_after_brace(bytes, i + 1, range.end);
                if closes_item {
                    let mut end = i + 1;
                    if bytes.get(end) == Some(&b';') {
                        end += 1;
                    }
                    items.push(Span {
                        start: item_start.take().unwrap_or(end),
                        end,
                    });
                    saw_brace = false;
                    i = end;
                    continue;
                }
            }
            b'(' => paren += 1,
            b')' => paren = paren.saturating_sub(1),
            b'[' => bracket += 1,
            b']' => bracket = bracket.saturating_sub(1),
            b';' if brace == 0 && paren == 0 && bracket == 0 => {
                if let Some(start) = item_start.take() {
                    items.push(Span { start, end: i + 1 });
                }
                saw_brace = false;
            }
            _ => {}
        }
        i += 1;
    }
    // Trailing text with no terminator (truncated file, trailing comments).
    if let Some(start) = item_start
        && start < range.end
    {
        items.push(Span {
            start,
            end: range.end,
        });
    }
    items
}

/// Whether the statement goes on after a `}` that closed at depth zero.
///
/// INTENT: a `}` ends an item in Rust, but in JavaScript and PHP a brace is
/// often part of an expression — the bound name of a destructuring `import`, a
/// class or object literal — and closing there cuts the statement in two.
fn continues_after_brace(bytes: &[u8], mut i: usize, end: usize) -> bool {
    while i < end && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    match bytes.get(i) {
        Some(b'>' | b',' | b'=' | b')' | b']' | b'.') => true,
        Some(_) => {
            let mut j = i;
            while bytes.get(j).is_some_and(|c| c.is_ascii_alphabetic()) {
                j += 1;
            }
            let word = &bytes[i..j];
            matches!(
                word,
                b"from"
                    | b"else"
                    | b"while"
                    | b"catch"
                    | b"finally"
                    | b"in"
                    | b"of"
                    | b"instanceof"
                    | b"as"
                    | b"satisfies"
            )
        }
        None => false,
    }
}

/// Returns the span between the first structural `{` of `item` and its matching
/// `}` (exclusive), or `None` when the item has no body.
pub fn body_span(src: &str, item: Span, lex: &Lexicon) -> Option<Span> {
    let bytes = src.as_bytes();
    let mut i = item.start;
    let mut open = None::<usize>;
    let mut depth = 0usize;
    while i < item.end {
        if let Some(next) = skip_opaque(bytes, i, lex) {
            i = next.min(item.end);
            continue;
        }
        match bytes[i] {
            b'{' => {
                depth += 1;
                if depth == 1 {
                    open = Some(i + 1);
                }
            }
            b'}' => {
                // A `}` with nothing open is a stray brace in the header, not a
                // body boundary: it cannot close a body that never opened.
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return open.map(|start| Span { start, end: i });
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
        // A malformed file can put a `}` before the item's own brace. It must
        // answer "no body", not subtract past zero.
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

    #[test]
    fn line_index_is_one_based() {
        let idx = LineIndex::new("a\nb\nc");
        assert_eq!(idx.line_of(0), 1);
        assert_eq!(idx.line_of(2), 2);
        assert_eq!(idx.line_of(4), 3);
    }
}
