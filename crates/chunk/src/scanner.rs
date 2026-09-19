//! A minimal lexical scanner for Rust source.
//!
//! INTENT: chunking needs item boundaries, not a syntax tree. A full parser
//! (tree-sitter, syn) would drag a C toolchain or a proc-macro dependency into
//! the image for information we throw away immediately. What we do need is to
//! never mistake a brace inside a string, a comment or a raw literal for a
//! structural brace — that is exactly what this scanner guarantees.

/// Byte span of one top-level item, leading trivia included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn slice<'a>(&self, src: &'a str) -> &'a str {
        &src[self.start..self.end]
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// Skips a lexical construct whose contents must not be read structurally.
///
/// Returns the offset just past the construct, or `None` when the byte at `i`
/// starts no such construct and the caller should interpret it.
pub fn skip_opaque(src: &[u8], i: usize) -> Option<usize> {
    match src.get(i)? {
        b'/' => match src.get(i + 1) {
            Some(b'/') => Some(skip_line_comment(src, i + 2)),
            Some(b'*') => Some(skip_block_comment(src, i + 2)),
            _ => None,
        },
        b'"' => Some(skip_string(src, i + 1)),
        b'r' | b'b' => skip_raw_or_byte_literal(src, i),
        b'\'' => skip_char_literal(src, i),
        _ => None,
    }
}

fn skip_line_comment(src: &[u8], mut i: usize) -> usize {
    while i < src.len() && src[i] != b'\n' {
        i += 1;
    }
    i
}

/// Block comments nest in Rust, so depth matters.
fn skip_block_comment(src: &[u8], mut i: usize) -> usize {
    let mut depth = 1usize;
    while i < src.len() {
        if src[i] == b'/' && src.get(i + 1) == Some(&b'*') {
            depth += 1;
            i += 2;
        } else if src[i] == b'*' && src.get(i + 1) == Some(&b'/') {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    i
}

fn skip_string(src: &[u8], mut i: usize) -> usize {
    while i < src.len() {
        match src[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    i
}

/// Handles `r"..."`, `r#"..."#`, `b"..."`, `br#"..."#`.
fn skip_raw_or_byte_literal(src: &[u8], i: usize) -> Option<usize> {
    let mut j = i;
    if src.get(j) == Some(&b'b') {
        j += 1;
    }
    let raw = src.get(j) == Some(&b'r');
    if raw {
        j += 1;
    }
    if !raw {
        // `b"..."`: a plain escaped string.
        return if src.get(j) == Some(&b'"') && j > i {
            Some(skip_string(src, j + 1))
        } else {
            None
        };
    }
    let hashes = {
        let mut n = 0usize;
        while src.get(j + n) == Some(&b'#') {
            n += 1;
        }
        n
    };
    j += hashes;
    if src.get(j) != Some(&b'"') {
        return None; // an identifier such as `result` or `raw_mode`, not a literal
    }
    j += 1;
    // Closing delimiter is `"` followed by exactly `hashes` `#`.
    while j < src.len() {
        if src[j] == b'"' {
            let mut k = j + 1;
            let mut n = 0usize;
            while n < hashes && src.get(k) == Some(&b'#') {
                k += 1;
                n += 1;
            }
            if n == hashes {
                return Some(k);
            }
        }
        j += 1;
    }
    Some(j)
}

/// Distinguishes a char literal from a lifetime: `'a'` is a literal, `'a` is a
/// lifetime and must be stepped over one byte at a time.
fn skip_char_literal(src: &[u8], i: usize) -> Option<usize> {
    match src.get(i + 1) {
        Some(b'\\') => {
            let mut j = i + 2;
            while j < src.len() && src[j] != b'\'' {
                j += if src[j] == b'\\' { 2 } else { 1 };
            }
            Some(j + 1)
        }
        Some(_) if src.get(i + 2) == Some(&b'\'') => Some(i + 3),
        _ => None,
    }
}

/// Splits a source range into top-level items.
///
/// An item runs from the first non-whitespace byte after the previous item —
/// which naturally absorbs its doc comments and attributes — up to the closing
/// brace of its body, or to the `;` that terminates a body-less item.
pub fn split_items(src: &str, range: Span) -> Vec<Span> {
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
        if let Some(next) = skip_opaque(bytes, i) {
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
                if brace == 0 && saw_brace && item_start.is_some() {
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
            b';' if brace == 0 && paren == 0 && bracket == 0 && !saw_brace => {
                if let Some(start) = item_start.take() {
                    items.push(Span { start, end: i + 1 });
                }
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

/// Returns the span between the first structural `{` of `item` and its matching
/// `}` (exclusive), or `None` when the item has no body.
pub fn body_span(src: &str, item: Span) -> Option<Span> {
    let bytes = src.as_bytes();
    let mut i = item.start;
    let mut open = None::<usize>;
    let mut depth = 0usize;
    while i < item.end {
        if let Some(next) = skip_opaque(bytes, i) {
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
                depth -= 1;
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

    pub fn line_count(&self) -> u32 {
        self.starts.len() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn braces_inside_strings_do_not_close_an_item() {
        let src = r#"fn a() { let s = "}"; let t = '}'; }
fn b() {}
"#;
        let items = split_items(
            src,
            Span {
                start: 0,
                end: src.len(),
            },
        );
        assert_eq!(items.len(), 2, "got {items:?}");
        assert!(items[0].slice(src).starts_with("fn a"));
        assert!(items[1].slice(src).starts_with("fn b"));
    }

    #[test]
    fn raw_strings_and_nested_block_comments_are_opaque() {
        let src = "fn a() { let r = r#\"} not a brace \"#; /* /* }} */ */ }\nfn b() {}\n";
        let items = split_items(
            src,
            Span {
                start: 0,
                end: src.len(),
            },
        );
        assert_eq!(items.len(), 2, "got {items:?}");
    }

    #[test]
    fn lifetimes_are_not_char_literals() {
        let src = "fn a<'x>(v: &'x str) -> &'x str { v }\nfn b() {}\n";
        let items = split_items(
            src,
            Span {
                start: 0,
                end: src.len(),
            },
        );
        assert_eq!(items.len(), 2, "got {items:?}");
    }

    #[test]
    fn semicolon_items_and_attributes_ride_along() {
        let src = "use std::io;\n#[derive(Debug)]\npub struct S { a: u8 }\n";
        let items = split_items(
            src,
            Span {
                start: 0,
                end: src.len(),
            },
        );
        assert_eq!(items.len(), 2);
        assert!(items[1].slice(src).starts_with("#[derive"));
    }

    #[test]
    fn body_span_isolates_the_block() {
        let src = "impl S { fn a() {} }";
        let item = Span {
            start: 0,
            end: src.len(),
        };
        let body = body_span(src, item).expect("has a body");
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
