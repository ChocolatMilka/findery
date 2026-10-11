//! JavaScript and TypeScript chunker: one chunk per top-level declaration, one
//! per member inside a class big enough to be split.
//!
//! INTENT: `.js` and `.ts` share a lexical grammar — types are erased before a
//! brace is ever counted, so one chunker serves both. `.jsx` and `.tsx` are
//! deliberately NOT routed here: JSX text may contain an apostrophe
//! (`<p>don't</p>`), which a lexical scanner reads as the opening of a string
//! and lets run across the markup. They take the text fallback instead.

use crate::brace::{ItemMeta, Profile, chunk_with, keyword_header, split_trivia};
use crate::items::LineIndex;
use crate::scanner::Lexicon;
use findery_core::Result;
use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, Lang, RawChunk};
use findery_core::traits::Chunker;

/// Splits JavaScript and TypeScript into items.
pub struct JsChunker;

impl Chunker for JsChunker {
    fn lang(&self) -> Lang {
        Lang::JavaScript
    }

    fn chunk(&self, path: &str, text: &str, cfg: &ChunkConfig) -> Result<Vec<RawChunk>> {
        chunk_with(&JsProfile, path, text, cfg)
    }
}

/// The tokens that name something at the top level. Modifiers — `export`,
/// `default`, `async`, `declare`, `abstract` — are simply skipped by the scan.
const JS_KEYWORDS: &[(&str, ChunkKind)] = &[
    ("function", ChunkKind::Function),
    ("class", ChunkKind::Struct),
    ("const", ChunkKind::Const),
    ("let", ChunkKind::Const),
    ("var", ChunkKind::Const),
    ("enum", ChunkKind::Enum),
    ("interface", ChunkKind::Trait),
    ("type", ChunkKind::Const),
    ("namespace", ChunkKind::Module),
];

/// The modifiers that may precede a member's name inside a class body.
const MEMBER_MODIFIERS: &[&str] = &[
    "static",
    "async",
    "public",
    "private",
    "protected",
    "readonly",
    "abstract",
    "override",
    "declare",
    "get",
    "set",
];

struct JsProfile;

impl Profile for JsProfile {
    fn lexicon(&self) -> &'static Lexicon {
        &Lexicon::JAVASCRIPT
    }

    fn classify(&self, item: &str, nested: bool) -> ItemMeta {
        let (_, code) = split_trivia(item, self.lexicon());
        let header_end = code.find(['{', ';', '=']).unwrap_or(code.len());
        let header = &code[..header_end];
        let (kind, symbol) = keyword_header(header, JS_KEYWORDS);
        if kind != ChunkKind::Text {
            return ItemMeta {
                kind,
                symbol,
                is_test: false,
            };
        }
        // A class member has no keyword of its own, so its name is read from
        // the call shape of its header. Only INSIDE a container: a top-level
        // `doSomething();` is a statement, not a method of anything.
        if nested && let Some(name) = member_symbol(header) {
            return ItemMeta {
                kind: ChunkKind::Method,
                symbol: Some(name),
                is_test: false,
            };
        }
        ItemMeta {
            kind: ChunkKind::Text,
            symbol: None,
            is_test: false,
        }
    }

    fn preamble(&self, src: &str, _path: &str, _index: &LineIndex) -> (usize, Option<RawChunk>) {
        // A shebang is an instruction to the OS, not a chunk.
        let Some(rest) = src.strip_prefix("#!") else {
            return (0, None);
        };
        let end = rest.find('\n').map(|i| i + 3).unwrap_or(src.len());
        (end, None)
    }

    fn recursable(&self, kind: ChunkKind) -> bool {
        matches!(
            kind,
            ChunkKind::Struct | ChunkKind::Trait | ChunkKind::Module | ChunkKind::Impl
        )
    }
}

/// The name of a class member, which has no keyword to announce it:
/// `render(props) {`, `static async boot() {`.
fn member_symbol(header: &str) -> Option<String> {
    let mut rest = header.trim();
    loop {
        let before = rest;
        for modifier in MEMBER_MODIFIERS {
            if let Some(after) = rest.strip_prefix(modifier)
                && after.starts_with([' ', '(', '<'])
            {
                rest = after.trim_start();
            }
        }
        if rest == before {
            break;
        }
    }
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
        .collect();
    if name.is_empty() {
        return None;
    }
    let after = rest[name.len()..].trim_start();
    // A call-like header names a member; anything else is a field or a stray.
    if after.starts_with('(') || after.starts_with('<') {
        Some(name)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(src: &str) -> Vec<RawChunk> {
        let cfg = ChunkConfig {
            min_chars: 1,
            ..ChunkConfig::default()
        };
        JsChunker
            .chunk("src/app.js", src, &cfg)
            .expect("chunking succeeds")
    }

    #[test]
    fn a_modifier_before_the_keyword_does_not_hide_the_name() {
        let out = chunk("export default async function boot() {\n  return 1;\n}\n");
        let f = out
            .iter()
            .find(|c| c.kind == ChunkKind::Function)
            .expect("fn");
        assert_eq!(f.symbol.as_deref(), Some("boot"));
    }

    #[test]
    fn a_class_is_struct_and_a_class_member_is_a_method() {
        let filler = "    // padding padding padding padding padding padding\n".repeat(30);
        let src = format!(
            "class Widget extends Base {{\n  render(props) {{\n{filler}  }}\n\
             \n  static async boot() {{\n{filler}  }}\n}}\n"
        );
        let out = chunk(&src);
        let render = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("render"))
            .expect("render");
        assert_eq!(render.kind, ChunkKind::Method);
        assert_eq!(render.breadcrumb.as_deref(), Some("Widget > render"));
        assert!(out.iter().any(|c| c.symbol.as_deref() == Some("boot")));
    }

    #[test]
    fn a_brace_inside_a_regex_or_a_template_is_not_structural() {
        let src = "const re = /^a{2}$/;\n\nconst t = `x ${ {a: 1} } y`;\n\nfunction after() {}\n";
        let out = chunk(src);
        let names: Vec<_> = out.iter().filter_map(|c| c.symbol.clone()).collect();
        assert_eq!(
            names,
            vec!["re".to_string(), "t".to_string(), "after".to_string()]
        );
    }

    #[test]
    fn an_arrow_function_keeps_its_binding_name() {
        let out = chunk("const withBackoff = (fn, n) => {\n  return n;\n};\n");
        let c = out.first().expect("chunk");
        assert_eq!(c.kind, ChunkKind::Const);
        assert_eq!(c.symbol.as_deref(), Some("withBackoff"));
    }

    #[test]
    fn an_import_is_an_anonymous_fragment() {
        let out = chunk("import { readFile } from 'node:fs';\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, ChunkKind::Text);
        assert!(out[0].symbol.is_none());
    }

    #[test]
    fn a_decorated_class_keeps_its_name_and_its_decorator() {
        let src = "@Component({\n  selector: 'app-widget',\n})\nexport class Widget {\n  render() {}\n}\n";
        let out = chunk(src);
        let w = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("Widget"))
            .expect("Widget found");
        assert_eq!(w.kind, ChunkKind::Struct);
        assert!(w.text.starts_with("@Component"), "got {:?}", w.text);
    }

    #[test]
    fn a_decorator_without_arguments_keeps_its_class() {
        let out = chunk("@sealed class Foo { bar() {} }\n");
        let f = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("Foo"))
            .expect("Foo found");
        assert_eq!(f.kind, ChunkKind::Struct);
    }

    #[test]
    fn a_one_line_decorator_keeps_its_class() {
        let out = chunk("@Component() class Foo { bar() {} }\n");
        let f = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("Foo"))
            .expect("Foo found");
        assert_eq!(f.kind, ChunkKind::Struct);
    }

    #[test]
    fn a_top_level_call_is_not_a_method_of_anything() {
        let out = chunk("boot();\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, ChunkKind::Text);
        assert!(out[0].symbol.is_none(), "got {out:?}");
    }

    #[test]
    fn a_shebang_is_not_a_chunk() {
        let out = chunk("#!/usr/bin/env node\nconst x = 1;\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].symbol.as_deref(), Some("x"));
        assert!(!out[0].text.contains("#!"));
    }
}
