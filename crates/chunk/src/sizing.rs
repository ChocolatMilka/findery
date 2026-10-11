//! The two post-passes every chunker shares: windowing a fragment that is too
//! big, merging neighbours that are too small.
//!
//! INTENT: both are size decisions taken AFTER a chunker has spoken, so no
//! language has to think about the budget while it splits.

use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, RawChunk};

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

/// Merges runs of undersized, **anonymous** neighbours into one chunk.
///
/// INTENT: merging is for fragments that carry no identity of their own — a run
/// of `use`, `import` or stray lines. A chunk that names something is never
/// folded away, because the merge has no way to give it back what it loses: it
/// would drop the `kind` a filter selects on and the symbol the full-text index
/// weights four times over, which is precisely what a search for an exact
/// identifier (`MAX_RETRIES`, `with_backoff`) relies on.
///
/// INVARIANT: every symbol a chunker produced survives `merge_small`.
pub fn merge_small(chunks: Vec<RawChunk>, cfg: &ChunkConfig) -> Vec<RawChunk> {
    let mut out: Vec<RawChunk> = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let Some(prev) = out.last_mut() else {
            out.push(chunk);
            continue;
        };
        let anonymous = prev.symbol.is_none() && chunk.symbol.is_none();
        let both_small = prev.text.len() < cfg.min_chars && chunk.text.len() < cfg.min_chars;
        let fits = prev.text.len() + chunk.text.len() <= cfg.max_chars;
        if anonymous && both_small && fits {
            if !prev.text.ends_with('\n') {
                prev.text.push('\n');
            }
            prev.text.push_str(&chunk.text);
            prev.end_line = chunk.end_line.max(prev.end_line);
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

    fn mk(n: usize, line: u32) -> RawChunk {
        RawChunk {
            kind: ChunkKind::Text,
            symbol: None,
            breadcrumb: None,
            start_line: line,
            end_line: line,
            text: "a".repeat(n),
        }
    }

    fn named(n: usize, line: u32, symbol: &str) -> RawChunk {
        RawChunk {
            symbol: Some(symbol.into()),
            breadcrumb: Some(symbol.into()),
            kind: ChunkKind::Const,
            ..mk(n, line)
        }
    }

    #[test]
    fn small_neighbours_merge_but_large_ones_do_not() {
        let merged = merge_small(vec![mk(5, 1), mk(5, 2), mk(30, 3)], &cfg());
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].text.len(), 11); // 5 + newline + 5
    }

    #[test]
    fn a_named_chunk_is_never_folded_into_its_neighbour() {
        // Two tiny `use` lines and a two-line `const MAX`: the constant keeps
        // its own chunk, its kind and its symbol — the exact identifier the
        // keyword leg exists to find.
        let merged = merge_small(vec![mk(5, 1), mk(5, 2), named(5, 3, "MAX")], &cfg());
        assert_eq!(merged.len(), 2);
        let max = merged.last().expect("kept");
        assert_eq!(max.symbol.as_deref(), Some("MAX"));
        assert_eq!(max.kind, ChunkKind::Const);
        assert_eq!(max.breadcrumb.as_deref(), Some("MAX"));
        assert_eq!(max.text, "aaaaa");
    }

    #[test]
    fn every_symbol_survives_the_merge() {
        let before = vec![
            named(5, 1, "MAX"),
            named(5, 2, "MIN"),
            mk(5, 3),
            mk(4, 4),
            named(9, 5, "render"),
        ];
        let after = merge_small(before.clone(), &cfg());
        let kept: Vec<&str> = after.iter().filter_map(|c| c.symbol.as_deref()).collect();
        assert_eq!(kept, vec!["MAX", "MIN", "render"], "got {after:?}");
    }

    #[test]
    fn blank_fragments_are_dropped() {
        let kept = drop_empty(vec![RawChunk {
            text: "}\n\n".into(),
            ..mk(0, 1)
        }]);
        assert!(kept.is_empty());
    }
}
