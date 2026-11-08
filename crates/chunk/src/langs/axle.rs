//! Axle chunker: one chunk per top-level declaration, one per member inside a
//! class, trait or struct big enough to be split.
//!
//! INTENT: Axle is brace-delimited with `//` and `/* */` comments, so it rides
//! the shared pipeline of [`crate::brace`] and only says what is Axle's own: its
//! keywords, its `@annotation(…)` prefixes, and the two members that have no
//! `fn` — `constructor` and `drop`, named after the keyword itself. The
//! syntax is still moving; everything that may change is the keyword table
//! below, nothing structural.

use crate::brace::{ItemMeta, Profile, chunk_with, next_ident, split_trivia};
use crate::scanner::Lexicon;
use findery_core::Result;
use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, Lang, RawChunk};
use findery_core::traits::Chunker;

/// Splits Axle source into items.
pub struct AxleChunker;

impl Chunker for AxleChunker {
    fn lang(&self) -> Lang {
        Lang::Axle
    }

    fn chunk(&self, path: &str, text: &str, cfg: &ChunkConfig) -> Result<Vec<RawChunk>> {
        chunk_with(&AxleProfile, path, text, cfg)
    }
}

/// The words that name an item. Visibility (`pub`, `pub(crate)`, `pub(file)`),
/// `async`, `static`, `own`, `shared` and `extern "C"` are modifiers: the scan
/// walks past them and the keyword still names the item.
const KEYWORDS: &[(&str, ChunkKind)] = &[
    ("fn", ChunkKind::Function),
    ("class", ChunkKind::Struct),
    ("struct", ChunkKind::Struct),
    ("exception", ChunkKind::Struct),
    ("trait", ChunkKind::Trait),
    ("enum", ChunkKind::Enum),
    ("const", ChunkKind::Const),
    ("type", ChunkKind::Const),
    ("impl", ChunkKind::Impl),
    ("constructor", ChunkKind::Function),
    ("drop", ChunkKind::Function),
];

/// Members whose name IS the keyword: `constructor(v : i32)` has no identifier
/// after it, only a parameter list.
const SELF_NAMED: &[&str] = &["constructor", "drop"];

struct AxleProfile;

impl Profile for AxleProfile {
    fn lexicon(&self) -> &'static Lexicon {
        &Lexicon::AXLE
    }

    fn classify(&self, item: &str, _nested: bool) -> ItemMeta {
        let (_, code) = split_trivia(item, self.lexicon());
        // `=` ends the header too: `const N : i32 = 4;` names `N`, and what
        // follows the `=` is a value that may spell any keyword.
        let header_end = code.find(['{', ';', '=']).unwrap_or(code.len());
        let (kind, symbol) = parse_header(&code[..header_end]);
        ItemMeta {
            kind,
            symbol,
            is_test: false,
        }
    }

    fn recursable(&self, kind: ChunkKind) -> bool {
        matches!(kind, ChunkKind::Struct | ChunkKind::Trait | ChunkKind::Impl)
    }
}

/// Reads the kind and the symbol out of an item header (no braces, no body).
fn parse_header(header: &str) -> (ChunkKind, Option<String>) {
    // Generic lists and parameter lists are blown apart so that
    // `class List<T>` tokenizes as `class` + `List`.
    let cleaned = header.replace(['(', ')', ',', '<', '>', ':'], " ");
    let mut toks = cleaned.split_whitespace();
    while let Some(tok) = toks.next() {
        let Some((word, kind)) = KEYWORDS.iter().find(|(kw, _)| *kw == tok) else {
            continue;
        };
        let symbol = if SELF_NAMED.contains(word) {
            Some((*word).to_string())
        } else {
            next_ident(toks.next())
        };
        return (*kind, symbol);
    }
    (ChunkKind::Text, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(src: &str) -> Vec<RawChunk> {
        tight(src, ChunkConfig::default().max_chars)
    }

    fn tight(src: &str, max_chars: usize) -> Vec<RawChunk> {
        let cfg = ChunkConfig {
            min_chars: 1,
            max_chars,
            ..ChunkConfig::default()
        };
        AxleChunker
            .chunk("std/collections/list.axle", src, &cfg)
            .expect("chunking succeeds")
    }

    fn symbols(out: &[RawChunk]) -> Vec<&str> {
        out.iter().filter_map(|c| c.symbol.as_deref()).collect()
    }

    #[test]
    fn visibility_and_async_do_not_hide_the_name() {
        let out = chunk(
            "pub(crate) async fn fetch(url : string) : string ! IoError {\n    return url;\n}\n",
        );
        assert_eq!(out[0].kind, ChunkKind::Function);
        assert_eq!(out[0].symbol.as_deref(), Some("fetch"));
    }

    #[test]
    fn declarations_get_their_kind() {
        let out = chunk(
            "enum Sort {\n    Up,\n    Down,\n}\n\nexception IoError : Exception {\n    code : i32;\n}\n\n\
             trait Eq {\n    fn eq(self, o : Self) : bool;\n}\n\npub const MAX : i64 = 8;\n",
        );
        let kinds: Vec<_> = out.iter().map(|c| (c.kind, c.symbol.as_deref())).collect();
        assert_eq!(
            kinds,
            vec![
                (ChunkKind::Enum, Some("Sort")),
                (ChunkKind::Struct, Some("IoError")),
                (ChunkKind::Trait, Some("Eq")),
                (ChunkKind::Const, Some("MAX")),
            ],
            "got {out:?}"
        );
    }

    #[test]
    fn an_annotated_extern_declaration_is_one_named_item() {
        let out = chunk(
            "@link(symbol = \"getpid\")\nextern \"C\" fn libc_getpid() : i32;\n\nfn after() : void {}\n",
        );
        assert_eq!(symbols(&out), vec!["libc_getpid", "after"], "got {out:?}");
        assert!(out[0].text.starts_with("@link("));
    }

    #[test]
    fn doc_comments_ride_with_their_item() {
        let out = chunk(
            "/** Adds. */\n/// More.\nfn add(a : i32, b : i32) : i32 {\n    return a + b;\n}\n",
        );
        assert!(out[0].text.starts_with("/** Adds. */"), "got {out:?}");
        assert_eq!(out[0].symbol.as_deref(), Some("add"));
    }

    #[test]
    fn braces_in_strings_templates_and_chars_do_not_close_an_item() {
        let src = "fn a() : string {\n    let c = '}';\n    return `x { ${c} }\n    }`;\n}\n\nfn b() : string {\n    return \"}\";\n}\n";
        assert_eq!(symbols(&chunk(src)), vec!["a", "b"]);
    }

    #[test]
    fn a_large_class_splits_per_member_and_names_constructor_and_drop() {
        let filler = "        // padding padding padding padding padding padding\n".repeat(12);
        let src = format!(
            "class Buf<T> {{\n    data : T[];\n\n    constructor(n : i64) {{\n{filler}    }}\n\n\
             pub fn size(self) : i64 {{\n{filler}    }}\n\n    drop() {{\n{filler}    }}\n}}\n"
        );
        let out = tight(&src, 300);
        let size = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("size"))
            .expect("size");
        assert_eq!(size.kind, ChunkKind::Method);
        assert_eq!(size.breadcrumb.as_deref(), Some("Buf > size"));
        let names = symbols(&out);
        assert!(names.contains(&"constructor"), "got {names:?}");
        assert!(names.contains(&"drop"), "got {names:?}");
    }

    #[test]
    fn a_small_class_stays_in_one_piece() {
        let out =
            chunk("class P {\n    x : i32;\n    pub fn get(self) : i32 { return self.x; }\n}\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].symbol.as_deref(), Some("P"));
    }

    #[test]
    fn a_value_spelling_a_keyword_does_not_rename_the_constant() {
        let out = chunk("const FN_NAME : string = \"fn other\";\n");
        assert_eq!(out[0].symbol.as_deref(), Some("FN_NAME"));
    }

    #[test]
    fn an_unterminated_string_still_yields_something() {
        let out = chunk("fn a() : string { return \"open;\n}\n");
        assert!(!out.is_empty());
    }
}
