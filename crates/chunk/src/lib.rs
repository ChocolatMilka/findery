//! # findery-chunk — layer L1
//!
//! Splits a source file into embeddable fragments. Pure, deterministic, no I/O:
//! the whole incremental sync rests on `chunk(text) == chunk(text)` forever.
//!
//! One chunker per language, selected by [`Registry`]. Adding a language means
//! adding a [`findery_core::traits::Chunker`] and one line in the registry — nothing
//! else in the workspace changes.

pub mod markdown;
pub mod rust;
pub mod scanner;
pub mod text;

use findery_core::Result;
use findery_core::config::ChunkConfig;
use findery_core::model::{Lang, RawChunk};
use findery_core::traits::Chunker;

pub use markdown::MarkdownChunker;
pub use rust::RustChunker;
pub use text::TextChunker;

/// Routes a file to the chunker registered for its language.
pub struct Registry {
    rust: RustChunker,
    markdown: MarkdownChunker,
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
            fallback: TextChunker,
        }
    }

    /// The chunker that will handle `lang`.
    pub fn chunker_for(&self, lang: Lang) -> &dyn Chunker {
        match lang {
            Lang::Rust => &self.rust,
            Lang::Markdown => &self.markdown,
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

    #[test]
    fn registry_routes_by_language() {
        let r = Registry::new();
        assert_eq!(r.chunker_for(Lang::Rust).lang(), Lang::Rust);
        assert_eq!(r.chunker_for(Lang::Markdown).lang(), Lang::Markdown);
        assert_eq!(r.chunker_for(Lang::Toml).lang(), Lang::Other);
    }

    #[test]
    fn every_chunk_has_sane_bounds_and_non_empty_text() {
        let r = Registry::new();
        let cfg = ChunkConfig::default();
        let src = include_str!("lib.rs");
        for lang in [Lang::Rust, Lang::Markdown, Lang::Other] {
            let chunks = r
                .chunk("src/lib.rs", lang, src, &cfg)
                .expect("chunking succeeds");
            for c in &chunks {
                assert!(c.start_line >= 1, "{lang:?}: {c:?}");
                assert!(c.end_line >= c.start_line, "{lang:?}: {c:?}");
                assert!(!c.text.trim().is_empty(), "{lang:?}: {c:?}");
            }
        }
    }

    #[test]
    fn chunking_is_deterministic() {
        let r = Registry::new();
        let cfg = ChunkConfig::default();
        let src = include_str!("rust.rs");
        let a = r.chunk("a.rs", Lang::Rust, src, &cfg).expect("first pass");
        let b = r.chunk("a.rs", Lang::Rust, src, &cfg).expect("second pass");
        assert_eq!(a, b);
    }

    #[test]
    fn empty_input_yields_no_chunks() {
        let r = Registry::new();
        let cfg = ChunkConfig::default();
        for lang in [Lang::Rust, Lang::Markdown, Lang::Other] {
            assert!(
                r.chunk("x", lang, "   \n\n", &cfg)
                    .expect("chunking succeeds")
                    .is_empty()
            );
        }
    }
}
