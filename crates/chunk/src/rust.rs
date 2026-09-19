//! Rust chunker: one chunk per item, one chunk per method inside a large
//! `impl`, doc comments and attributes riding along with the item they document.

use crate::scanner::{LineIndex, Span, body_span, split_items};
use crate::text::{drop_empty, merge_small, split_windows};
use findery_core::Result;
use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, Lang, RawChunk};
use findery_core::traits::Chunker;

pub struct RustChunker;

impl Chunker for RustChunker {
    fn lang(&self) -> Lang {
        Lang::Rust
    }

    fn chunk(&self, path: &str, text: &str, cfg: &ChunkConfig) -> Result<Vec<RawChunk>> {
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        let index = LineIndex::new(text);
        let mut out = Vec::new();

        // The file-level `//!` / `#![...]` preamble documents the module, not
        // the first item that happens to follow it.
        let head_end = module_header_end(text);
        if head_end > 0 {
            let head = &text[..head_end];
            if head.trim().chars().any(|c| c.is_alphanumeric()) {
                out.push(RawChunk {
                    kind: ChunkKind::Module,
                    symbol: Some(module_name(path)),
                    breadcrumb: Some(format!("module {}", module_name(path))),
                    start_line: 1,
                    end_line: index.line_of(head_end.saturating_sub(1)),
                    text: head.trim_end().to_string(),
                });
            }
        }

        let items = split_items(
            text,
            Span {
                start: head_end,
                end: text.len(),
            },
        );
        for item in items {
            emit_item(text, item, &index, None, false, cfg, &mut out);
        }

        let out = merge_small(drop_empty(out), cfg);
        Ok(out)
    }
}

/// Emits one item, recursing into `impl` / `trait` / `mod` bodies that exceed
/// the size budget.
fn emit_item(
    src: &str,
    item: Span,
    index: &LineIndex,
    parent: Option<&str>,
    parent_is_test: bool,
    cfg: &ChunkConfig,
    out: &mut Vec<RawChunk>,
) {
    let body_text = item.slice(src);
    if body_text.trim().is_empty() {
        return;
    }
    let meta = classify(body_text);
    let is_test = parent_is_test || meta.is_test;
    if is_test && !cfg.include_tests {
        return;
    }

    let kind = if is_test {
        ChunkKind::Test
    } else if parent.is_some() && meta.kind == ChunkKind::Function {
        ChunkKind::Method
    } else {
        meta.kind
    };
    let breadcrumb = match (parent, &meta.symbol) {
        (Some(p), Some(s)) => Some(format!("{p} > {s}")),
        (Some(p), None) => Some(p.to_string()),
        (None, Some(s)) => Some(s.clone()),
        (None, None) => None,
    };
    let start_line = index.line_of(item.start);

    let recursable = matches!(
        meta.kind,
        ChunkKind::Impl | ChunkKind::Trait | ChunkKind::Module
    );
    if recursable
        && body_text.len() > cfg.max_chars
        && let Some(body) = body_span(src, item)
    {
        let children = split_items(src, body);
        if !children.is_empty() {
            let label = breadcrumb.clone().unwrap_or_else(|| "?".into());
            for child in children {
                emit_item(src, child, index, Some(&label), is_test, cfg, out);
            }
            return;
        }
    }

    if body_text.len() <= cfg.max_chars {
        out.push(RawChunk {
            kind,
            symbol: meta.symbol,
            breadcrumb,
            start_line,
            end_line: index.line_of(item.end.saturating_sub(1)),
            text: body_text.trim_end().to_string(),
        });
    } else {
        out.extend(split_windows(
            body_text,
            start_line,
            kind,
            meta.symbol,
            breadcrumb,
            cfg,
        ));
    }
}

/// What the header of an item says about it.
struct ItemMeta {
    kind: ChunkKind,
    symbol: Option<String>,
    is_test: bool,
}

/// Offset just past the leading `//!` / `#![...]` block of a file.
fn module_header_end(text: &str) -> usize {
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let t = line.trim_start();
        if t.starts_with("//!") || t.starts_with("#![") || t.is_empty() {
            offset += line.len();
        } else {
            break;
        }
    }
    // A file starting with blank lines only has no preamble worth keeping.
    if text[..offset].trim().is_empty() {
        0
    } else {
        offset
    }
}

/// `crates/core/src/path_map.rs` -> `path_map`.
fn module_name(path: &str) -> String {
    path.rsplit('/')
        .next()
        .unwrap_or(path)
        .trim_end_matches(".rs")
        .to_string()
}

/// Splits an item into its trivia (comments + attributes) and its code header.
fn classify(item: &str) -> ItemMeta {
    let (trivia, code) = split_trivia(item);
    let is_test = trivia.contains("#[cfg(test)]")
        || trivia.contains("#[test]")
        || trivia.contains("::test]")
        || trivia.contains("#[cfg(all(test");

    let header_end = code.find(['{', ';', '=']).unwrap_or(code.len());
    let header = &code[..header_end];
    let (kind, symbol) = parse_header(header);
    ItemMeta {
        kind,
        symbol,
        is_test,
    }
}

/// Returns `(trivia, code)`: everything up to the first line that is neither a
/// comment, an attribute nor blank, and the rest.
fn split_trivia(item: &str) -> (&str, &str) {
    let mut offset = 0usize;
    let mut attr_depth = 0usize;
    let mut in_block_comment = false;
    for line in item.split_inclusive('\n') {
        let t = line.trim();
        let consume = if in_block_comment {
            if t.contains("*/") {
                in_block_comment = false;
            }
            true
        } else if attr_depth > 0 {
            attr_depth += t.matches('[').count();
            attr_depth = attr_depth.saturating_sub(t.matches(']').count());
            true
        } else if t.starts_with("//") {
            true
        } else if t.starts_with("/*") {
            in_block_comment = !t.contains("*/");
            true
        } else if t.starts_with("#[") || t.starts_with("#![") {
            let opens = t.matches('[').count();
            let closes = t.matches(']').count();
            attr_depth = opens.saturating_sub(closes);
            true
        } else {
            t.is_empty()
        };
        if !consume {
            break;
        }
        offset += line.len();
    }
    (&item[..offset], &item[offset..])
}

/// Reads the kind and the symbol out of an item header (no braces, no body).
fn parse_header(header: &str) -> (ChunkKind, Option<String>) {
    // Generic parameter lists are blown apart so that `impl<'a>` tokenizes as
    // `impl`, and `struct Foo<T>` as `struct` + `Foo`.
    let cleaned = header.replace(['(', ')', ',', '<', '>'], " ");
    let mut toks = cleaned.split_whitespace().peekable();
    while let Some(tok) = toks.next() {
        match tok {
            "fn" => return (ChunkKind::Function, next_ident(toks.next())),
            "struct" => return (ChunkKind::Struct, next_ident(toks.next())),
            "enum" => return (ChunkKind::Enum, next_ident(toks.next())),
            "union" => return (ChunkKind::Struct, next_ident(toks.next())),
            "trait" => return (ChunkKind::Trait, next_ident(toks.next())),
            "mod" => return (ChunkKind::Module, next_ident(toks.next())),
            "type" | "static" => return (ChunkKind::Const, next_ident(toks.next())),
            "impl" => return (ChunkKind::Impl, impl_symbol(header)),
            "macro_rules!" => return (ChunkKind::Macro, next_ident(toks.next())),
            "const" => {
                if toks.peek() == Some(&"fn") {
                    continue;
                }
                return (ChunkKind::Const, next_ident(toks.next()));
            }
            _ => continue,
        }
    }
    (ChunkKind::Text, None)
}

/// Keeps the identifier part of a token: `Foo<T>` -> `Foo`, `X:` -> `X`.
fn next_ident(tok: Option<&str>) -> Option<String> {
    let raw = tok?;
    let ident: String = raw
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if ident.is_empty() { None } else { Some(ident) }
}

/// `impl<'a> Chunker for RustChunker` -> `Chunker for RustChunker`.
fn impl_symbol(header: &str) -> Option<String> {
    let after = header.split_once("impl")?.1;
    let after = strip_leading_generics(after);
    let s = after.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.is_empty() { None } else { Some(s) }
}

/// Drops a `<...>` generic parameter list sitting at the start of `s`.
fn strip_leading_generics(s: &str) -> &str {
    let t = s.trim_start();
    if !t.starts_with('<') {
        return t;
    }
    let mut depth = 0usize;
    for (i, c) in t.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return t[i + 1..].trim_start();
                }
            }
            _ => {}
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `min_chars` is dropped to 1: these fixtures are far below the real
    /// merge threshold, and merging is covered on its own in `text.rs`.
    fn cfg() -> ChunkConfig {
        ChunkConfig {
            min_chars: 1,
            ..ChunkConfig::default()
        }
    }

    fn chunk(src: &str) -> Vec<RawChunk> {
        RustChunker
            .chunk("crates/demo/src/thing.rs", src, &cfg())
            .expect("chunking succeeds")
    }

    #[test]
    fn module_preamble_becomes_its_own_chunk() {
        let out = chunk("//! Module docs.\n//! More.\n\nfn a() {}\n");
        assert_eq!(out[0].kind, ChunkKind::Module);
        assert_eq!(out[0].symbol.as_deref(), Some("thing"));
        assert!(out[0].text.contains("Module docs"));
    }

    #[test]
    fn items_keep_their_doc_comments_and_attributes() {
        let out = chunk("/// Docs for S.\n#[derive(Debug)]\npub struct S {\n    a: u8,\n}\n");
        let s = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("S"))
            .expect("struct found");
        assert_eq!(s.kind, ChunkKind::Struct);
        assert!(s.text.starts_with("/// Docs for S."));
    }

    #[test]
    fn a_large_impl_is_split_per_method_with_a_breadcrumb() {
        let filler = "    // padding padding padding padding padding padding\n".repeat(30);
        let src = format!(
            "impl Widget {{\n    pub fn new() -> Self {{\n{filler}        Self\n    }}\n\
             \n    pub fn render(&self) {{\n{filler}    }}\n}}\n"
        );
        let out = chunk(&src);
        let names: Vec<_> = out.iter().filter_map(|c| c.symbol.clone()).collect();
        assert!(names.contains(&"new".to_string()), "got {names:?}");
        assert!(names.contains(&"render".to_string()), "got {names:?}");
        let m = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("render"))
            .expect("method");
        assert_eq!(m.kind, ChunkKind::Method);
        assert_eq!(m.breadcrumb.as_deref(), Some("Widget > render"));
    }

    #[test]
    fn a_small_impl_stays_in_one_piece() {
        let out = chunk("impl Widget {\n    fn a(&self) {}\n}\n");
        let impls: Vec<_> = out.iter().filter(|c| c.kind == ChunkKind::Impl).collect();
        assert_eq!(impls.len(), 1);
        assert_eq!(impls[0].symbol.as_deref(), Some("Widget"));
    }

    #[test]
    fn test_modules_are_tagged_and_can_be_excluded() {
        let src = "#[cfg(test)]\nmod tests {\n    #[test]\n    fn works() { assert!(true); }\n}\n";
        let out = chunk(src);
        assert!(out.iter().any(|c| c.kind == ChunkKind::Test));

        let strict = ChunkConfig {
            include_tests: false,
            ..cfg()
        };
        let out = RustChunker
            .chunk("a.rs", src, &strict)
            .expect("chunking succeeds");
        assert!(out.is_empty(), "got {out:?}");
    }

    #[test]
    fn trait_impl_symbol_drops_generics() {
        let out = chunk("impl<'a> Chunker for RustChunker {\n    fn lang(&self) {}\n}\n");
        assert_eq!(out[0].symbol.as_deref(), Some("Chunker for RustChunker"));
    }

    #[test]
    fn line_numbers_point_at_the_real_item() {
        let out = chunk("fn a() {}\n\nfn b() {\n    ();\n}\n");
        let b = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("b"))
            .expect("fn b");
        assert_eq!(b.start_line, 3);
        assert_eq!(b.end_line, 5);
    }

    #[test]
    fn consts_and_uses_are_recognised() {
        let out = chunk("use std::io;\n\npub const MAX: usize = 4;\n\nfn a() {}\n");
        assert!(out.iter().any(|c| c.symbol.as_deref() == Some("MAX")));
    }
}
