//! The pipeline every brace-delimited language shares.
//!
//! INTENT: Rust, JavaScript, PHP, Axle, Java, Kotlin and Go differ in
//! their keywords and their trivia, not in the shape of an item. Writing that
//! pipeline once — split, classify, recurse into an oversized container, window
//! what is still too big — is what keeps a fix to item boundaries a fix for
//! every language at once (CONV-024: the chunker splits, and nothing else).
//! Reading one item's header is [`crate::header`]'s job.

use findery_core::Result;
use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, RawChunk};

use crate::header::split_trivia;
use crate::items::{LineIndex, Span, body_span, split_items};
use crate::scanner::{Lexicon, is_comment_at, skip_opaque};
use crate::sizing::{MAX_NESTING, drop_empty, merge_small, split_windows, within_budget};

/// What the header of an item says about it.
pub(crate) struct ItemMeta {
    /// What the item declares.
    pub kind: ChunkKind,
    /// The name it declares, when it names one.
    pub symbol: Option<String>,
    /// It is a test, or holds only tests.
    pub is_test: bool,
}

/// A language as the shared pipeline needs to see it.
pub(crate) trait Profile {
    /// The language's lexical trivia.
    fn lexicon(&self) -> &'static Lexicon;

    /// Reads the kind and the symbol out of one item's text. `parent` is the
    /// kind of the container the item sits in, which is what makes a
    /// `Method` a method — and a Java `URL(String s) {}` a constructor in a
    /// class but a constant in an enum.
    fn classify(&self, item: &str, parent: Option<ChunkKind>) -> ItemMeta;

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

    /// The members of a declaration group that is split at any size — Go's
    /// `type ( … )` — or `None` when `item` is no such group.
    fn group_body(&self, _src: &str, _item: Span) -> Option<Span> {
        None
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

/// The container a member is emitted from.
struct Parent {
    /// Its breadcrumb, which prefixes its members'. A group adds none: the
    /// types of a `type ( … )` are named as if declared one by one.
    label: Option<String>,
    kind: ChunkKind,
    is_test: bool,
    /// How many containers enclose its members: 1 for a top-level parent.
    depth: usize,
}

/// One item, classified: what its chunks will carry.
struct Classified {
    kind: ChunkKind,
    symbol: Option<String>,
    breadcrumb: Option<String>,
    is_test: bool,
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
    if let Some(head) = head {
        out.extend(within_budget(head, cfg));
    }
    let emitter = Emitter {
        src,
        index: &index,
        cfg,
        profile,
        lex,
    };
    let file = Span {
        start,
        end: src.len(),
    };
    for item in split_items(src, file, lex) {
        emitter.emit_item(item, None, &mut out);
    }
    Ok(merge_small(drop_empty(out), cfg))
}

impl Emitter<'_> {
    /// Emits one item, recursing into a container body that exceeds the budget.
    fn emit_item(&self, item: Span, parent: Option<&Parent>, out: &mut Vec<RawChunk>) {
        let item_text = item.slice(self.src);
        if item_text.trim().is_empty() {
            return;
        }
        let meta = self.profile.classify(item_text, parent.map(|p| p.kind));
        let item_kind = meta.kind;
        let this = self.classified(meta, parent);
        if this.is_test && !self.cfg.include_tests {
            return;
        }
        if let Some((body, is_group)) = self.body_to_split(item, item_kind, parent) {
            let children = split_items(self.src, body, self.lex);
            if !children.is_empty() {
                let label = match is_group {
                    true => parent.and_then(|p| p.label.clone()),
                    false => Some(this.breadcrumb.clone().unwrap_or_else(|| "?".into())),
                };
                let container = Parent {
                    label,
                    kind: item_kind,
                    is_test: this.is_test,
                    depth: parent.map_or(1, |p| p.depth + 1),
                };
                // The header — doc comment, annotations, supertypes, up to the
                // `{` — is content no member carries: it gets its chunk.
                let header = Span {
                    start: item.start,
                    end: body.start,
                };
                self.emit_container(header, &children, this, &container, out);
                return;
            }
        }
        self.emit_leaf(item, this, out);
    }

    /// Stamps the item's kind and breadcrumb from what its parent is.
    fn classified(&self, meta: ItemMeta, parent: Option<&Parent>) -> Classified {
        let is_test = parent.is_some_and(|p| p.is_test) || meta.is_test;
        let kind = match (is_test, parent) {
            (true, _) => ChunkKind::Test,
            (false, Some(_)) => self.profile.nested_kind(meta.kind),
            (false, None) => meta.kind,
        };
        let label = parent.and_then(|p| p.label.as_deref());
        let breadcrumb = match (label, &meta.symbol) {
            (Some(label), Some(s)) => Some(format!("{label} > {s}")),
            (Some(label), None) => Some(label.to_string()),
            (None, Some(s)) => Some(s.clone()),
            (None, None) => None,
        };
        Classified {
            kind,
            symbol: meta.symbol,
            breadcrumb,
            is_test,
        }
    }

    /// The body to split `item` into, and whether it is a group: a group is
    /// split at any size, any other container once it is too big — and none
    /// past [`MAX_NESTING`], where what is left is windowed as one leaf.
    fn body_to_split(
        &self,
        item: Span,
        kind: ChunkKind,
        parent: Option<&Parent>,
    ) -> Option<(Span, bool)> {
        if parent.is_some_and(|p| p.depth >= MAX_NESTING) {
            return None;
        }
        if let Some(group) = self.profile.group_body(self.src, item) {
            return Some((group, true));
        }
        let oversized = item.slice(self.src).len() > self.cfg.max_chars;
        let with_parent = parent.is_some() && self.profile.splits_with_parent(kind);
        let wanted = self.profile.recursable(kind) && (oversized || with_parent);
        let body = wanted
            .then(|| body_span(self.src, item, self.lex))
            .flatten()?;
        Some((body, false))
    }

    /// Emits a split container: its header, then each member below it.
    fn emit_container(
        &self,
        header: Span,
        children: &[Span],
        this: Classified,
        container: &Parent,
        out: &mut Vec<RawChunk>,
    ) {
        let bare_test = this.is_test && !self.documents(header);
        let first = out.len();
        self.emit_leaf(header, this, out);
        let header_is_one_chunk = out.len() == first + 1;
        for child in children {
            self.emit_item(*child, Some(container), out);
        }
        if bare_test && header_is_one_chunk {
            fold_next_into_header(out, first, self.cfg);
        }
    }

    /// Whether the trivia of `span` holds a comment — a doc worth a chunk.
    fn documents(&self, span: Span) -> bool {
        let (trivia, _) = split_trivia(span.slice(self.src), self.lex);
        let bytes = trivia.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            if is_comment_at(bytes, i, self.lex) {
                return true;
            }
            i = skip_opaque(bytes, i, self.lex).map_or(i + 1, |next| next.max(i + 1));
        }
        false
    }

    /// Emits `span` as one chunk, or as line windows when it is too big.
    fn emit_leaf(&self, span: Span, this: Classified, out: &mut Vec<RawChunk>) {
        let text = span.slice(self.src);
        let start_line = self.index.line_of(span.start);
        match text.len() <= self.cfg.max_chars {
            true => out.push(RawChunk {
                kind: this.kind,
                symbol: this.symbol,
                breadcrumb: this.breadcrumb,
                start_line,
                end_line: self.index.line_of(span.end.saturating_sub(1)),
                text: text.trim_end().to_string(),
            }),
            false => out.extend(split_windows(
                text,
                start_line,
                this.kind,
                this.symbol,
                this.breadcrumb,
                self.cfg,
            )),
        }
    }
}

/// Folds the anonymous member chunk after the header at `at` into it — the
/// `use super::*;` a test module opens on.
///
/// INTENT: the header of an undocumented test module — `#[cfg(test)]` over
/// `mod tests {` — carries nothing a search wants on its own, and as a chunk
/// of its own it is a 25-byte hit that answers nothing. Joined to its first
/// member it keeps every word and costs no chunk. A member that names
/// something is left alone: folding would drop one of the two names, and a
/// name is never merged away.
fn fold_next_into_header(out: &mut Vec<RawChunk>, at: usize, cfg: &ChunkConfig) {
    let foldable = match (out.get(at), out.get(at + 1)) {
        (Some(header), Some(next)) => {
            next.symbol.is_none() && header.text.len() + 1 + next.text.len() <= cfg.max_chars
        }
        _ => false,
    };
    if !foldable {
        return;
    }
    let next = out.remove(at + 1);
    let header = &mut out[at];
    header.text.push('\n');
    header.text.push_str(&next.text);
    header.end_line = next.end_line;
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
