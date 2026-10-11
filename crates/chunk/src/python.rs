//! Python chunker: one chunk per top-level definition or assignment, one per
//! method inside a class big enough to be split.
//!
//! INTENT: Python has no braces — a block is what is indented further than its
//! header — so this chunker cannot reuse the brace pipeline. What it DOES reuse
//! is the principle: boundaries come from a lexical pass, never from a guess,
//! and a `def` inside a triple-quoted string is not a definition.

use crate::text::{drop_empty, merge_small, split_windows};
use findery_core::Result;
use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, Lang, RawChunk};
use findery_core::traits::Chunker;

/// Splits Python source into items.
pub struct PythonChunker;

impl Chunker for PythonChunker {
    fn lang(&self) -> Lang {
        Lang::Python
    }

    fn chunk(&self, path: &str, text: &str, cfg: &ChunkConfig) -> Result<Vec<RawChunk>> {
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let mut out = Vec::new();
        for range in python_items(&lines, 0..lines.len(), "") {
            emit(&lines, range, path, None, cfg, &mut out);
        }
        Ok(merge_small(drop_empty(out), cfg))
    }
}

/// What a line-by-line pass must remember: a bracket left open, a triple-quoted
/// string left open, and a line continued with a backslash.
#[derive(Default, Clone, Copy)]
struct LexState {
    depth: usize,
    triple: Option<u8>,
    continued: bool,
}

/// Advances the lexical state over one line.
fn scan_line(line: &str, state: &mut LexState) {
    let bytes = line.as_bytes();
    let mut i = 0usize;
    state.continued = false;
    while i < bytes.len() {
        if let Some(q) = state.triple {
            if bytes[i] == b'\\' {
                i += 2;
            } else if bytes[i] == q && bytes.get(i + 1) == Some(&q) && bytes.get(i + 2) == Some(&q)
            {
                state.triple = None;
                i += 3;
            } else {
                i += 1;
            }
            continue;
        }
        match bytes[i] {
            // A `#` outside a string comments out the rest of the line.
            b'#' => return,
            q @ (b'\'' | b'"') => {
                if bytes.get(i + 1) == Some(&q) && bytes.get(i + 2) == Some(&q) {
                    state.triple = Some(q);
                    i += 3;
                } else {
                    i += 1;
                    while i < bytes.len() && bytes[i] != q {
                        i += if bytes[i] == b'\\' { 2 } else { 1 };
                    }
                    i += 1;
                }
            }
            b'(' | b'[' | b'{' => {
                state.depth += 1;
                i += 1;
            }
            b')' | b']' | b'}' => {
                state.depth = state.depth.saturating_sub(1);
                i += 1;
            }
            _ => i += 1,
        }
    }
    // A trailing backslash joins the next line to this statement, so that line
    // must not open an item of its own.
    if line.trim_end_matches(['\r', '\n']).ends_with('\\') {
        state.continued = true;
    }
}

/// Splits a range of lines into items at `indent`, the exact indentation of the
/// block those items belong to (empty at the top level).
///
/// An item runs from its first line, leading comments and decorators included,
/// to the line before the next one starts.
fn python_items(
    lines: &[&str],
    range: std::ops::Range<usize>,
    indent: &str,
) -> Vec<std::ops::Range<usize>> {
    let mut state = LexState::default();
    let mut anchors: Vec<usize> = Vec::new();
    for i in range.clone() {
        let opens_item = state.depth == 0
            && state.triple.is_none()
            && !state.continued
            && opens_a_statement(lines[i], indent);
        if opens_item {
            anchors.push(i);
        }
        scan_line(lines[i], &mut state);
    }

    let mut heads: Vec<usize> = Vec::with_capacity(anchors.len());
    for (k, &anchor) in anchors.iter().enumerate() {
        // Walk back over the comments and decorators that document it, but
        // never past the previous item's last line of code.
        let floor = if k == 0 {
            range.start
        } else {
            anchors[k - 1] + 1
        };
        let mut head = anchor;
        while head > floor && documents_next(lines[head - 1]) {
            head -= 1;
        }
        heads.push(head);
    }

    let mut items = Vec::new();
    for (k, &head) in heads.iter().enumerate() {
        let end = heads.get(k + 1).copied().unwrap_or(range.end);
        if end > head {
            items.push(head..end);
        }
    }
    items
}

/// Whether this line starts a statement of the block at `indent`: at that exact
/// indentation, and not a comment, a decorator or a blank line.
fn opens_a_statement(line: &str, indent: &str) -> bool {
    let Some(rest) = line.strip_prefix(indent) else {
        return false;
    };
    if rest.is_empty() || rest.starts_with([' ', '\t', '\r', '\n']) {
        return false;
    }
    let code = rest.trim_end_matches(['\r', '\n']);
    !code.is_empty() && !code.starts_with('#') && !code.starts_with('@')
}

/// A line that documents the item below it rather than standing on its own:
/// a comment or a decorator. A blank line is NOT one — kept with the item
/// above, it lands where the reader expects it and costs nothing, since a
/// chunk's trailing whitespace is trimmed.
fn documents_next(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('#') || t.starts_with('@')
}

/// Whether a line carries nothing at all.
fn is_trivia(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#') || t.starts_with('@')
}

/// Emits one item, recursing into a class body that exceeds the budget.
fn emit(
    lines: &[&str],
    range: std::ops::Range<usize>,
    path: &str,
    parent: Option<&str>,
    cfg: &ChunkConfig,
    out: &mut Vec<RawChunk>,
) {
    let text = item_text(lines, &range);
    if text.trim().is_empty() {
        return;
    }
    let header = header_line(lines, &range);
    let meta = classify(header, range.start == 0);
    let kind = match parent {
        Some(_) if meta.kind == ChunkKind::Function => ChunkKind::Method,
        _ => meta.kind,
    };
    let breadcrumb = match (parent, &meta.symbol) {
        (Some(p), Some(s)) => Some(format!("{p} > {s}")),
        (Some(p), None) => Some(p.to_string()),
        (None, Some(s)) => Some(s.clone()),
        (None, None) => None,
    };
    let start_line = (range.start + 1) as u32;
    let end_line = end_line_of(lines, &range);

    if meta.kind == ChunkKind::Struct && text.len() > cfg.max_chars {
        let body_start = range.start + 1;
        if let Some(indent) = body_indent(lines, body_start..range.end) {
            let children = python_items(lines, body_start..range.end, indent);
            if !children.is_empty() {
                let label = breadcrumb.clone().unwrap_or_else(|| "?".into());
                for child in children {
                    emit(lines, child, path, Some(&label), cfg, out);
                }
                return;
            }
        }
    }

    if text.len() <= cfg.max_chars {
        // A module docstring documents the file, so it takes the file's name —
        // the same answer the Rust chunker gives its `//!` preamble.
        let named = (meta.kind == ChunkKind::Module).then(|| module_name(path));
        out.push(RawChunk {
            kind,
            symbol: named.clone().or(meta.symbol),
            breadcrumb: named.map(|n| format!("module {n}")).or(breadcrumb),
            start_line,
            end_line,
            text: text.trim_end().to_string(),
        });
    } else {
        out.extend(split_windows(
            &text,
            start_line,
            kind,
            meta.symbol,
            breadcrumb,
            cfg,
        ));
    }
}

/// The text of a line range.
fn item_text(lines: &[&str], range: &std::ops::Range<usize>) -> String {
    lines[range.clone()].concat()
}

/// The first line of the item that is real code, not a comment or a decorator.
fn header_line<'a>(lines: &[&'a str], range: &std::ops::Range<usize>) -> &'a str {
    lines[range.clone()]
        .iter()
        .find(|l| !is_trivia(l))
        .copied()
        .unwrap_or("")
}

/// 1-indexed number of the last line carrying something.
fn end_line_of(lines: &[&str], range: &std::ops::Range<usize>) -> u32 {
    let last = lines[range.clone()]
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .unwrap_or(0);
    (range.start + last + 1) as u32
}

/// The indentation of the first non-blank line of a block, which is the
/// indentation every statement of that block shares.
fn body_indent<'a>(lines: &[&'a str], range: std::ops::Range<usize>) -> Option<&'a str> {
    let line = lines[range].iter().find(|l| !l.trim().is_empty())?;
    let width = line.len() - line.trim_start().len();
    Some(&line[..width])
}

/// What the header of an item says about it.
struct PythonMeta {
    kind: ChunkKind,
    symbol: Option<String>,
}

/// Reads the kind out of an item's first code line.
///
/// INTENT: an assignment at the top level is a named binding, so it is a
/// constant — `MAX_RETRIES = 4` is exactly the identifier a search looks for.
fn classify(header: &str, first_item: bool) -> PythonMeta {
    let code = header.trim_end_matches(['\r', '\n']);
    let code = code.split("  #").next().unwrap_or(code).trim();
    for (prefix, kind) in [
        ("async def ", ChunkKind::Function),
        ("def ", ChunkKind::Function),
        ("class ", ChunkKind::Struct),
    ] {
        if let Some(rest) = code.strip_prefix(prefix) {
            return PythonMeta {
                kind,
                symbol: identifier(rest),
            };
        }
    }
    if first_item && (code.starts_with("\"\"\"") || code.starts_with("'''")) {
        return PythonMeta {
            kind: ChunkKind::Module,
            symbol: None,
        };
    }
    // An annotation is part of the binding, not of the name: `MAX: int = 4`
    // names `MAX`.
    if let Some((lhs, _)) = code.split_once('=')
        && let Some(annotated) = lhs.split(':').next().map(str::trim)
        && let Some(name) = identifier(annotated)
        && annotated == name
    {
        return PythonMeta {
            kind: ChunkKind::Const,
            symbol: Some(name),
        };
    }
    PythonMeta {
        kind: ChunkKind::Text,
        symbol: None,
    }
}

/// The identifier at the start of a fragment.
fn identifier(s: &str) -> Option<String> {
    let name: String = s
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() { None } else { Some(name) }
}

/// `app/rate_limit.py` -> `rate_limit`.
fn module_name(path: &str) -> String {
    path.rsplit('/')
        .next()
        .unwrap_or(path)
        .trim_end_matches(".py")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(src: &str) -> Vec<RawChunk> {
        chunk_under(src, ChunkConfig::default().max_chars)
    }

    fn chunk_under(src: &str, max_chars: usize) -> Vec<RawChunk> {
        let cfg = ChunkConfig {
            min_chars: 1,
            max_chars,
            ..ChunkConfig::default()
        };
        PythonChunker
            .chunk("app/rate_limit.py", src, &cfg)
            .expect("chunking succeeds")
    }

    #[test]
    fn a_function_keeps_its_docstring_and_its_decorators() {
        let out =
            chunk("@app.route('/x')\ndef handler(request):\n    \"\"\"Docs.\"\"\"\n    return 1\n");
        let h = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("handler"))
            .expect("handler");
        assert_eq!(h.kind, ChunkKind::Function);
        assert!(h.text.starts_with("@app.route"), "got {:?}", h.text);
        assert_eq!(h.start_line, 1);
    }

    #[test]
    fn an_annotated_assignment_names_its_constant() {
        let out = chunk("MAX_RETRIES: int = 4\n\nLIMITS: dict[str, int] = {}\n");
        assert_eq!(
            out.iter()
                .filter_map(|c| c.symbol.as_deref())
                .collect::<Vec<_>>(),
            vec!["MAX_RETRIES", "LIMITS"]
        );
        assert!(out.iter().all(|c| c.kind == ChunkKind::Const));
    }

    #[test]
    fn a_tuple_unpacking_is_not_a_constant() {
        let out = chunk("x, y = 1, 2\n");
        assert!(out[0].symbol.is_none(), "got {out:?}");
    }

    #[test]
    fn a_comment_belongs_to_the_definition_below_it() {
        let out = chunk("import os\n\n# explains MAX\nMAX_RETRIES = 4\n");
        let max = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("MAX_RETRIES"))
            .expect("const");
        assert_eq!(max.kind, ChunkKind::Const);
        assert!(max.text.starts_with("# explains MAX"), "got {:?}", max.text);
        let import = out.first().expect("import");
        assert_eq!(import.kind, ChunkKind::Text);
        assert!(!import.text.contains("explains"));
    }

    #[test]
    fn a_def_inside_a_triple_quoted_string_is_not_a_definition() {
        let src = "TEMPLATE = \"\"\"\ndef not_a_function():\n    pass\n\"\"\"\n\ndef real():\n    return 1\n";
        let out = chunk(src);
        let names: Vec<_> = out.iter().filter_map(|c| c.symbol.clone()).collect();
        assert_eq!(
            names,
            vec!["TEMPLATE".to_string(), "real".to_string()],
            "got {out:?}"
        );
    }

    #[test]
    fn a_large_class_is_split_per_method() {
        let body = "        value = 1\n".repeat(40);
        let src = format!(
            "class Widget:\n    \"\"\"Docs.\"\"\"\n\n    def render(self):\n{body}\n    def paint(self):\n{body}"
        );
        let out = chunk_under(&src, 200);
        let render = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("render"))
            .expect("render");
        assert_eq!(render.kind, ChunkKind::Method);
        assert_eq!(render.breadcrumb.as_deref(), Some("Widget > render"));
        assert!(out.iter().any(|c| c.symbol.as_deref() == Some("paint")));
    }

    #[test]
    fn a_statement_continued_over_lines_stays_one_item() {
        let src = "SETTINGS = {\n    'a': 1,\n    'b': 2,\n}\n\ndef after():\n    return 1\n";
        let out = chunk(src);
        let names: Vec<_> = out.iter().filter_map(|c| c.symbol.clone()).collect();
        assert_eq!(
            names,
            vec!["SETTINGS".to_string(), "after".to_string()],
            "got {out:?}"
        );
        assert_eq!(out[0].end_line, 4);
    }

    #[test]
    fn a_module_docstring_heads_its_own_chunk() {
        let out = chunk("\"\"\"Module docs.\"\"\"\n\ndef a():\n    return 1\n");
        assert_eq!(out[0].kind, ChunkKind::Module);
        assert_eq!(out[0].symbol.as_deref(), Some("rate_limit"));
    }

    #[test]
    fn line_numbers_cover_the_whole_block() {
        let out = chunk("def a():\n    x = 1\n    return x\n\n\ndef b():\n    return 2\n");
        let b = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("b"))
            .expect("b");
        assert_eq!(b.start_line, 6);
        assert_eq!(b.end_line, 7);
    }
}
