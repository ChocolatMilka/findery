//! The fallback chunker: the whole file while it fits, line windows after that.

use crate::sizing::{count_lines, split_windows};
use findery_core::Result;
use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, Lang, RawChunk};
use findery_core::traits::Chunker;

/// Line-window chunker used for `Other` languages and as the last resort when a
/// syntactic chunker produces a fragment that is still too large.
pub struct TextChunker;

impl Chunker for TextChunker {
    fn lang(&self) -> Lang {
        Lang::Other
    }

    fn chunk(&self, _path: &str, text: &str, cfg: &ChunkConfig) -> Result<Vec<RawChunk>> {
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        if text.len() <= cfg.max_chars {
            return Ok(vec![RawChunk {
                kind: ChunkKind::File,
                symbol: None,
                breadcrumb: None,
                start_line: 1,
                end_line: count_lines(text),
                text: text.to_string(),
            }]);
        }
        Ok(split_windows(text, 1, ChunkKind::Text, None, None, cfg))
    }
}
