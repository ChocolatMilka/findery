//! JSON chunker: the whole file while it fits, one chunk per top-level member
//! once it does not.
//!
//! INTENT: a JSON file is one document — `package.json` answered in one chunk
//! is worth more than its five sections answered separately. Only a file too
//! big for the budget becomes members, and then the KEY is the symbol, so
//! "dependencies" or "scripts" is found by name and not only by vector.

use crate::items::{LineIndex, Span};
use crate::scanner::{Lexicon, skip_opaque};
use crate::text::{drop_empty, merge_small, split_windows};
use findery_core::Result;
use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, Lang, RawChunk};
use findery_core::traits::Chunker;

/// How far the recursion goes before a member is windowed instead. A JSON
/// document nested deeper than this is data, not structure worth naming.
const MAX_DEPTH: u32 = 3;

/// Splits JSON into members.
pub struct JsonChunker;

impl Chunker for JsonChunker {
    fn lang(&self) -> Lang {
        Lang::Json
    }

    fn chunk(&self, _path: &str, text: &str, cfg: &ChunkConfig) -> Result<Vec<RawChunk>> {
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        let index = LineIndex::new(text);
        let mut out = Vec::new();
        emit(
            text,
            Span {
                start: 0,
                end: text.len(),
            },
            None,
            &index,
            cfg,
            0,
            &mut out,
        );
        Ok(merge_small(drop_empty(out), cfg))
    }
}

/// Emits one span, splitting it into members while it is too big to keep.
fn emit(
    src: &str,
    span: Span,
    key: Option<String>,
    index: &LineIndex,
    cfg: &ChunkConfig,
    depth: u32,
    out: &mut Vec<RawChunk>,
) {
    let raw = span.slice(src);
    let text = raw.trim();
    if text.is_empty() {
        return;
    }
    let lead = raw.len() - raw.trim_start().len();
    let start_line = index.line_of(span.start + lead);

    if text.len() > cfg.max_chars
        && depth < MAX_DEPTH
        && let Some(members) = members(src, span)
        && members.len() > 1
    {
        for (member, member_key) in members {
            emit(src, member, member_key, index, cfg, depth + 1, out);
        }
        return;
    }

    let kind = if depth == 0 {
        ChunkKind::File
    } else {
        ChunkKind::Text
    };
    if text.len() <= cfg.max_chars {
        out.push(RawChunk {
            kind,
            symbol: key.clone(),
            breadcrumb: key,
            start_line,
            end_line: index.line_of(span.end.saturating_sub(1)),
            text: text.to_string(),
        });
    } else {
        out.extend(split_windows(text, start_line, kind, key.clone(), key, cfg));
    }
}

/// The top-level members of the container in `span`, with the key of each when
/// the container is an object.
fn members(src: &str, span: Span) -> Option<Vec<(Span, Option<String>)>> {
    let bytes = src.as_bytes();
    let mut i = span.start;
    let mut root = false;
    let mut object = false;
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut out = Vec::new();
    while i < span.end {
        if let Some(next) = skip_opaque(bytes, i, &Lexicon::JSON) {
            i = next.min(span.end);
            continue;
        }
        match bytes[i] {
            b'{' | b'[' => {
                if !root {
                    root = true;
                    object = bytes[i] == b'{';
                    start = i + 1;
                }
                depth += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 && root {
                    push_member(&mut out, src, start, i, object);
                    return Some(out);
                }
            }
            b',' if depth == 1 && root => {
                push_member(&mut out, src, start, i, object);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    // No container was ever closed: an unterminated file is not split.
    None
}

fn push_member(
    out: &mut Vec<(Span, Option<String>)>,
    src: &str,
    from: usize,
    to: usize,
    object: bool,
) {
    let mut start = from;
    while start < to && src.as_bytes()[start].is_ascii_whitespace() {
        start += 1;
    }
    if start >= to {
        return;
    }
    let key = if object {
        member_key(&src[start..to])
    } else {
        None
    };
    out.push((Span { start, end: to }, key));
}

/// The key of an object member: the string before its `:`.
fn member_key(member: &str) -> Option<String> {
    let rest = member.trim_start().strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next()?),
            '"' => return Some(out),
            _ => out.push(c),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(src: &str) -> Vec<RawChunk> {
        chunk_with(src, &ChunkConfig::default())
    }

    /// A budget tight enough to force the split, without any overlap.
    fn chunk_with(src: &str, cfg: &ChunkConfig) -> Vec<RawChunk> {
        JsonChunker
            .chunk("package.json", src, cfg)
            .expect("chunking succeeds")
    }

    fn under(max_chars: usize, src: &str) -> Vec<RawChunk> {
        chunk_with(
            src,
            &ChunkConfig {
                max_chars,
                min_chars: 1,
                overlap_chars: 0,
                include_tests: true,
            },
        )
    }

    #[test]
    fn a_small_document_stays_whole() {
        let out = chunk("{\n  \"name\": \"demo\",\n  \"version\": \"1.0.0\"\n}\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, ChunkKind::File);
        assert!(out[0].text.contains("version"));
    }

    #[test]
    fn an_oversized_document_splits_per_member_and_carries_the_key() {
        let src = "{\n  \"dependencies\": {\n    \"a\": \"1.0.0\",\n    \"b\": \"2.0.0\"\n  },\n  \"scripts\": {\n    \"build\": \"cargo build\",\n    \"test\": \"cargo test\"\n  }\n}\n";
        let out = under(120, src);
        let keys: Vec<_> = out.iter().filter_map(|c| c.symbol.clone()).collect();
        assert_eq!(
            keys,
            vec!["dependencies".to_string(), "scripts".to_string()],
            "got {out:?}"
        );
        assert_eq!(out[0].breadcrumb.as_deref(), Some("dependencies"));
        assert!(out[0].text.contains("\"b\": \"2.0.0\""));
    }

    #[test]
    fn a_comma_inside_a_string_is_not_a_separator() {
        let src = "{\n  \"a\": \"x, y\",\n  \"b\": \"z\"\n}\n";
        let out = under(26, src);
        let keys: Vec<_> = out.iter().filter_map(|c| c.symbol.clone()).collect();
        assert_eq!(keys, vec!["a".to_string(), "b".to_string()], "got {out:?}");
    }

    #[test]
    fn an_array_of_records_splits_per_element() {
        let src = "[\n  {\"id\": 1},\n  {\"id\": 2},\n  {\"id\": 3},\n  {\"id\": 4}\n]\n";
        let out = under(30, src);
        assert!(out.len() > 1, "got {out:?}");
        assert!(out.iter().all(|c| c.symbol.is_none()));
    }

    #[test]
    fn line_numbers_point_at_the_member() {
        let src = "{\n  \"dependencies\": {\n    \"a\": \"1.0.0\"\n  },\n  \"scripts\": {\n    \"build\": \"cargo build\"\n  }\n}\n";
        let out = under(60, src);
        let scripts = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("scripts"))
            .expect("scripts");
        assert_eq!(scripts.start_line, 5);
    }
}
