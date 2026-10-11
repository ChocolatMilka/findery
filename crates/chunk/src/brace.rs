//! The pipeline every brace-delimited language shares.
//!
//! INTENT: Rust, JavaScript, PHP and JSON differ in their keywords and their
//! trivia, not in the shape of an item. Writing that pipeline once — split,
//! classify, recurse into an oversized container, window what is still too big
//! — is what keeps a fix to item boundaries a fix for every language at once
//! (CONV-024: the chunker splits, and nothing else).

use crate::items::{LineIndex, Span, body_span, split_items};
use crate::scanner::Lexicon;
use crate::text::{drop_empty, merge_small, split_windows};
use findery_core::Result;
use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, RawChunk};

/// What the header of an item says about it.
pub(crate) struct ItemMeta {
    pub kind: ChunkKind,
    pub symbol: Option<String>,
    pub is_test: bool,
}

/// A language as the shared pipeline needs to see it.
pub(crate) trait Profile {
    /// The language's lexical trivia.
    fn lexicon(&self) -> &'static Lexicon;

    /// Reads the kind and the symbol out of one item's text. `nested` says the
    /// item sits inside a container, which is what makes a `Method` a method.
    fn classify(&self, item: &str, nested: bool) -> ItemMeta;

    /// The file preamble — module docs, a shebang, `<?php` — and the chunk it
    /// becomes. Returns where the items start, and the chunk, if any.
    fn preamble(&self, _src: &str, _path: &str, _index: &LineIndex) -> (usize, Option<RawChunk>) {
        (0, None)
    }

    /// Whether an item of this kind, too big for one chunk, is split per member.
    fn recursable(&self, kind: ChunkKind) -> bool {
        matches!(
            kind,
            ChunkKind::Impl | ChunkKind::Trait | ChunkKind::Module | ChunkKind::Struct
        )
    }

    /// The kind a member takes once it sits inside a parent.
    fn nested_kind(&self, kind: ChunkKind) -> ChunkKind {
        if kind == ChunkKind::Function {
            ChunkKind::Method
        } else {
            kind
        }
    }
}

/// The fixed inputs of one chunking pass, carried together so a recursive call
/// reads as what it is: an item, its parent, and where to write.
struct Emitter<'a> {
    src: &'a str,
    index: &'a LineIndex,
    cfg: &'a ChunkConfig,
    profile: &'a dyn Profile,
    lex: &'a Lexicon,
}

/// Splits one file: preamble, then one pass per top-level item.
pub(crate) fn chunk_with(
    profile: &dyn Profile,
    path: &str,
    src: &str,
    cfg: &ChunkConfig,
) -> Result<Vec<RawChunk>> {
    if src.trim().is_empty() {
        return Ok(Vec::new());
    }
    let index = LineIndex::new(src);
    let lex = profile.lexicon();
    let mut out = Vec::new();
    let (start, head) = profile.preamble(src, path, &index);
    out.extend(head);
    let emitter = Emitter {
        src,
        index: &index,
        cfg,
        profile,
        lex,
    };
    for item in split_items(
        src,
        Span {
            start,
            end: src.len(),
        },
        lex,
    ) {
        emitter.emit_item(item, None, false, &mut out);
    }
    Ok(merge_small(drop_empty(out), cfg))
}

impl Emitter<'_> {
    /// Emits one item, recursing into a container body that exceeds the budget.
    fn emit_item(
        &self,
        item: Span,
        parent: Option<&str>,
        parent_is_test: bool,
        out: &mut Vec<RawChunk>,
    ) {
        let body_text = item.slice(self.src);
        if body_text.trim().is_empty() {
            return;
        }
        let meta = self.profile.classify(body_text, parent.is_some());
        let is_test = parent_is_test || meta.is_test;
        if is_test && !self.cfg.include_tests {
            return;
        }

        let kind = if is_test {
            ChunkKind::Test
        } else if parent.is_some() {
            self.profile.nested_kind(meta.kind)
        } else {
            meta.kind
        };
        let breadcrumb = match (parent, &meta.symbol) {
            (Some(p), Some(s)) => Some(format!("{p} > {s}")),
            (Some(p), None) => Some(p.to_string()),
            (None, Some(s)) => Some(s.clone()),
            (None, None) => None,
        };
        let start_line = self.index.line_of(item.start);

        if self.profile.recursable(meta.kind)
            && body_text.len() > self.cfg.max_chars
            && let Some(body) = body_span(self.src, item, self.lex)
        {
            let children = split_items(self.src, body, self.lex);
            if !children.is_empty() {
                let label = breadcrumb.clone().unwrap_or_else(|| "?".into());
                for child in children {
                    self.emit_item(child, Some(&label), is_test, out);
                }
                return;
            }
        }

        if body_text.len() <= self.cfg.max_chars {
            out.push(RawChunk {
                kind,
                symbol: meta.symbol,
                breadcrumb,
                start_line,
                end_line: self.index.line_of(item.end.saturating_sub(1)),
                text: body_text.trim_end().to_string(),
            });
        } else {
            out.extend(split_windows(
                body_text,
                start_line,
                kind,
                meta.symbol,
                breadcrumb,
                self.cfg,
            ));
        }
    }
}

/// Returns `(trivia, code)`: everything up to the first byte that is not a
/// comment, an attribute, a decorator or whitespace, and the rest.
///
/// INTENT: a scanner, not a line loop. Several attributes may share a line with
/// each other and with the item they document — `#[ignore] #[test] fn t() {}` is
/// one item, tested, named `t` — and only a byte-wise walk gets that right. The
/// trivia is read for the item's `kind`, its symbol and its `test` flag; the
/// item's TEXT is its whole span either way.
pub(crate) fn split_trivia<'a>(item: &'a str, lex: &Lexicon) -> (&'a str, &'a str) {
    let bytes = item.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if lex.attributes && starts_with(bytes, i, b"#[") {
            match balanced_at(bytes, i + 1, lex, ATTRIBUTE_BRACKETS) {
                Some(end) => i = end,
                // An attribute that never closes swallows the rest of the item.
                None => return (item, &item[item.len()..]),
            }
            continue;
        }
        if lex.attributes && starts_with(bytes, i, b"#![") {
            match balanced_at(bytes, i + 2, lex, ATTRIBUTE_BRACKETS) {
                Some(end) => i = end,
                None => return (item, &item[item.len()..]),
            }
            continue;
        }
        if lex.decorators && b == b'@' {
            i = decorator_end(bytes, i, lex);
            continue;
        }
        // Before the line comments is impossible here: `#[…]` was handled
        // above, and every other `#` really is a comment.
        let is_comment = lex
            .line_comments
            .iter()
            .any(|open| starts_with(bytes, i, open.as_bytes()))
            || lex
                .block_comment
                .as_ref()
                .is_some_and(|bc| starts_with(bytes, i, bc.open.as_bytes()));
        if is_comment && let Some(next) = crate::scanner::skip_opaque(bytes, i, lex) {
            i = next.min(bytes.len());
            continue;
        }
        break;
    }
    (&item[..i], &item[i..])
}

/// Whether `needle` sits at `i`.
fn starts_with(bytes: &[u8], i: usize, needle: &[u8]) -> bool {
    bytes[i..].starts_with(needle)
}

/// The pairs an attribute's argument list is written with.
const ATTRIBUTE_BRACKETS: &[(u8, u8)] = &[(b'[', b']')];

/// The pairs a decorator's argument list is written with.
const DECORATOR_BRACKETS: &[(u8, u8)] = &[(b'(', b')'), (b'[', b']'), (b'{', b'}')];

/// The offset just past the construct opened at `start`, or `None` when it
/// never closes before the end of the input.
///
/// INTENT: the count runs through [`crate::scanner::skip_opaque`], so a bracket
/// inside a string — `@Route('/', methods: ['GET'])`, `#[doc = "a ] b"]` — is
/// not a bracket at all, and a list may span several lines.
fn balanced_at(bytes: &[u8], start: usize, lex: &Lexicon, pairs: &[(u8, u8)]) -> Option<usize> {
    let mut i = start;
    let mut depth = 0isize;
    while i < bytes.len() {
        if let Some(next) = crate::scanner::skip_opaque(bytes, i, lex) {
            i = next.min(bytes.len());
            continue;
        }
        if pairs.iter().any(|(open, _)| *open == bytes[i]) {
            depth += 1;
        } else if pairs.iter().any(|(_, close)| *close == bytes[i]) {
            depth -= 1;
            if depth <= 0 {
                return Some(i + 1);
            }
        }
        i += 1;
    }
    None
}

/// The offset just past a decorator starting at `@`: its name, then its
/// argument list when it has one. `@sealed class X` is a decorator followed by
/// code, not two lines of trivia.
fn decorator_end(bytes: &[u8], i: usize, lex: &Lexicon) -> usize {
    let mut name = i + 1;
    while bytes
        .get(name)
        .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'$' | b'.'))
    {
        name += 1;
    }
    if name == i + 1 {
        return name; // a lone `@`: not a decorator, let the caller decide
    }
    let mut args = name;
    while bytes
        .get(args)
        .is_some_and(|c| c.is_ascii_whitespace() && *c != b'\n')
    {
        args += 1;
    }
    match bytes.get(args) {
        Some(b'(') | Some(b'[') | Some(b'{') => {
            balanced_at(bytes, args, lex, DECORATOR_BRACKETS).unwrap_or(bytes.len())
        }
        _ => name,
    }
}

/// Reads a kind and a symbol out of a header using a keyword table.
///
/// INTENT: the token AFTER the keyword names the item. Scanning the header for
/// the first keyword — rather than parsing it — is what makes `export default
/// async function`, `public static function` and `abstract class` one rule.
pub(crate) fn keyword_header(
    header: &str,
    table: &[(&str, ChunkKind)],
) -> (ChunkKind, Option<String>) {
    // Generic parameter lists are blown apart so that `impl<'a>` tokenizes as
    // `impl`, and `struct Foo<T>` as `struct` + `Foo`.
    let cleaned = header.replace(['(', ')', ',', '<', '>'], " ");
    let mut toks = cleaned.split_whitespace();
    while let Some(tok) = toks.next() {
        if let Some((_, kind)) = table.iter().find(|(kw, _)| *kw == tok) {
            return (*kind, next_ident(toks.next()));
        }
    }
    (ChunkKind::Text, None)
}

/// Keeps the identifier part of a token: `Foo<T>` -> `Foo`, `X:` -> `X`.
///
/// A backslash belongs to it and to nothing else: `namespace App\Http` is one
/// name, and no other language puts a `\` in an identifier.
pub(crate) fn next_ident(tok: Option<&str>) -> Option<String> {
    let raw = tok?;
    let ident: String = raw
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '\\')
        .collect();
    if ident.is_empty() { None } else { Some(ident) }
}

/// The line offset where a leading `//!` / `#![…]` preamble ends.
///
/// INTENT: an inner attribute can span several lines, so the attribute's
/// bracket depth — not the mere shape of a line — decides where the preamble
/// stops.
pub(crate) fn inner_doc_preamble_end(text: &str) -> usize {
    let mut offset = 0usize;
    let mut attr_depth = 0usize;
    for line in text.split_inclusive('\n') {
        let t = line.trim_start();
        if attr_depth > 0 {
            attr_depth += t.matches('[').count();
            attr_depth = attr_depth.saturating_sub(t.matches(']').count());
            offset += line.len();
        } else if t.starts_with("//!") || t.is_empty() {
            offset += line.len();
        } else if t.starts_with("#![") {
            attr_depth = t
                .matches('[')
                .count()
                .saturating_sub(t.matches(']').count());
            offset += line.len();
        } else {
            break;
        }
    }
    // A file that opens on blank lines has no preamble worth keeping.
    if text[..offset].trim().is_empty() {
        0
    } else {
        offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trivia_stops_at_the_first_line_of_code() {
        let item = "/// Doc.\n// plain\n#[derive(Debug)]\nstruct S {\n    a: u8,\n}\n";
        let (trivia, code) = split_trivia(item, &Lexicon::RUST);
        assert!(trivia.ends_with("#[derive(Debug)]\n"));
        assert!(code.starts_with("struct S"));
    }

    #[test]
    fn a_php_hash_comment_is_trivia_but_an_attribute_is_too() {
        let item = "# a comment\n#[Attr]\nclass C {}\n";
        let (trivia, code) = split_trivia(item, &Lexicon::PHP);
        assert!(trivia.contains("# a comment"));
        assert!(trivia.contains("#[Attr]"));
        assert!(code.starts_with("class C"));
    }

    #[test]
    fn a_multi_line_attribute_stays_in_the_preamble() {
        let src =
            "//! Module docs.\n#![cfg_attr(\n    test,\n    allow(dead_code)\n)]\n\nfn a() {}\n";
        let end = inner_doc_preamble_end(src);
        assert!(src[..end].contains("allow(dead_code)"));
        assert!(!src[..end].contains("fn a"));
    }

    #[test]
    fn a_decorator_with_braced_arguments_stays_trivia() {
        let item = "@Component({\n    selector: 'app-widget',\n    template: 'a ] b'\n})\nexport class Widget {\n    render() {}\n}\n";
        let (trivia, code) = split_trivia(item, &Lexicon::JAVASCRIPT);
        assert!(trivia.contains("selector"), "got {trivia:?}");
        assert!(trivia.contains(')'), "got {trivia:?}");
        assert!(code.starts_with("export class Widget"), "got {code:?}");
    }

    #[test]
    fn an_attribute_sharing_its_line_with_the_item_does_not_eat_it() {
        // `#[inline] fn f() {}`: the attribute is trivia, the declaration that
        // follows it is not — reading the whole line as trivia loses the item.
        let item = "#[inline]\nfn a() {}\n";
        let (trivia, code) = split_trivia(item, &Lexicon::RUST);
        assert_eq!(trivia, "#[inline]\n");
        assert!(code.starts_with("fn a"), "got {code:?}");

        let item = "#[inline] pub fn a() { 1 }\n";
        let (trivia, code) = split_trivia(item, &Lexicon::RUST);
        assert_eq!(trivia.trim_end(), "#[inline]");
        assert!(code.trim_start().starts_with("pub fn a"), "got {code:?}");

        let item = "@Component() class Widget {}\n";
        let (trivia, code) = split_trivia(item, &Lexicon::JAVASCRIPT);
        assert_eq!(trivia.trim_end(), "@Component()");
        assert!(
            code.trim_start().starts_with("class Widget"),
            "got {code:?}"
        );
    }

    #[test]
    fn every_trivia_of_a_shared_line_is_read() {
        // The test flag is read from the trivia, so a second attribute sharing
        // the line must be trivia too — otherwise `include_tests = false` keeps
        // a test it promised to drop.
        let (trivia, code) = split_trivia("#[ignore] #[test] fn t() {}\n", &Lexicon::RUST);
        assert!(trivia.contains("#[ignore]"), "got {trivia:?}");
        assert!(trivia.contains("#[test]"), "got {trivia:?}");
        assert!(code.trim_start().starts_with("fn t"), "got {code:?}");

        // A decorator with no argument list ends at its name.
        let (trivia, code) = split_trivia("@sealed class Foo {}\n", &Lexicon::JAVASCRIPT);
        assert_eq!(trivia.trim_end(), "@sealed");
        assert!(code.trim_start().starts_with("class Foo"), "got {code:?}");
    }

    #[test]
    fn a_multi_line_attribute_counts_no_bracket_inside_a_string() {
        let item = "#[doc = \"a ] b\"]\nfn a() {}\n";
        let (trivia, code) = split_trivia(item, &Lexicon::RUST);
        assert!(trivia.contains("doc"), "got {trivia:?}");
        assert!(code.starts_with("fn a"), "got {code:?}");
    }

    #[test]
    fn the_keyword_after_the_modifiers_names_the_item() {
        let table = &[
            ("function", ChunkKind::Function),
            ("class", ChunkKind::Struct),
        ];
        assert_eq!(
            keyword_header("export default async function boot ", table),
            (ChunkKind::Function, Some("boot".into()))
        );
        assert_eq!(
            keyword_header("public static function render ", table),
            (ChunkKind::Function, Some("render".into()))
        );
    }
}
