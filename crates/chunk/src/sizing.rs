//! The two post-passes every chunker shares: windowing a fragment that is too
//! big, merging neighbours that are too small.
//!
//! INTENT: both are size decisions taken AFTER a chunker has spoken, so no
//! language has to think about the budget while it splits.
//!
//! INVARIANT: no chunk is longer than `max_chars`, except by less than one
//! character when the budget is narrower than a single UTF-8 character.

use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, RawChunk};

/// How deep a chunker follows containers into containers before it windows
/// what is left as one leaf.
///
/// INTENT: a recursion whose depth the input controls is a stack the input
/// controls — and chunking runs inline in the daemon's sync, where a stack
/// overflow aborts the process. Real code nests a handful of containers; a
/// generated file of ten thousand keeps its text, windowed, and costs no more
/// than sixteen passes over itself. Not a tuning knob: no budget is better
/// served by a different value, so it is not in `ChunkConfig` (CONV-014).
pub(crate) const MAX_NESTING: usize = 16;

/// Number of lines in `text`, counting a trailing partial line.
pub fn count_lines(text: &str) -> u32 {
    let nl = text.bytes().filter(|b| *b == b'\n').count() as u32;
    if text.ends_with('\n') {
        nl.max(1)
    } else {
        nl + 1
    }
}

/// A piece of text a window is built from: a line, or a cut of a line too
/// long for one window.
struct Unit<'a> {
    text: &'a str,
    line: u32,
}

/// Splits `text` into overlapping windows, on line boundaries.
///
/// INTENT: cutting mid-line produces fragments that embed poorly and read worse
/// in an agent's context, so a window ends at a newline whenever one fits.
/// A line longer than the budget — a minified bundle, a generated table — is
/// cut anyway, on whitespace or punctuation when it can be: one megabyte line
/// sent to the embedder as one chunk is worse than any cut. Overlap keeps a
/// construct that straddles a boundary retrievable from either side.
pub fn split_windows(
    text: &str,
    start_line: u32,
    kind: ChunkKind,
    symbol: Option<String>,
    breadcrumb: Option<String>,
    cfg: &ChunkConfig,
) -> Vec<RawChunk> {
    let units = units(text, start_line, cfg.max_chars);
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < units.len() {
        let j = window_end(&units, i, cfg.max_chars);
        out.push(RawChunk {
            kind,
            symbol: symbol.clone(),
            breadcrumb: breadcrumb.clone(),
            start_line: units[i].line,
            end_line: units[j - 1].line,
            text: units[i..j].iter().map(|u| u.text).collect(),
        });
        if j >= units.len() {
            break;
        }
        i = j - overlap(&units[i..j], cfg.overlap_chars);
    }
    out
}

/// `chunk` itself while it fits the budget, its windows otherwise.
pub fn within_budget(chunk: RawChunk, cfg: &ChunkConfig) -> Vec<RawChunk> {
    if chunk.text.len() <= cfg.max_chars {
        return vec![chunk];
    }
    split_windows(
        &chunk.text,
        chunk.start_line,
        chunk.kind,
        chunk.symbol,
        chunk.breadcrumb,
        cfg,
    )
}

/// The lines of `text`, each one longer than `max_chars` cut into pieces.
fn units(text: &str, start_line: u32, max_chars: usize) -> Vec<Unit<'_>> {
    let mut out = Vec::new();
    for (k, line) in text.split_inclusive('\n').enumerate() {
        let line_no = start_line + k as u32;
        let mut rest = line;
        while rest.len() > max_chars {
            let cut = cut_point(rest, max_chars);
            out.push(Unit {
                text: &rest[..cut],
                line: line_no,
            });
            rest = &rest[cut..];
        }
        if !rest.is_empty() {
            out.push(Unit {
                text: rest,
                line: line_no,
            });
        }
    }
    out
}

/// Where to cut a line too long for one window: after its last break that
/// fits, otherwise at the last character boundary that fits — and never
/// before the first character.
///
/// INTENT: a word is what a search looks for, so a cut keeps words whole
/// whenever it can: `axle_unhandled_exception` is one word, and cutting it at
/// an `_` lost it. A short piece costs nothing, since pieces are packed back
/// into windows, so only a word longer than the whole budget is ever cut.
fn cut_point(line: &str, max_chars: usize) -> usize {
    let mut limit = max_chars.min(line.len());
    while !line.is_char_boundary(limit) {
        limit -= 1;
    }
    if limit == 0 {
        return line.chars().next().map_or(line.len(), char::len_utf8);
    }
    let ends_between_words = line[limit..].starts_with(is_break);
    let last_break = line[..limit]
        .char_indices()
        .rev()
        .find(|(_, c)| is_break(*c))
        .map(|(at, c)| at + c.len_utf8());
    match (ends_between_words, last_break) {
        (true, _) | (false, None) => limit,
        (false, Some(cut)) => cut,
    }
}

/// A character no word holds: words are what the coverage invariant counts,
/// letters, digits and `_`.
fn is_break(c: char) -> bool {
    !(c.is_alphanumeric() || c == '_')
}

/// The unit just past the window that starts at `i`: as many units as fit,
/// and always at least one.
fn window_end(units: &[Unit<'_>], i: usize, max_chars: usize) -> usize {
    let mut len = 0usize;
    let mut j = i;
    while j < units.len() && (len + units[j].text.len() <= max_chars || j == i) {
        len += units[j].text.len();
        j += 1;
    }
    j
}

/// How many units at the end of `window` the next one repeats: enough to cover
/// `overlap_chars`, and never the whole window, so a window always advances.
fn overlap(window: &[Unit<'_>], overlap_chars: usize) -> usize {
    let mut back = 0usize;
    let mut acc = 0usize;
    for unit in window.iter().skip(1).rev() {
        if acc + unit.text.len() > overlap_chars {
            break;
        }
        acc += unit.text.len();
        back += 1;
    }
    back
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
/// INVARIANT: every symbol a chunker produced survives `merge_small`, and a
/// merged chunk fits the budget, the newline it gains included.
pub fn merge_small(chunks: Vec<RawChunk>, cfg: &ChunkConfig) -> Vec<RawChunk> {
    let mut out: Vec<RawChunk> = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let Some(prev) = out.last_mut() else {
            out.push(chunk);
            continue;
        };
        let anonymous = prev.symbol.is_none() && chunk.symbol.is_none();
        let both_small = prev.text.len() < cfg.min_chars && chunk.text.len() < cfg.min_chars;
        let separator = usize::from(!prev.text.ends_with('\n'));
        let fits = prev.text.len() + separator + chunk.text.len() <= cfg.max_chars;
        if anonymous && both_small && fits {
            if separator == 1 {
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

/// Drops fragments that carry no word: blank, or a lone brace.
///
/// INTENT: a word is what the coverage invariant counts and what a search can
/// look for, `_` included — `_ => {}` is one, and dropping it lost it.
pub fn drop_empty(chunks: Vec<RawChunk>) -> Vec<RawChunk> {
    chunks
        .into_iter()
        .filter(|c| c.text.chars().any(|ch| ch.is_alphanumeric() || ch == '_'))
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

    fn windows(text: &str) -> Vec<RawChunk> {
        split_windows(text, 1, ChunkKind::Text, None, None, &cfg())
    }

    #[test]
    fn windows_never_cut_mid_line_and_always_advance() {
        let text = (1..=20).map(|i| format!("line {i}\n")).collect::<String>();
        let parts = windows(&text);
        assert!(parts.len() > 1);
        for p in &parts {
            assert!(p.text.ends_with('\n'));
            assert!(p.end_line >= p.start_line);
        }
        assert_eq!(parts[0].start_line, 1);
        assert!(parts.last().expect("non empty").end_line >= 20);
    }

    #[test]
    fn a_line_longer_than_the_budget_is_cut_into_pieces_that_fit() {
        let text = "x".repeat(200);
        let parts = windows(&text);
        assert!(parts.len() >= 5, "got {parts:?}");
        assert!(parts.iter().all(|p| p.text.len() <= 40));
        assert!(parts.iter().all(|p| (p.start_line, p.end_line) == (1, 1)));
        assert_eq!(parts.iter().map(|p| p.text.len()).sum::<usize>(), 200);
    }

    #[test]
    fn an_identifier_is_not_cut_at_its_underscores() {
        let text = "`axle_take_exception`, `axle_unhandled_exception`, `axle_panic_oob`\n";
        for p in windows(text) {
            for word in p.text.split(is_break).filter(|w| !w.is_empty()) {
                assert!(
                    text.contains(&format!("`{word}`")),
                    "`{word}` was cut: {p:?}"
                );
            }
        }
    }

    #[test]
    fn a_long_line_is_cut_between_words_when_it_can_be() {
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa;\n";
        let parts = windows(text);
        assert!(parts.len() > 1, "got {parts:?}");
        assert!(parts[0].text.ends_with(' '), "got {parts:?}");
        assert!(parts.iter().all(|p| p.text.len() <= 40));
        assert_eq!(
            parts.iter().map(|p| p.text.as_str()).collect::<String>(),
            text
        );
    }

    #[test]
    fn a_cut_never_splits_a_character() {
        let text = "é".repeat(100);
        for p in windows(&text) {
            assert!(p.text.len() <= 40, "got {p:?}");
            assert!(p.text.chars().all(|c| c == 'é'));
        }
        // A budget narrower than one character still moves forward.
        let tiny = ChunkConfig {
            max_chars: 1,
            ..cfg()
        };
        let parts = split_windows("€€", 1, ChunkKind::Text, None, None, &tiny);
        assert_eq!(parts.len(), 2, "got {parts:?}");
    }

    #[test]
    fn a_chunk_over_the_budget_is_windowed_and_one_within_it_is_kept() {
        let big = RawChunk {
            text: "a word\n".repeat(20),
            ..mk(0, 1)
        };
        let parts = within_budget(big, &cfg());
        assert!(parts.len() > 1);
        assert!(parts.iter().all(|p| p.text.len() <= 40));
        assert_eq!(within_budget(mk(10, 3), &cfg()), vec![mk(10, 3)]);
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
    fn a_merge_counts_the_newline_it_inserts() {
        // 20 + 20 fits 40 only without the newline between them.
        let tight = ChunkConfig {
            min_chars: 30,
            ..cfg()
        };
        let merged = merge_small(vec![mk(20, 1), mk(20, 2)], &tight);
        assert_eq!(merged.len(), 2, "got {merged:?}");
        assert!(merged.iter().all(|c| c.text.len() <= 40));
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

    #[test]
    fn a_fragment_whose_only_word_is_an_underscore_is_kept() {
        let kept = drop_empty(vec![RawChunk {
            text: "_ => {}".into(),
            ..mk(0, 1)
        }]);
        assert_eq!(kept.len(), 1);
    }
}
