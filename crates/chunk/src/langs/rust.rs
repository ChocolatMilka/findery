//! Rust chunker: one chunk per item, one chunk per method inside a large
//! `impl`, doc comments and attributes riding along with the item they document.

use crate::brace::{
    ItemMeta, Profile, chunk_with, inner_doc_preamble_end, next_ident, split_trivia,
};
use crate::items::LineIndex;
use crate::scanner::Lexicon;
use findery_core::Result;
use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, Lang, RawChunk};
use findery_core::traits::Chunker;

/// Splits Rust source into items.
pub struct RustChunker;

impl Chunker for RustChunker {
    fn lang(&self) -> Lang {
        Lang::Rust
    }

    fn chunk(&self, path: &str, text: &str, cfg: &ChunkConfig) -> Result<Vec<RawChunk>> {
        chunk_with(&RustProfile, path, text, cfg)
    }
}

struct RustProfile;

impl Profile for RustProfile {
    fn lexicon(&self) -> &'static Lexicon {
        &Lexicon::RUST
    }

    fn classify(&self, item: &str, _nested: bool) -> ItemMeta {
        let (trivia, code) = split_trivia(item, self.lexicon());
        // `#[cfg(test)]` and `#[test]` tag the item; the `::test]` and
        // `cfg(all(test` forms catch a test attribute that carries arguments.
        let is_test = trivia.contains("#[cfg(test)]")
            || trivia.contains("#[test]")
            || trivia.contains("::test]")
            || trivia.contains("#[cfg(all(test");

        let header_end = code.find(['{', ';', '=']).unwrap_or(code.len());
        let (kind, symbol) = parse_header(&code[..header_end]);
        ItemMeta {
            kind,
            symbol,
            is_test,
        }
    }

    fn preamble(&self, src: &str, path: &str, index: &LineIndex) -> (usize, Option<RawChunk>) {
        // The file-level `//!` / `#![...]` preamble documents the module, not
        // the first item that happens to follow it.
        let end = inner_doc_preamble_end(src);
        let head = &src[..end];
        if end == 0 || !head.trim().chars().any(|c| c.is_alphanumeric()) {
            return (end, None);
        }
        let name = module_name(path);
        let chunk = RawChunk {
            kind: ChunkKind::Module,
            symbol: Some(name.clone()),
            breadcrumb: Some(format!("module {name}")),
            start_line: 1,
            end_line: index.line_of(end.saturating_sub(1)),
            text: head.trim_end().to_string(),
        };
        (end, Some(chunk))
    }

    fn recursable(&self, kind: ChunkKind) -> bool {
        matches!(kind, ChunkKind::Impl | ChunkKind::Trait | ChunkKind::Module)
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
    fn a_test_attribute_sharing_its_line_is_still_a_test() {
        let src = "#[ignore] #[test] fn t() {}\n\nfn keep() {}\n";
        let out = chunk(src);
        let t = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("t"))
            .expect("t");
        assert_eq!(t.kind, ChunkKind::Test);

        let strict = ChunkConfig {
            include_tests: false,
            ..cfg()
        };
        let kept = RustChunker
            .chunk("a.rs", src, &strict)
            .expect("chunking succeeds");
        assert_eq!(
            kept.iter()
                .filter_map(|c| c.symbol.as_deref())
                .collect::<Vec<_>>(),
            vec!["keep"],
            "got {kept:?}"
        );
    }

    #[test]
    fn an_attribute_sharing_its_line_with_the_item_keeps_the_name() {
        let out = chunk("#[inline] pub fn a() { 1 }\n\n#[derive(Debug)] pub struct S;\n");
        assert_eq!(
            out.iter()
                .filter_map(|c| c.symbol.as_deref())
                .collect::<Vec<_>>(),
            vec!["a", "S"],
            "got {out:?}"
        );
        assert_eq!(out[0].kind, ChunkKind::Function);
        assert_eq!(out[1].kind, ChunkKind::Struct);
    }

    #[test]
    fn consts_and_uses_are_recognised() {
        let out = chunk("use std::io;\n\npub const MAX: usize = 4;\n\nfn a() {}\n");
        assert!(out.iter().any(|c| c.symbol.as_deref() == Some("MAX")));
    }

    #[test]
    fn a_non_ascii_char_literal_keeps_the_next_item_separate() {
        let src = "fn a(c: char) -> bool { matches!(c, 'é'|'{') }\n\nfn b() {}\n";
        let out = chunk(src);
        let names: Vec<_> = out.iter().filter_map(|c| c.symbol.clone()).collect();
        assert_eq!(names, vec!["a".to_string(), "b".to_string()], "got {out:?}");
    }

    #[test]
    fn a_static_holding_a_closure_keeps_its_whole_statement() {
        let src = "static X: Lazy<u8> = Lazy::new(|| { 1 });\n\nfn b() {}\n";
        let out = chunk(src);
        let x = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("X"))
            .expect("static X");
        assert!(x.text.ends_with(");"), "got {:?}", x.text);
        assert!(out.iter().any(|c| c.symbol.as_deref() == Some("b")));
    }
}
