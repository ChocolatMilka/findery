//! PHP chunker: one chunk per class, function or constant, one per method
//! inside a class big enough to be split.
//!
//! INTENT: heredocs and nowdocs are opaque to the scanner, so a SQL string or a
//! template holding `{`, `}` and `,` never counts as structure.

use crate::brace::{ItemMeta, Profile, chunk_with, keyword_header, split_trivia};
use crate::items::LineIndex;
use crate::scanner::Lexicon;
use findery_core::Result;
use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, Lang, RawChunk};
use findery_core::traits::Chunker;

/// Splits PHP into items.
pub struct PhpChunker;

impl Chunker for PhpChunker {
    fn lang(&self) -> Lang {
        Lang::Php
    }

    fn chunk(&self, path: &str, text: &str, cfg: &ChunkConfig) -> Result<Vec<RawChunk>> {
        chunk_with(&PhpProfile, path, text, cfg)
    }
}

/// The tokens that name something. Modifiers — `abstract`, `final`, `readonly`,
/// `public`, `static` — are simply skipped by the scan. `use` is left out on
/// purpose: a namespace import carries no name worth weighting, and staying
/// anonymous is what lets consecutive imports merge.
const PHP_KEYWORDS: &[(&str, ChunkKind)] = &[
    ("function", ChunkKind::Function),
    ("class", ChunkKind::Struct),
    ("interface", ChunkKind::Trait),
    ("trait", ChunkKind::Trait),
    ("enum", ChunkKind::Enum),
    ("const", ChunkKind::Const),
    ("namespace", ChunkKind::Module),
];

struct PhpProfile;

impl Profile for PhpProfile {
    fn lexicon(&self) -> &'static Lexicon {
        &Lexicon::PHP
    }

    fn classify(&self, item: &str, _nested: bool) -> ItemMeta {
        let (_, code) = split_trivia(item, self.lexicon());
        let header_end = code.find(['{', ';', '=']).unwrap_or(code.len());
        let (kind, symbol) = keyword_header(&code[..header_end], PHP_KEYWORDS);
        ItemMeta {
            kind,
            symbol,
            is_test: false,
        }
    }

    fn preamble(&self, src: &str, _path: &str, _index: &LineIndex) -> (usize, Option<RawChunk>) {
        // The opening tag is not code. It is skipped, not made a chunk: a
        // one-line `<?php` fragment is noise in the index.
        let trimmed = src.trim_start();
        let skipped = src.len() - trimmed.len();
        if !trimmed.starts_with("<?php") {
            return (0, None);
        }
        let after = skipped + "<?php".len();
        let line_end = src[after..]
            .find('\n')
            .map(|i| after + i + 1)
            .unwrap_or(src.len());
        // `<?php echo 1;` keeps its code; a tag alone on its line takes the
        // line with it.
        let end = if src[after..line_end].trim().is_empty() {
            line_end
        } else {
            after
        };
        (end, None)
    }

    fn recursable(&self, kind: ChunkKind) -> bool {
        matches!(
            kind,
            ChunkKind::Struct | ChunkKind::Trait | ChunkKind::Impl | ChunkKind::Module
        )
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
        PhpChunker
            .chunk("src/Http/Client.php", src, &cfg)
            .expect("chunking succeeds")
    }

    #[test]
    fn the_opening_tag_is_not_a_chunk() {
        let out = chunk("<?php\n\nfunction boot() {\n    return 1;\n}\n");
        assert!(!out.iter().any(|c| c.text.contains("<?php")), "got {out:?}");
        assert!(out.iter().any(|c| c.symbol.as_deref() == Some("boot")));
    }

    #[test]
    fn a_class_is_split_per_method_with_a_breadcrumb() {
        let filler = "    // padding padding padding padding padding padding\n".repeat(30);
        let src = format!(
            "<?php\n\nclass Client extends Base {{\n    public function send($r) {{\n{filler}    }}\n\
             \n    private function sign($r) {{\n{filler}    }}\n}}\n"
        );
        let out = chunk(&src);
        let send = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("send"))
            .expect("send");
        assert_eq!(send.kind, ChunkKind::Method);
        assert_eq!(send.breadcrumb.as_deref(), Some("Client > send"));
        assert!(out.iter().any(|c| c.symbol.as_deref() == Some("sign")));
    }

    #[test]
    fn a_braced_heredoc_does_not_close_the_function() {
        let src = "<?php\nfunction q() {\n    return <<<SQL\n    SELECT * FROM t WHERE x = '{ a }'\n    SQL;\n}\n\nfunction after() {}\n";
        let out = chunk(src);
        let names: Vec<_> = out.iter().filter_map(|c| c.symbol.clone()).collect();
        assert_eq!(
            names,
            vec!["q".to_string(), "after".to_string()],
            "got {out:?}"
        );
    }

    #[test]
    fn a_hash_comment_is_trivia_and_an_attribute_is_not_a_comment() {
        let out = chunk("<?php\n# a comment\n#[Route('/x')]\nfunction handle() {}\n");
        let h = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("handle"))
            .expect("handle");
        assert!(h.text.starts_with('#'));
        assert!(h.text.contains("#[Route"));
    }

    #[test]
    fn an_attribute_spanning_several_lines_stays_trivia() {
        let out =
            chunk("<?php\n#[Route(\n    '/x',\n    methods: ['GET'],\n)]\nfunction handle() {}\n");
        let h = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("handle"))
            .expect("handle");
        assert!(h.text.starts_with("#[Route"), "got {:?}", h.text);
        assert!(h.text.contains("methods"));
    }

    #[test]
    fn a_namespace_import_is_anonymous() {
        let out = chunk("<?php\nuse App\\Http\\Client;\n");
        assert_eq!(out[0].kind, ChunkKind::Text);
        assert!(out[0].symbol.is_none());
    }
}
