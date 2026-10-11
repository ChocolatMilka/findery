//! # findery-chunk — layer L1
//!
//! Splits a source file into embeddable fragments. Pure, deterministic, no I/O:
//! the whole incremental sync rests on `chunk(text) == chunk(text)` forever.
//!
//! One chunker per language, selected by [`Registry`]. Adding a language means
//! adding a [`findery_core::traits::Chunker`] and one line in the registry — nothing
//! else in the workspace changes.
//!
//! Two families share the work. The brace-delimited languages (Rust,
//! JavaScript, PHP, JSON) differ only in their trivia and their keywords, so
//! they run the SAME pipeline over one [`scanner::Lexicon`] each; Python, whose
//! blocks are indentation and not braces, has its own.

mod brace;
mod items;
pub mod langs;
mod scanner;
pub mod sizing;

pub use langs::{
    JsChunker, JsonChunker, MarkdownChunker, PhpChunker, PythonChunker, RustChunker, TextChunker,
};

use findery_core::Result;
use findery_core::config::ChunkConfig;
use findery_core::model::{Lang, RawChunk};
use findery_core::traits::Chunker;

/// Routes a file to the chunker registered for its language.
pub struct Registry {
    rust: RustChunker,
    markdown: MarkdownChunker,
    javascript: JsChunker,
    python: PythonChunker,
    php: PhpChunker,
    json: JsonChunker,
    fallback: TextChunker,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self {
            rust: RustChunker,
            markdown: MarkdownChunker,
            javascript: JsChunker,
            python: PythonChunker,
            php: PhpChunker,
            json: JsonChunker,
            fallback: TextChunker,
        }
    }

    /// The chunker that will handle `lang`.
    pub fn chunker_for(&self, lang: Lang) -> &dyn Chunker {
        match lang {
            Lang::Rust => &self.rust,
            Lang::Markdown => &self.markdown,
            Lang::JavaScript => &self.javascript,
            Lang::Python => &self.python,
            Lang::Php => &self.php,
            Lang::Json => &self.json,
            Lang::Toml | Lang::Other => &self.fallback,
        }
    }

    /// Splits one file. `lang` is passed explicitly rather than re-derived from
    /// `path`, so a caller can force a chunker for an inline document.
    pub fn chunk(
        &self,
        path: &str,
        lang: Lang,
        text: &str,
        cfg: &ChunkConfig,
    ) -> Result<Vec<RawChunk>> {
        self.chunker_for(lang).chunk(path, text, cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every language the registry routes, so a new one cannot be added without
    /// being exercised below.
    const ALL: &[Lang] = &[
        Lang::Rust,
        Lang::Markdown,
        Lang::Toml,
        Lang::JavaScript,
        Lang::Python,
        Lang::Php,
        Lang::Json,
        Lang::Other,
    ];

    #[test]
    fn registry_routes_by_language() {
        let r = Registry::new();
        assert_eq!(r.chunker_for(Lang::Rust).lang(), Lang::Rust);
        assert_eq!(r.chunker_for(Lang::Markdown).lang(), Lang::Markdown);
        assert_eq!(r.chunker_for(Lang::JavaScript).lang(), Lang::JavaScript);
        assert_eq!(r.chunker_for(Lang::Python).lang(), Lang::Python);
        assert_eq!(r.chunker_for(Lang::Php).lang(), Lang::Php);
        assert_eq!(r.chunker_for(Lang::Json).lang(), Lang::Json);
        assert_eq!(r.chunker_for(Lang::Toml).lang(), Lang::Other);
        assert_eq!(r.chunker_for(Lang::Other).lang(), Lang::Other);
    }

    #[test]
    fn every_language_round_trips_through_its_string_form() {
        for lang in ALL {
            assert_eq!(Lang::parse(lang.as_str()), Some(*lang), "{lang:?}");
        }
    }

    #[test]
    fn every_chunk_has_sane_bounds_and_non_empty_text() {
        let r = Registry::new();
        let cfg = ChunkConfig::default();
        let src = include_str!("lib.rs");
        for lang in ALL {
            let chunks = r
                .chunk("src/lib.rs", *lang, src, &cfg)
                .expect("chunking succeeds");
            for c in &chunks {
                assert!(c.start_line >= 1, "{lang:?}: {c:?}");
                assert!(c.end_line >= c.start_line, "{lang:?}: {c:?}");
                assert!(!c.text.trim().is_empty(), "{lang:?}: {c:?}");
                assert!(
                    c.end_line <= src.lines().count() as u32 + 1,
                    "{lang:?}: {c:?}"
                );
            }
        }
    }

    #[test]
    fn chunking_is_deterministic() {
        let r = Registry::new();
        let cfg = ChunkConfig::default();
        let src = include_str!("langs/rust.rs");
        let a = r.chunk("a.rs", Lang::Rust, src, &cfg).expect("first pass");
        let b = r.chunk("a.rs", Lang::Rust, src, &cfg).expect("second pass");
        assert_eq!(a, b);
    }

    #[test]
    fn every_symbol_a_chunker_produces_survives_the_merge() {
        let r = Registry::new();
        let cfg = ChunkConfig::default();
        // A small file of anonymous fragments — imports, uses — is exactly what
        // merging folds together, and exactly where a named item used to
        // disappear from the keyword index.
        let sources: &[(Lang, &str)] = &[
            (
                Lang::Rust,
                "use std::io;\nuse std::fmt;\n\nconst MAX: usize = 4;\n\nfn a() {}\n",
            ),
            (
                Lang::JavaScript,
                "import a from 'a';\nimport b from 'b';\n\nconst MAX = 4;\n",
            ),
            (Lang::Python, "import os\nimport sys\n\nMAX_RETRIES = 4\n"),
        ];
        for (lang, src) in sources {
            let chunks = r.chunk("x", *lang, src, &cfg).expect("chunking succeeds");
            let symbols: Vec<Option<String>> = chunks.iter().map(|c| c.symbol.clone()).collect();
            match lang {
                Lang::Rust => assert!(symbols.contains(&Some("MAX".into())), "{chunks:?}"),
                Lang::JavaScript => assert!(symbols.contains(&Some("MAX".into())), "{chunks:?}"),
                Lang::Python => {
                    assert!(symbols.contains(&Some("MAX_RETRIES".into())), "{chunks:?}")
                }
                _ => {}
            }
        }
    }

    /// The error path of a chunker is not an `Err`: nothing a chunker does can
    /// fail, by construction — it reads no file, opens no socket and has no
    /// state to be inconsistent (CONV-025). Its degenerate inputs are empty,
    /// blank, truncated and syntactically broken text, which must yield either
    /// nothing or sane fragments. `crates/chunk/tests/languages.rs` walks every
    /// prefix of a realistic file per language for exactly that.
    #[test]
    fn empty_input_yields_no_chunks() {
        let r = Registry::new();
        let cfg = ChunkConfig::default();
        for lang in ALL {
            assert!(
                r.chunk("x", *lang, "   \n\n", &cfg)
                    .expect("chunking succeeds")
                    .is_empty(),
                "{lang:?}"
            );
        }
    }
}
