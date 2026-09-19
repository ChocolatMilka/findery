//! Generic fallback chunker plus the two post-passes every chunker shares:
//! window splitting for oversized fragments and merging for undersized ones.

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

/// Number of lines in `text`, counting a trailing partial line.
pub fn count_lines(text: &str) -> u32 {
    let nl = text.bytes().filter(|b| *b == b'\n').count() as u32;
    if text.ends_with('\n') {
        nl.max(1)
    } else {
        nl + 1
    }
}

/// Splits `text` into overlapping windows, always on line boundaries.
///
/// INTENT: cutting mid-line produces fragments that embed poorly and read worse
/// in an agent's context. Overlap keeps a construct that straddles a boundary
/// retrievable from either side.
pub fn split_windows(
    text: &str,
    start_line: u32,
    kind: ChunkKind,
    symbol: Option<String>,
    breadcrumb: Option<String>,
    cfg: &ChunkConfig,
) -> Vec<RawChunk> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let mut len = 0usize;
        let mut j = i;
        while j < lines.len() && (len + lines[j].len() <= cfg.max_chars || j == i) {
            len += lines[j].len();
            j += 1;
        }
        let body: String = lines[i..j].concat();
        out.push(RawChunk {
            kind,
            symbol: symbol.clone(),
            breadcrumb: breadcrumb.clone(),
            start_line: start_line + i as u32,
            end_line: start_line + (j - 1) as u32,
            text: body,
        });
        if j >= lines.len() {
            break;
        }
        // Step back far enough to cover `overlap_chars`, but always make
        // forward progress.
        let mut back = 0usize;
        let mut acc = 0usize;
        while back < j - i - 1 && acc + lines[j - 1 - back].len() <= cfg.overlap_chars {
            acc += lines[j - 1 - back].len();
            back += 1;
        }
        i = j - back;
    }
    out
}

/// Merges runs of undersized neighbours into one chunk.
///
/// A three-line `impl Display` embedded alone is noise; folded in with the
/// `use` block and the type it formats, it is a usable fragment.
pub fn merge_small(chunks: Vec<RawChunk>, cfg: &ChunkConfig) -> Vec<RawChunk> {
    let mut out: Vec<RawChunk> = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let Some(prev) = out.last_mut() else {
            out.push(chunk);
            continue;
        };
        let both_small = prev.text.len() < cfg.min_chars && chunk.text.len() < cfg.min_chars;
        let fits = prev.text.len() + chunk.text.len() <= cfg.max_chars;
        let contiguous = chunk.start_line >= prev.start_line;
        if both_small && fits && contiguous {
            if !prev.text.ends_with('\n') {
                prev.text.push('\n');
            }
            prev.text.push_str(&chunk.text);
            prev.end_line = chunk.end_line.max(prev.end_line);
            prev.kind = ChunkKind::Text;
            prev.symbol = None;
            prev.breadcrumb = None;
        } else {
            out.push(chunk);
        }
    }
    out
}

/// Drops fragments that carry no information (blank, or a lone brace).
pub fn drop_empty(chunks: Vec<RawChunk>) -> Vec<RawChunk> {
    chunks
        .into_iter()
        .filter(|c| c.text.chars().any(|ch| ch.is_alphanumeric()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ChunkConfig {
        ChunkConfig {
            max_chars: 40,
            min_chars: 15,
            overlap_chars: 10,
            include_tests: true,
        }
    }

    #[test]
    fn windows_never_cut_mid_line_and_always_advance() {
        let text = (1..=20).map(|i| format!("line {i}\n")).collect::<String>();
        let parts = split_windows(&text, 1, ChunkKind::Text, None, None, &cfg());
        assert!(parts.len() > 1);
        for p in &parts {
            assert!(p.text.ends_with('\n'));
            assert!(p.end_line >= p.start_line);
        }
        assert_eq!(parts[0].start_line, 1);
        assert!(parts.last().expect("non empty").end_line >= 20);
    }

    #[test]
    fn a_single_oversized_line_still_yields_one_chunk() {
        let text = "x".repeat(200);
        let parts = split_windows(&text, 1, ChunkKind::Text, None, None, &cfg());
        assert_eq!(parts.len(), 1);
    }

    #[test]
    fn small_neighbours_merge_but_large_ones_do_not() {
        let mk = |n: usize, line: u32| RawChunk {
            kind: ChunkKind::Const,
            symbol: None,
            breadcrumb: None,
            start_line: line,
            end_line: line,
            text: "a".repeat(n),
        };
        let merged = merge_small(vec![mk(5, 1), mk(5, 2), mk(30, 3)], &cfg());
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].text.len(), 11); // 5 + newline + 5
    }

    #[test]
    fn blank_fragments_are_dropped() {
        let kept = drop_empty(vec![RawChunk {
            kind: ChunkKind::Text,
            symbol: None,
            breadcrumb: None,
            start_line: 1,
            end_line: 1,
            text: "}\n\n".into(),
        }]);
        assert!(kept.is_empty());
    }
}
