//! Markdown chunker: one chunk per heading section, breadcrumbs carrying the
//! full heading ancestry so a fragment stays self-locating out of context.

use crate::text::{count_lines, drop_empty, merge_small, split_windows};
use findery_core::Result;
use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, Lang, RawChunk};
use findery_core::traits::Chunker;

pub struct MarkdownChunker;

impl Chunker for MarkdownChunker {
    fn lang(&self) -> Lang {
        Lang::Markdown
    }

    fn chunk(&self, _path: &str, text: &str, cfg: &ChunkConfig) -> Result<Vec<RawChunk>> {
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        let sections = split_sections(text);
        let mut out = Vec::new();
        for section in sections {
            if section.text.trim().is_empty() {
                continue;
            }
            if section.text.len() <= cfg.max_chars {
                out.push(RawChunk {
                    kind: ChunkKind::DocSection,
                    symbol: section.title.clone(),
                    breadcrumb: section.breadcrumb.clone(),
                    start_line: section.start_line,
                    end_line: section.start_line + count_lines(&section.text) - 1,
                    text: section.text,
                });
            } else {
                out.extend(split_windows(
                    &section.text,
                    section.start_line,
                    ChunkKind::DocSection,
                    section.title,
                    section.breadcrumb,
                    cfg,
                ));
            }
        }
        Ok(merge_small(drop_empty(out), cfg))
    }
}

struct Section {
    title: Option<String>,
    breadcrumb: Option<String>,
    start_line: u32,
    text: String,
}

/// Splits on ATX headings, ignoring anything inside a fenced code block.
///
/// INTENT: a `# comment` line inside a ```` ```rust ```` fence is not a heading,
/// and treating it as one shatters every code sample in the documentation.
fn split_sections(text: &str) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    let mut ancestry: Vec<(u8, String)> = Vec::new();
    let mut current = Section {
        title: None,
        breadcrumb: None,
        start_line: 1,
        text: String::new(),
    };
    let mut fence: Option<String> = None;

    for (i, line) in text.split_inclusive('\n').enumerate() {
        let line_no = (i + 1) as u32;
        let trimmed = line.trim_start();

        if let Some(marker) = &fence {
            if trimmed.starts_with(marker.as_str()) {
                fence = None;
            }
            current.text.push_str(line);
            continue;
        }
        if let Some(marker) = fence_marker(trimmed) {
            fence = Some(marker);
            current.text.push_str(line);
            continue;
        }

        if let Some((level, title)) = heading(trimmed) {
            if !current.text.trim().is_empty() {
                sections.push(std::mem::replace(
                    &mut current,
                    Section {
                        title: None,
                        breadcrumb: None,
                        start_line: line_no,
                        text: String::new(),
                    },
                ));
            }
            ancestry.retain(|(l, _)| *l < level);
            ancestry.push((level, title.clone()));
            let crumb = ancestry
                .iter()
                .map(|(l, t)| format!("{} {}", "#".repeat(*l as usize), t))
                .collect::<Vec<_>>()
                .join(" > ");
            current = Section {
                title: Some(title),
                breadcrumb: Some(crumb),
                start_line: line_no,
                text: String::new(),
            };
        }
        current.text.push_str(line);
    }
    if !current.text.trim().is_empty() {
        sections.push(current);
    }
    sections
}

/// Returns the fence marker (``` or ~~~, repeated as written) opening here.
fn fence_marker(trimmed: &str) -> Option<String> {
    for ch in ['`', '~'] {
        let n = trimmed.chars().take_while(|c| *c == ch).count();
        if n >= 3 {
            return Some(std::iter::repeat_n(ch, n).collect());
        }
    }
    None
}

/// Parses an ATX heading: 1..=6 `#` followed by whitespace.
fn heading(trimmed: &str) -> Option<(u8, String)> {
    let level = trimmed.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &trimmed[level..];
    if !rest.starts_with([' ', '\t']) {
        return None;
    }
    let title = rest.trim().trim_end_matches('#').trim().to_string();
    if title.is_empty() {
        None
    } else {
        Some((level as u8, title))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `min_chars` is dropped to 1 so the fixtures below keep one chunk per
    /// section; merging of tiny neighbours is covered in `text.rs`.
    fn chunk(src: &str) -> Vec<RawChunk> {
        let cfg = ChunkConfig {
            min_chars: 1,
            ..ChunkConfig::default()
        };
        MarkdownChunker
            .chunk("README.md", src, &cfg)
            .expect("chunking succeeds")
    }

    #[test]
    fn sections_carry_their_heading_ancestry() {
        let out = chunk(
            "# Guide\n\nintro text here\n\n## Install\n\ncargo add it\n\n### Windows\n\nuse msvc\n",
        );
        let win = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("Windows"))
            .expect("section");
        assert_eq!(
            win.breadcrumb.as_deref(),
            Some("# Guide > ## Install > ### Windows")
        );
        assert_eq!(win.kind, ChunkKind::DocSection);
    }

    #[test]
    fn a_sibling_heading_pops_the_ancestry() {
        let out = chunk("# A\n\naaaa aaaa\n\n## B1\n\nbbbb bbbb\n\n## B2\n\ncccc cccc\n");
        let b2 = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("B2"))
            .expect("section");
        assert_eq!(b2.breadcrumb.as_deref(), Some("# A > ## B2"));
    }

    #[test]
    fn headings_inside_code_fences_are_not_headings() {
        let src = "# Real\n\ntext text text\n\n```sh\n# not a heading\necho hi\n```\n\nmore text\n";
        let out = chunk(src);
        assert_eq!(out.len(), 1, "got {out:?}");
        assert!(out[0].text.contains("# not a heading"));
    }

    #[test]
    fn preamble_before_the_first_heading_survives() {
        let out =
            chunk("Some preamble that is long enough to matter here.\n\n# Title\n\nbody body\n");
        assert!(out[0].symbol.is_none());
        assert_eq!(out[0].start_line, 1);
    }

    #[test]
    fn line_numbers_follow_the_source() {
        let out = chunk("# A\n\naaaaaaaaaaaaaaaa\n\n# B\n\nbbbbbbbbbbbbbbbb\n");
        let b = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("B"))
            .expect("section");
        assert_eq!(b.start_line, 5);
    }
}
