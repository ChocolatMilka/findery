//! Axle chunker: one chunk per top-level declaration, one per member inside a
//! class, trait or struct big enough to be split.
//!
//! INTENT: Axle is brace-delimited with `//` and `/* */` comments, so it rides
//! the shared pipeline of [`crate::brace`] and only says what is Axle's own: its
//! keywords, its `@annotation(…)` prefixes, and the two members that have no
//! `fn` — `constructor` and `drop`, named after the keyword itself. The
//! syntax is still moving; everything that may change is the keyword table
//! below, nothing structural.

use findery_core::Result;
use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, Lang, RawChunk};
use findery_core::traits::Chunker;

use crate::brace::{ItemMeta, Profile, chunk_with};
use crate::header::{header_of, next_ident, split_trivia};
use crate::scanner::Lexicon;

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

    fn classify(&self, item: &str, _parent: Option<ChunkKind>) -> ItemMeta {
        let lex = self.lexicon();
        let (_, code) = split_trivia(item, lex);
        // `=` ends the header too: `const N : i32 = 4;` names `N`, and what
        // follows the `=` is a value that may spell any keyword. A parameter's
        // default (`fn f(x : i32 = 4)`) does not.
        let (kind, symbol) = parse_header(header_of(code, lex, b"{;="));
        ItemMeta {
            kind,
            symbol,
            is_test: false,
        }
    }

    fn recursable(&self, kind: ChunkKind) -> bool {
        matches!(
            kind,
            ChunkKind::Struct | ChunkKind::Trait | ChunkKind::Impl | ChunkKind::Module
        )
    }

    /// A `when T : Bound { … }` group — the one `Impl` that sits inside a
    /// class — only gathers methods under a condition: when its class is
    /// split, its methods are split too.
    fn splits_with_parent(&self, kind: ChunkKind) -> bool {
        kind == ChunkKind::Impl
    }
}

/// Reads the kind and the symbol out of an item header (no braces, no body).
fn parse_header(header: &str) -> (ChunkKind, Option<String>) {
    // Generic lists and parameter lists are blown apart so that
    // `class List<T>` tokenizes as `class` + `List`.
    let cleaned = header.replace(['(', ')', ',', '<', '>', ':'], " ");
    let toks: Vec<&str> = cleaned.split_whitespace().collect();
    for (k, tok) in toks.iter().enumerate() {
        let Some((word, kind)) = KEYWORDS.iter().find(|(kw, _)| kw == tok) else {
            continue;
        };
        let symbol = if SELF_NAMED.contains(word) {
            Some((*word).to_string())
        } else {
            next_ident(toks.get(k + 1).copied())
        };
        return (*kind, symbol);
    }
    // No declaring keyword: the three forms that declare without one.
    match toks.as_slice() {
        // `when T : Printable { … }` — conditional methods, named by their
        // condition, as an `impl` is named by what it implements.
        ["when", ..] => {
            let condition = header.split_whitespace().collect::<Vec<_>>().join(" ");
            (ChunkKind::Impl, Some(condition))
        }
        // `extern "C" from "user32" { … }` — a file of bindings, named after
        // the library it binds.
        _ if toks.contains(&"extern") && toks.contains(&"from") => {
            let lib = toks
                .iter()
                .skip_while(|t| **t != "from")
                .nth(1)
                .map(|t| t.trim_matches('"'))
                .filter(|t| !t.is_empty())
                .map(str::to_string);
            (ChunkKind::Module, lib)
        }
        // `pub static MAX : i64 = 500;` — one slot for the class.
        _ if toks.contains(&"static") => {
            let after = toks.iter().skip_while(|t| **t != "static").nth(1);
            (ChunkKind::Const, next_ident(after.copied()))
        }
        _ => (ChunkKind::Text, None),
    }
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
    fn a_stop_byte_inside_a_string_does_not_cut_the_header() {
        let out = chunk("extern \"C\" from \"lib{x}=1\" {\n    fn f() : i32;\n}\n");
        assert_eq!(out[0].kind, ChunkKind::Module, "got {out:#?}");
        assert_eq!(out[0].symbol.as_deref(), Some("lib{x}=1"));
    }

    #[test]
    fn a_static_fn_and_a_derived_visibility_are_still_named_members() {
        let filler = "        // padding padding padding padding padding padding\n".repeat(4);
        let src = format!(
            "class StringUtil {{\n    pub static fn isAscii(s : string) : bool {{\n{filler}        return true;\n    }}\n\n\
             \x20   pub(derived) fn hook(self) : void {{\n{filler}    }}\n\n\
             \x20   readonly id : i64;\n    pub static MAX_OVERDRAFT : i64 = 500;\n}}\n"
        );
        let out = tight(&src, 200);
        let is_ascii = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("isAscii"))
            .expect("isAscii");
        assert_eq!(is_ascii.kind, ChunkKind::Method);
        assert_eq!(
            out.iter()
                .find(|c| c.symbol.as_deref() == Some("hook"))
                .map(|c| c.kind),
            Some(ChunkKind::Method)
        );
        // A static slot is the class's constant; a readonly field is a field.
        assert_eq!(
            out.iter()
                .find(|c| c.symbol.as_deref() == Some("MAX_OVERDRAFT"))
                .map(|c| c.kind),
            Some(ChunkKind::Const),
            "got {out:#?}"
        );
        assert!(!out.iter().any(|c| c.symbol.as_deref() == Some("id")));
    }

    #[test]
    fn an_extern_group_is_one_module_named_after_its_library() {
        let out = chunk(
            "@target(os = windows)\nextern \"C\" from \"winmm\" {\n    fn timeBeginPeriod(ms : i32) : i32;\n}\n\nfn after() : void {}\n",
        );
        assert_eq!(out[0].kind, ChunkKind::Module, "got {out:#?}");
        assert_eq!(out[0].symbol.as_deref(), Some("winmm"));
        assert!(out[0].text.starts_with("@target("));
        assert_eq!(symbols(&out), vec!["winmm", "after"]);
    }

    #[test]
    fn a_large_extern_group_splits_per_binding() {
        let src = "extern \"C\" from \"user32\" {\n    fn RegisterClassExW(wc : ptr) : i32;\n    fn CreateWindowExW(style : i32) : ptr;\n}\n";
        let out = tight(src, 60);
        let create = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("CreateWindowExW"))
            .expect("CreateWindowExW");
        assert_eq!(
            create.breadcrumb.as_deref(),
            Some("user32 > CreateWindowExW")
        );
    }

    #[test]
    fn a_when_group_splits_with_its_class_and_keeps_its_methods_named() {
        let filler = "        // padding padding padding padding padding padding\n".repeat(4);
        let src = format!(
            "class Wrapper<T> {{\n    value : T;\n\n    pub fn get(self) : T {{\n{filler}        return self.value;\n    }}\n\n\
             \x20   when T : Printable {{\n        fn tag(self) : i32 {{ return 1; }}\n        fn show(self) : void {{}}\n    }}\n}}\n"
        );
        let out = tight(&src, 200);
        let tag = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("tag"))
            .expect("tag is its own chunk");
        assert_eq!(tag.kind, ChunkKind::Method);
        assert_eq!(
            tag.breadcrumb.as_deref(),
            Some("Wrapper > when T : Printable > tag")
        );
        assert!(out.iter().any(|c| c.symbol.as_deref() == Some("show")));
    }

    #[test]
    fn every_annotation_prefix_rides_with_its_item() {
        let out = chunk(
            "@inline(always)\n@heat(cold)\nfn hot() : void {}\n\n@derive(Eq, Ord, Hashable)\nstruct P {\n    x : i32;\n}\n\n@layout(packed)\nextern \"C\" struct H {\n    m : u16;\n}\n",
        );
        assert_eq!(symbols(&out), vec!["hot", "P", "H"], "got {out:#?}");
        assert!(out[0].text.starts_with("@inline(always)\n@heat(cold)"));
        assert!(out[1].text.starts_with("@derive("));
        assert_eq!(out[2].kind, ChunkKind::Struct);
    }

    #[test]
    fn an_unterminated_string_still_yields_something() {
        let out = chunk("fn a() : string { return \"open;\n}\n");
        assert!(!out.is_empty());
    }
}
