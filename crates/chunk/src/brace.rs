//! The pipeline every brace-delimited language shares.
//!
//! INTENT: Rust, JavaScript, PHP, Axle, Java, Kotlin and Go differ in
//! their keywords and their trivia, not in the shape of an item. Writing that
//! pipeline once — split, classify, recurse into an oversized container, window
//! what is still too big — is what keeps a fix to item boundaries a fix for
//! every language at once (CONV-024: the chunker splits, and nothing else).
//! Reading one item's header is [`crate::header`]'s job.

use crate::items::{LineIndex, Span, body_span, split_items};
use crate::scanner::Lexicon;
use crate::sizing::{drop_empty, merge_small, split_windows};
use findery_core::Result;
use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, RawChunk};

/// What the header of an item says about it.
pub(crate) struct ItemMeta {
    pub kind: ChunkKind,
    pub symbol: Option<String>,
    pub is_test: bool,
}

/// A language as the shared pipeline needs to see it.
pub(crate) trait Profile {
    /// The language's lexical trivia.
    fn lexicon(&self) -> &'static Lexicon;

    /// Reads the kind and the symbol out of one item's text. `nested` says the
    /// item sits inside a container, which is what makes a `Method` a method.
    fn classify(&self, item: &str, nested: bool) -> ItemMeta;

    /// The file preamble — module docs, a shebang, `<?php` — and the chunk it
    /// becomes. Returns where the items start, and the chunk, if any.
    fn preamble(&self, _src: &str, _path: &str, _index: &LineIndex) -> (usize, Option<RawChunk>) {
        (0, None)
    }

    /// Whether an item of this kind, too big for one chunk, is split per member.
    fn recursable(&self, kind: ChunkKind) -> bool {
        matches!(
            kind,
            ChunkKind::Impl | ChunkKind::Trait | ChunkKind::Module | ChunkKind::Struct
        )
    }

    /// Whether a member of this kind is split whenever its parent is, however
    /// small it is: a group that only gathers members under a condition — an
    /// Axle `when T : Bound { … }` — must not hide them inside one chunk, where
    /// they would lose their `Method` kind and their symbols.
    fn splits_with_parent(&self, _kind: ChunkKind) -> bool {
        false
    }

    /// The kind a member takes once it sits inside a parent.
    fn nested_kind(&self, kind: ChunkKind) -> ChunkKind {
        if kind == ChunkKind::Function {
            ChunkKind::Method
        } else {
            kind
        }
    }
}

/// The fixed inputs of one chunking pass, carried together so a recursive call
/// reads as what it is: an item, its parent, and where to write.
struct Emitter<'a> {
    src: &'a str,
    index: &'a LineIndex,
    cfg: &'a ChunkConfig,
    profile: &'a dyn Profile,
    lex: &'a Lexicon,
}

/// Splits one file: preamble, then one pass per top-level item.
pub(crate) fn chunk_with(
    profile: &dyn Profile,
    path: &str,
    src: &str,
    cfg: &ChunkConfig,
) -> Result<Vec<RawChunk>> {
    if src.trim().is_empty() {
        return Ok(Vec::new());
    }
    let index = LineIndex::new(src);
    let lex = profile.lexicon();
    let mut out = Vec::new();
    let (start, head) = profile.preamble(src, path, &index);
    out.extend(head);
    let emitter = Emitter {
        src,
        index: &index,
        cfg,
        profile,
        lex,
    };
    for item in split_items(
        src,
        Span {
            start,
            end: src.len(),
        },
        lex,
    ) {
        emitter.emit_item(item, None, false, &mut out);
    }
    Ok(merge_small(drop_empty(out), cfg))
}

impl Emitter<'_> {
    /// Emits one item, recursing into a container body that exceeds the budget.
    fn emit_item(
        &self,
        item: Span,
        parent: Option<&str>,
        parent_is_test: bool,
        out: &mut Vec<RawChunk>,
    ) {
        let item_text = item.slice(self.src);
        if item_text.trim().is_empty() {
            return;
        }
        let meta = self.profile.classify(item_text, parent.is_some());
        let is_test = parent_is_test || meta.is_test;
        if is_test && !self.cfg.include_tests {
            return;
        }

        let kind = if is_test {
            ChunkKind::Test
        } else if parent.is_some() {
            self.profile.nested_kind(meta.kind)
        } else {
            meta.kind
        };
        let breadcrumb = match (parent, &meta.symbol) {
            (Some(p), Some(s)) => Some(format!("{p} > {s}")),
            (Some(p), None) => Some(p.to_string()),
            (None, Some(s)) => Some(s.clone()),
            (None, None) => None,
        };
        let oversized = item_text.len() > self.cfg.max_chars;
        let group = parent.is_some() && self.profile.splits_with_parent(meta.kind);
        if self.profile.recursable(meta.kind)
            && (oversized || group)
            && let Some(body) = body_span(self.src, item, self.lex)
        {
            let children = split_items(self.src, body, self.lex);
            if !children.is_empty() {
                let label = breadcrumb.clone().unwrap_or_else(|| "?".into());
                // The header — doc comment, annotations, supertypes, up to the
                // `{` — is content no member carries: it gets its chunk.
                let header = Span {
                    start: item.start,
                    end: body.start,
                };
                self.emit_leaf(header, kind, meta.symbol, breadcrumb, out);
                for child in children {
                    self.emit_item(child, Some(&label), is_test, out);
                }
                return;
            }
        }
        self.emit_leaf(item, kind, meta.symbol, breadcrumb, out);
    }

    /// Emits `span` as one chunk, or as line windows when it is too big.
    fn emit_leaf(
        &self,
        span: Span,
        kind: ChunkKind,
        symbol: Option<String>,
        breadcrumb: Option<String>,
        out: &mut Vec<RawChunk>,
    ) {
        let text = span.slice(self.src);
        let start_line = self.index.line_of(span.start);
        if text.len() <= self.cfg.max_chars {
            out.push(RawChunk {
                kind,
                symbol,
                breadcrumb,
                start_line,
                end_line: self.index.line_of(span.end.saturating_sub(1)),
                text: text.trim_end().to_string(),
            });
        } else {
            out.extend(split_windows(
                text, start_line, kind, symbol, breadcrumb, self.cfg,
            ));
        }
    }
}

/// The line offset where a leading `//!` / `#![…]` preamble ends.
///
/// INTENT: an inner attribute can span several lines, so the attribute's
/// bracket depth — not the mere shape of a line — decides where the preamble
/// stops.
pub(crate) fn inner_doc_preamble_end(text: &str) -> usize {
    let mut offset = 0usize;
    let mut attr_depth = 0usize;
    for line in text.split_inclusive('\n') {
        let t = line.trim_start();
        if attr_depth > 0 {
            attr_depth += t.matches('[').count();
            attr_depth = attr_depth.saturating_sub(t.matches(']').count());
            offset += line.len();
        } else if t.starts_with("//!") || t.is_empty() {
            offset += line.len();
        } else if t.starts_with("#![") {
            attr_depth = t
                .matches('[')
                .count()
                .saturating_sub(t.matches(']').count());
            offset += line.len();
        } else {
            break;
        }
    }
    // A file that opens on blank lines has no preamble worth keeping.
    if text[..offset].trim().is_empty() {
        0
    } else {
        offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_multi_line_attribute_stays_in_the_preamble() {
        let src =
            "//! Module docs.\n#![cfg_attr(\n    test,\n    allow(dead_code)\n)]\n\nfn a() {}\n";
        let end = inner_doc_preamble_end(src);
        assert!(src[..end].contains("allow(dead_code)"));
        assert!(!src[..end].contains("fn a"));
    }
}
