//! Markdown chunker: one chunk per heading section, breadcrumbs carrying the
//! full heading ancestry so a fragment stays self-locating out of context.

use crate::text::{count_lines, drop_empty, merge_small, split_windows};
use findery_core::Result;
use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, Lang, RawChunk};
use findery_core::traits::Chunker;

/// Splits Markdown into heading sections.
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

impl Section {
    fn at(start_line: u32) -> Self {
        Self {
            title: None,
            breadcrumb: None,
            start_line,
            text: String::new(),
        }
    }
}

/// An open code fence: its character and the length of its opening run.
type Fence = (char, usize);

/// Splits on headings, ignoring anything inside a fenced block, an indented
/// code block or a front-matter block.
///
/// INTENT: a `# comment` line inside a ```` ```rust ```` fence, or inside a
/// four-space indented sample, is not a heading — treating it as one shatters
/// every code sample in the documentation.
fn split_sections(text: &str) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    let mut ancestry: Vec<(u8, String)> = Vec::new();
    let mut current = Section::at(1);
    let mut fence: Option<Fence> = None;
    let mut front_matter: Option<&str> = None;
    // Byte length of the line just appended, so a Setext underline can take it
    // back out and make it a title.
    let mut previous: Option<usize> = None;

    for (i, line) in text.split_inclusive('\n').enumerate() {
        let line_no = (i + 1) as u32;
        let indent = indent_width(line);
        let body = line.trim_start().trim_end_matches(['\n', '\r']);

        // Front matter only if it CLOSES. A document opening on `---` with no
        // second `---` opens with a thematic break, and swallowing the rest of
        // the file into an opaque block would lose every heading it contains.
        if i == 0
            && fence.is_none()
            && matches!(body, "---" | "+++")
            && closes_front_matter(text, body)
        {
            front_matter = Some(body);
            current.text.push_str(line);
            previous = None;
            continue;
        }
        if let Some(delimiter) = front_matter {
            if body == delimiter {
                front_matter = None;
            }
            current.text.push_str(line);
            previous = None;
            continue;
        }
        if let Some((ch, run)) = fence {
            if indent <= 3 && is_closing_fence(body, ch, run) {
                fence = None;
            }
            current.text.push_str(line);
            previous = None;
            continue;
        }
        if indent <= 3
            && let Some(opened) = opening_fence(body)
        {
            fence = Some(opened);
            current.text.push_str(line);
            previous = None;
            continue;
        }

        let atx = if indent <= 3 { atx_heading(body) } else { None };
        if let Some((level, title)) = atx {
            open_heading(
                &mut current,
                &mut sections,
                &mut ancestry,
                level,
                title,
                line_no,
            );
            current.text.push_str(line);
            previous = None;
            continue;
        }
        // A Setext title is the line ABOVE its underline: it has already been
        // appended, and comes back out to head its own section.
        if indent <= 3
            && let Some(level) = setext_level(body)
            && let Some(len) = previous
            && let Some(title) = setext_title(&current.text[current.text.len() - len..])
        {
            let title_line = line_no - 1;
            let title_text = current.text[current.text.len() - len..].to_string();
            current.text.truncate(current.text.len() - len);
            open_heading(
                &mut current,
                &mut sections,
                &mut ancestry,
                level,
                title,
                title_line,
            );
            current.text.push_str(&title_text);
            current.text.push_str(line);
            previous = None;
            continue;
        }

        current.text.push_str(line);
        previous = Some(line.len());
    }
    if !current.text.trim().is_empty() {
        sections.push(current);
    }
    sections
}

/// Whether the delimiter opening the file is closed further down, which is what
/// makes it front matter rather than a thematic break.
fn closes_front_matter(text: &str, delimiter: &str) -> bool {
    let mut lines = text.split_inclusive('\n');
    lines.next();
    lines.any(|line| line.trim_start().trim_end_matches(['\n', '\r']) == delimiter)
}

/// Closes the running section and opens one headed by `title`.
fn open_heading(
    current: &mut Section,
    sections: &mut Vec<Section>,
    ancestry: &mut Vec<(u8, String)>,
    level: u8,
    title: String,
    start_line: u32,
) {
    if !current.text.trim().is_empty() {
        sections.push(std::mem::replace(current, Section::at(start_line)));
    } else {
        *current = Section::at(start_line);
    }
    ancestry.retain(|(l, _)| *l < level);
    ancestry.push((level, title.clone()));
    let breadcrumb = ancestry
        .iter()
        .map(|(l, t)| format!("{} {}", "#".repeat(*l as usize), t))
        .collect::<Vec<_>>()
        .join(" > ");
    current.title = Some(title);
    current.breadcrumb = Some(breadcrumb);
}

/// Leading whitespace width, a tab counting as four columns. Four columns make
/// an indented code block, where `#` is a comment and not a heading.
fn indent_width(line: &str) -> usize {
    let mut width = 0usize;
    for c in line.chars() {
        match c {
            ' ' => width += 1,
            '\t' => width += 4,
            _ => break,
        }
    }
    width
}

/// The fence this line opens, if it opens one.
fn opening_fence(body: &str) -> Option<Fence> {
    let ch = body.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let run = body.chars().take_while(|c| *c == ch).count();
    if run < 3 {
        return None;
    }
    // A backtick fence's info string may not itself contain a backtick.
    if ch == '`' && body[run..].contains('`') {
        return None;
    }
    Some((ch, run))
}

/// Whether this line closes a fence opened with `run` characters of `ch`.
///
/// INTENT: a closing fence carries nothing but whitespace, so ```` ```rust ````
/// inside a block is content, not a closure — closing on it loses the rest of
/// the block.
fn is_closing_fence(body: &str, ch: char, run: usize) -> bool {
    let n = body.chars().take_while(|c| *c == ch).count();
    n >= run && body.chars().skip(n).all(|c| c == ' ' || c == '\t')
}

/// Parses an ATX heading: 1..=6 `#` followed by whitespace.
fn atx_heading(body: &str) -> Option<(u8, String)> {
    let level = body.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &body[level..];
    if !rest.starts_with([' ', '\t']) {
        return None;
    }
    let title = strip_closing_hashes(rest.trim());
    if title.is_empty() {
        None
    } else {
        Some((level as u8, title.to_string()))
    }
}

/// Drops the closing `###` sequence of an ATX heading — only where it stands
/// alone, so that `## C#` keeps its name.
fn strip_closing_hashes(s: &str) -> &str {
    let t = s.trim_end();
    let hashes = t.chars().rev().take_while(|c| *c == '#').count();
    if hashes == 0 {
        return t;
    }
    let cut = t.len() - hashes;
    // The start of the line counts as the space the sequence must follow, so
    // `## ###` is an EMPTY heading — and an unnamed section is not a section.
    if cut == 0 || t[..cut].ends_with([' ', '\t']) {
        t[..cut].trim_end()
    } else {
        t
    }
}

/// The level of a Setext underline: `===` is H1, `---` is H2.
fn setext_level(body: &str) -> Option<u8> {
    let t = body.trim_end();
    if t.is_empty() {
        return None;
    }
    if t.chars().all(|c| c == '=') {
        Some(1)
    } else if t.chars().all(|c| c == '-') {
        Some(2)
    } else {
        None
    }
}

/// The title a Setext underline claims: the paragraph line above it, which must
/// not itself be a heading, a list item, a marker or an indented code line.
fn setext_title(raw: &str) -> Option<String> {
    if indent_width(raw) > 3 {
        return None;
    }
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    let forbidden = ['#', '-', '=', '*', '+', '>', '|', '_'];
    if t.starts_with(forbidden) || t.starts_with("```") || t.starts_with("~~~") {
        return None;
    }
    Some(t.to_string())
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
    fn an_info_string_does_not_close_a_fence() {
        let src = "```rust\n# not a heading\n```rust\nlet x = 1;\n```\n\nafter the block\n";
        let out = chunk(src);
        assert_eq!(out.len(), 1, "got {out:?}");
        assert!(out[0].text.contains("let x = 1"));
        assert!(out[0].text.contains("after the block"));
    }

    #[test]
    fn an_indented_hash_is_a_comment_not_a_heading() {
        let src = "# Real\n\ntext text text\n\n    # a shell comment\n    echo hi\n";
        let out = chunk(src);
        assert_eq!(out.len(), 1, "got {out:?}");
        assert!(out[0].text.contains("# a shell comment"));
    }

    #[test]
    fn a_setext_underline_heads_the_line_above_it() {
        let src = "Install\n=======\n\ncargo add it\n\nNotes\n-----\n\nsome notes\n";
        let out = chunk(src);
        let install = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("Install"))
            .expect("h1");
        assert_eq!(install.start_line, 1);
        assert!(
            install.text.starts_with("Install\n======="),
            "{:?}",
            install.text
        );
        let notes = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("Notes"))
            .expect("h2");
        assert_eq!(notes.breadcrumb.as_deref(), Some("# Install > ## Notes"));
    }

    #[test]
    fn a_thematic_break_is_not_a_heading() {
        let out = chunk("# A\n\nbody body\n\n---\n\nmore body\n");
        assert_eq!(out.len(), 1, "got {out:?}");
        assert!(out[0].text.contains("more body"));
    }

    #[test]
    fn a_document_opening_on_a_rule_keeps_its_headings() {
        // `---` on the first line with no matching `---` is a thematic break,
        // not front matter: reading it as front matter swallows the document.
        let out = chunk("---\n# Title\n\nbody body\n\n## Next\n\nmore body\n");
        let title = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("Title"))
            .expect("heading");
        assert_eq!(title.breadcrumb.as_deref(), Some("# Title"));
        assert!(out.iter().any(|c| c.symbol.as_deref() == Some("Next")));
    }

    #[test]
    fn front_matter_is_not_a_heading_and_survives() {
        let src = "---\ntitle: Guide\n---\n\n# Real\n\nbody body\n";
        let out = chunk(src);
        let real = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("Real"))
            .expect("heading");
        assert_eq!(real.start_line, 5);
        assert!(out[0].text.contains("title: Guide"));
    }

    #[test]
    fn a_closing_hash_sequence_is_dropped_but_a_name_is_not() {
        assert_eq!(atx_heading("## Title ##"), Some((2, "Title".into())));
        assert_eq!(atx_heading("## C#"), Some((2, "C#".into())));
        assert_eq!(
            atx_heading("## ###"),
            None,
            "an empty heading names nothing"
        );
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
