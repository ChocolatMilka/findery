//! JSON chunker: the whole file while it fits, one chunk per top-level member
//! once it does not.
//!
//! INTENT: a JSON file is one document — `package.json` answered in one chunk
//! is worth more than its five sections answered separately. Only a file too
//! big for the budget becomes members, and then the KEY is the symbol, so
//! "dependencies" or "scripts" is found by name and not only by vector.

use crate::items::{LineIndex, Span};
use crate::scanner::{Lexicon, is_comment_at, skip_opaque};
use crate::sizing::{drop_empty, merge_small, split_windows};
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
    if span.slice(src).trim().len() > cfg.max_chars
        && depth < MAX_DEPTH
        && let Some(container) = members(src, span)
        && container.members.len() > 1
    {
        // The member's own key — `"dependencies": {` — is in none of its
        // entries: it gets its chunk. The root's lone `{` is dropped as empty.
        let head = Span {
            start: span.start,
            end: container.open,
        };
        leaf(src, head, key, depth, index, cfg, out);
        for (member, member_key) in container.members {
            emit(src, member, member_key, index, cfg, depth + 1, out);
        }
        // What follows the closing bracket — a jsonc comment after the last
        // member — is in no member either. A lone `}` is dropped as empty.
        let tail = Span {
            start: container.close,
            end: span.end,
        };
        leaf(src, tail, None, depth + 1, index, cfg, out);
        return;
    }
    leaf(src, span, key, depth, index, cfg, out);
}

/// Emits `span` as one chunk, or as line windows when it is too big.
fn leaf(
    src: &str,
    span: Span,
    key: Option<String>,
    depth: u32,
    index: &LineIndex,
    cfg: &ChunkConfig,
    out: &mut Vec<RawChunk>,
) {
    let raw = span.slice(src);
    let text = raw.trim();
    if text.is_empty() {
        return;
    }
    let lead = raw.len() - raw.trim_start().len();
    let start_line = index.line_of(span.start + lead);
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

/// One member of a container, with its key when the container is an object.
type Member = (Span, Option<String>);

/// A container read into its members.
struct Container {
    /// The offset just past its opening bracket.
    open: usize,
    /// The offset of its closing bracket.
    close: usize,
    members: Vec<Member>,
}

/// The container in `span`, and its top-level members.
fn members(src: &str, span: Span) -> Option<Container> {
    let bytes = src.as_bytes();
    let mut i = span.start;
    let mut root = false;
    let mut object = false;
    let mut depth = 0usize;
    let mut open = 0usize;
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
                    open = i + 1;
                    start = open;
                }
                depth += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 && root {
                    push_member(&mut out, src, start, i, object);
                    return Some(Container {
                        open,
                        close: i,
                        members: out,
                    });
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

fn push_member(out: &mut Vec<Member>, src: &str, from: usize, to: usize, object: bool) {
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

/// The key of an object member: the string before its `:`, past the jsonc
/// comments above it. An empty key `""` names nothing.
fn member_key(member: &str) -> Option<String> {
    let rest = past_comments(member).strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next()?),
            '"' => return Some(out).filter(|key| !key.is_empty()),
            _ => out.push(c),
        }
    }
    None
}

/// `text` past its leading whitespace and comments.
fn past_comments(text: &str) -> &str {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    loop {
        while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        match (
            is_comment_at(bytes, i, &Lexicon::JSON),
            skip_opaque(bytes, i, &Lexicon::JSON),
        ) {
            (true, Some(next)) => i = next.max(i + 1),
            _ => return &text[i.min(text.len())..],
        }
    }
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
    fn a_split_member_keeps_its_key_in_a_chunk_of_its_own() {
        let src = "{\n  \"dependencies\": {\n    \"left-pad\": \"1.3.0\",\n    \"lodash\": \"4.17.21\"\n  },\n  \"name\": \"demo\"\n}\n";
        let out = under(40, src);
        let head = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("dependencies"))
            .expect("the key keeps a chunk");
        assert_eq!(head.text, "\"dependencies\": {");
        assert_eq!((head.start_line, head.end_line), (2, 2));
        assert!(out.iter().any(|c| c.symbol.as_deref() == Some("lodash")));
    }

    #[test]
    fn a_comment_after_the_closing_bracket_of_a_split_member_is_kept() {
        let src = "{\n  \"extends\": \"base.json\",\n  \"compilerOptions\": {\n    \"allowJs\": true,\n    \"checkJs\": false\n  }\n  // Path aliases are handled elsewhere.\n}\n";
        let out = under(60, src);
        let tail = out
            .iter()
            .find(|c| c.text.contains("Path aliases"))
            .unwrap_or_else(|| panic!("the comment is lost: {out:#?}"));
        assert_eq!((tail.start_line, tail.end_line), (6, 7), "got {out:#?}");
    }

    #[test]
    fn a_comment_above_a_member_does_not_hide_its_key() {
        let src = "{\n  \"a\": 1,\n  // between\n  \"b\": 2\n}\n";
        let out = under(20, src);
        let keys: Vec<_> = out.iter().filter_map(|c| c.symbol.clone()).collect();
        assert_eq!(keys, vec!["a".to_string(), "b".to_string()], "got {out:#?}");
    }

    #[test]
    fn an_empty_key_names_nothing() {
        let out = under(10, "{\n  \"\": 1,\n  \"b\": 2\n}\n");
        assert!(
            !out.iter().any(|c| c.symbol.as_deref() == Some("")),
            "got {out:#?}"
        );
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
