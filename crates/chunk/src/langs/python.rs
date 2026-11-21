//! Python chunker: one chunk per top-level definition or assignment, one per
//! method inside a class big enough to be split.
//!
//! INTENT: Python has no braces — a block is what is indented further than its
//! header — so this chunker cannot reuse the brace pipeline. What it DOES reuse
//! is the principle: boundaries come from a lexical pass, never from a guess,
//! and a `def` inside a triple-quoted string is not a definition.

use std::ops::Range;

use findery_core::Result;
use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, Lang, RawChunk};
use findery_core::traits::Chunker;

use super::module_name;
use crate::sizing::{MAX_NESTING, drop_empty, merge_small, split_windows};

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
        let file = File {
            lines: &lines,
            path,
            cfg,
        };
        let mut out = Vec::new();
        for range in python_items(&lines, 0..lines.len(), "") {
            file.emit(range, None, &mut out);
        }
        Ok(merge_small(drop_empty(out), cfg))
    }
}

/// The files a package stands for: `app/__init__.py` is `app`.
const DIRECTORY_FILES: &[&str] = &["__init__", "__main__"];

/// The fixed inputs of one chunking pass.
struct File<'a> {
    lines: &'a [&'a str],
    path: &'a str,
    cfg: &'a ChunkConfig,
}

/// The class a member is emitted from.
struct Parent<'a> {
    label: &'a str,
    /// How many classes enclose its members: 1 for a top-level class.
    depth: usize,
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

/// What one pass over a block finds: the lines that open a statement, and the
/// lines that document the statement below them.
struct Block {
    anchors: Vec<usize>,
    /// Per line of the block: a comment, a decorator, or a line a decorator's
    /// argument list runs over — `@dataclass(` / `frozen=True,` / `)`.
    documents: Vec<bool>,
}

/// Splits a range of lines into items at `indent`, the exact indentation of the
/// block those items belong to (empty at the top level).
///
/// An item runs from its first line, leading comments and decorators included,
/// to the line before the next one starts. What comes before the first item —
/// a licence header, a file of comments only — is an item of its own: no line
/// is dropped.
fn python_items(lines: &[&str], range: Range<usize>, indent: &str) -> Vec<Item> {
    let block = scan_block(lines, range.clone(), indent);
    let heads = item_heads(&block, range.start);
    let first = heads.first().copied().unwrap_or(range.end);
    let mut items = Vec::with_capacity(heads.len() + 1);
    if first > range.start {
        items.push(Item {
            lines: range.start..first,
            anchor: None,
        });
    }
    for (k, (&head, &anchor)) in heads.iter().zip(&block.anchors).enumerate() {
        let end = heads.get(k + 1).copied().unwrap_or(range.end);
        items.push(Item {
            lines: head..end,
            anchor: Some(anchor),
        });
    }
    items
}

/// One item of a block.
struct Item {
    lines: Range<usize>,
    /// The line its statement opens on, past its comments and decorators —
    /// `None` for the lines before a block's first statement.
    anchor: Option<usize>,
}

/// One lexical pass over a block: where its statements open, and which lines
/// document the next one.
fn scan_block(lines: &[&str], range: Range<usize>, indent: &str) -> Block {
    let mut state = LexState::default();
    let mut block = Block {
        anchors: Vec::new(),
        documents: Vec::with_capacity(range.len()),
    };
    let mut in_decorator = false;
    for i in range {
        let at_statement = state.depth == 0 && state.triple.is_none() && !state.continued;
        match (at_statement, statement_at(lines[i], indent)) {
            (true, Statement::Code) => {
                block.anchors.push(i);
                in_decorator = false;
            }
            (true, Statement::Decorator) => in_decorator = true,
            (true, Statement::None) => in_decorator = false,
            (false, _) => {}
        }
        block.documents.push(in_decorator || is_comment(lines[i]));
        scan_line(lines[i], &mut state);
    }
    block
}

/// Where each item starts: its anchor, walked back over the comments and
/// decorators that document it — never past the previous item's code.
fn item_heads(block: &Block, start: usize) -> Vec<usize> {
    let documents = |line: usize| block.documents[line - start];
    let mut heads = Vec::with_capacity(block.anchors.len());
    for (k, &anchor) in block.anchors.iter().enumerate() {
        let floor = match k {
            0 => start,
            _ => block.anchors[k - 1] + 1,
        };
        let mut head = anchor;
        while head > floor && documents(head - 1) {
            head -= 1;
        }
        heads.push(head);
    }
    heads
}

/// What a line opens in the block at `indent`.
enum Statement {
    /// A statement of code at that exact indentation.
    Code,
    /// A decorator, which documents the statement below it.
    Decorator,
    /// Nothing: a blank line, a comment, a deeper line.
    None,
}

/// What this line opens in the block at `indent`.
fn statement_at(line: &str, indent: &str) -> Statement {
    let Some(rest) = line.strip_prefix(indent) else {
        return Statement::None;
    };
    let code = rest.trim_end_matches(['\r', '\n']);
    match code.chars().next() {
        None | Some(' ' | '\t' | '#') => Statement::None,
        Some('@') => Statement::Decorator,
        Some(_) => Statement::Code,
    }
}

/// A comment line. A blank line is NOT one that documents — kept with the
/// item above, it lands where the reader expects it and costs nothing, since a
/// chunk's trailing whitespace is trimmed.
fn is_comment(line: &str) -> bool {
    line.trim().starts_with('#')
}

impl File<'_> {
    /// Emits one item, recursing into a class body that exceeds the budget.
    fn emit(&self, item: Item, parent: Option<&Parent<'_>>, out: &mut Vec<RawChunk>) {
        let text = self.text(&item.lines);
        if text.trim().is_empty() {
            return;
        }
        let header = item.anchor.map_or("", |anchor| self.lines[anchor]);
        let meta = classify(header, item.lines.start == 0);
        let kind = match parent {
            Some(_) if meta.kind == ChunkKind::Function => ChunkKind::Method,
            _ => meta.kind,
        };
        let breadcrumb = match (parent, &meta.symbol) {
            (Some(p), Some(s)) => Some(format!("{} > {s}", p.label)),
            (Some(p), None) => Some(p.label.to_string()),
            (None, Some(s)) => Some(s.clone()),
            (None, None) => None,
        };
        let depth = parent.map_or(0, |p| p.depth);
        let oversized_class = meta.kind == ChunkKind::Struct && text.len() > self.cfg.max_chars;
        // Past MAX_NESTING classes deep, what is left is windowed as one leaf.
        if oversized_class
            && depth < MAX_NESTING
            && let Some(anchor) = item.anchor
        {
            let body_start = self.header_end(anchor, item.lines.end);
            let body = body_start..item.lines.end;
            if let Some(indent) = body_indent(self.lines, body.clone()) {
                let children = python_items(self.lines, body, indent);
                let label = breadcrumb.clone().unwrap_or_else(|| "?".into());
                // The header — comments, decorators, bases — is content no
                // method carries: it gets its chunk.
                let header = item.lines.start..body_start;
                out.extend(self.leaf(&header, kind, meta.symbol, breadcrumb));
                let class = Parent {
                    label: &label,
                    depth: depth + 1,
                };
                for child in children {
                    self.emit(child, Some(&class), out);
                }
                return;
            }
        }
        // A module docstring documents the file, so it takes the file's name —
        // the same answer the Rust chunker gives its `//!` preamble.
        let names_the_file = meta.kind == ChunkKind::Module && text.len() <= self.cfg.max_chars;
        let (symbol, breadcrumb) = match names_the_file {
            true => {
                let name = module_name(self.path, ".py", DIRECTORY_FILES);
                (Some(name.clone()), Some(format!("module {name}")))
            }
            false => (meta.symbol, breadcrumb),
        };
        out.extend(self.leaf(&item.lines, kind, symbol, breadcrumb));
    }

    /// `range` as one chunk, or as line windows when it is too big.
    fn leaf(
        &self,
        range: &Range<usize>,
        kind: ChunkKind,
        symbol: Option<String>,
        breadcrumb: Option<String>,
    ) -> Vec<RawChunk> {
        let text = self.text(range);
        let start_line = (range.start + 1) as u32;
        match text.len() > self.cfg.max_chars {
            true => split_windows(&text, start_line, kind, symbol, breadcrumb, self.cfg),
            false => vec![RawChunk {
                kind,
                symbol,
                breadcrumb,
                start_line,
                end_line: end_line_of(self.lines, range),
                text: text.trim_end().to_string(),
            }],
        }
    }

    /// The line just past the `class …:` statement opening at `anchor`, over
    /// however many lines it spans.
    fn header_end(&self, anchor: usize, end: usize) -> usize {
        let mut state = LexState::default();
        let mut i = anchor;
        while i < end {
            scan_line(self.lines[i], &mut state);
            i += 1;
            if state.depth == 0 && state.triple.is_none() && !state.continued {
                break;
            }
        }
        i
    }

    /// The text of a line range.
    fn text(&self, range: &Range<usize>) -> String {
        self.lines[range.clone()].concat()
    }
}

/// 1-indexed number of the last line carrying something.
fn end_line_of(lines: &[&str], range: &Range<usize>) -> u32 {
    let last = lines[range.clone()]
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .unwrap_or(0);
    (range.start + last + 1) as u32
}

/// The indentation of the first non-blank line of a block, which is the
/// indentation every statement of that block shares.
fn body_indent<'a>(lines: &[&'a str], range: Range<usize>) -> Option<&'a str> {
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
    fn a_split_class_keeps_its_header_in_a_chunk_of_its_own() {
        let body = "        value = 1\n".repeat(20);
        let src = format!(
            "# The widget.\n@dataclass\nclass Widget(\n    Base,\n):\n    def render(self):\n{body}"
        );
        let out = chunk_under(&src, 200);
        let header = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("Widget"))
            .expect("the class keeps a chunk");
        assert_eq!(header.kind, ChunkKind::Struct);
        assert_eq!(
            header.text,
            "# The widget.\n@dataclass\nclass Widget(\n    Base,\n):"
        );
        assert_eq!((header.start_line, header.end_line), (1, 5));
        let render = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("render"))
            .expect("render");
        assert_eq!(render.breadcrumb.as_deref(), Some("Widget > render"));
        assert_eq!(render.start_line, 6);
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
    fn a_file_of_comments_or_a_lone_shebang_still_yields_a_chunk() {
        for src in [
            "# Licence: MIT\n# Nothing else.\n",
            "#!/usr/bin/env python3",
        ] {
            let out = chunk(src);
            assert_eq!(out.len(), 1, "{src:?} gave {out:?}");
            assert_eq!(out[0].text, src.trim_end());
        }
    }

    #[test]
    fn the_lines_before_the_first_item_are_kept() {
        let out = chunk("# Licence: MIT\n\nimport os\n");
        assert!(out[0].text.starts_with("# Licence"), "got {out:?}");
        assert!(out.iter().any(|c| c.text.contains("import os")));
    }

    #[test]
    fn a_decorator_spanning_lines_stays_with_its_class() {
        let out = chunk("@dataclass(\n    frozen=True,\n)\nclass Widget:\n    x = 1\n");
        assert_eq!(out.len(), 1, "got {out:?}");
        assert_eq!(out[0].symbol.as_deref(), Some("Widget"));
        assert_eq!(out[0].start_line, 1);
    }

    #[test]
    fn a_package_init_takes_the_package_name() {
        let out = PythonChunker
            .chunk(
                "app/limits/__init__.py",
                "\"\"\"Limits.\"\"\"\n",
                &ChunkConfig::default(),
            )
            .expect("chunking succeeds");
        assert_eq!(out[0].symbol.as_deref(), Some("limits"));
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
