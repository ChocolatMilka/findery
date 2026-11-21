//! Reading what a search pointed at: one chunk, the lines around a locator,
//! and the indexed files a glob names.
//!
//! INTENT: the text comes from where the truth is. A file the daemon walked is
//! read from its disk — the agent is about to edit it, so it wants the lines
//! as they are now — and only when the bucket lists it as walked. Every file
//! the daemon walks into the bucket passed the project's walk rules, a
//! targeted sync's included, so a read can never reach a file those rules
//! exclude (`.env`, `*.pem`, `.findery/`…). A pushed or inline document has no
//! copy here but its chunks, so it is rebuilt from them, line for line.

use std::path::Path;

use findery_core::api::{ReadChunk, ReadQuery, ReadResponse, ReadSource};
use findery_core::ids::{ContentHash, ProjectId};
use findery_core::model::filter::CompiledFilter;
use findery_core::model::{Chunk, FileRecord, Locator, Origin, Project, SearchFilter};
use findery_core::{Error, Result};

use crate::gates::{Access, Lease};
use crate::storage::families::BucketHandle;
use crate::{Indexer, sanitize_rel};

impl Indexer {
    /// The lines a [`ReadQuery`] names, with its context, within
    /// `search.max_read_lines`.
    pub async fn read(&self, id: &ProjectId, query: &ReadQuery) -> Result<ReadResponse> {
        let target = query.target()?;
        let limits = &self.cfg.search;
        let context = query.context.unwrap_or(0).min(limits.max_read_context);
        let lease = self.lease(id, Access::Read).await?;
        let Some(member) = crate::search::keyword_source(&lease).await? else {
            return Err(Error::NotFound(format!(
                "project `{id}` has nothing indexed yet; run `findery sync -p {id}`"
            )));
        };
        let wanted = Wanted::resolve(&lease, &member, &target).await?;
        let record = member.bucket.file_map(id).await?.remove(&wanted.path);

        let mut warnings = Vec::new();
        let page =
            match disk_text(&lease.project, &wanted.path, record.as_ref(), &mut warnings).await {
                Some(text) => Page::from_text(&text),
                None => Page::from_chunks(&wanted.chunks),
            };
        let (from, to) = page.window(&wanted, context, limits.max_read_lines, &mut warnings)?;
        let text = page.text(from, to, &mut warnings);
        let lang = wanted.chunks.first().map(|c| c.lang);
        Ok(ReadResponse {
            host_path: crate::search::host_path(&lease.project, &wanted.path),
            project: lease.project.id.clone(),
            lang: lang
                .or(record.map(|r| r.lang))
                .unwrap_or(findery_core::model::Lang::Other),
            path: wanted.path,
            start_line: from,
            end_line: to,
            total_lines: page.total(),
            text,
            source: page.source,
            chunk: wanted.chunk.map(|c| ReadChunk {
                id: c.id,
                kind: c.kind,
                symbol: c.breadcrumb.or(c.symbol),
                start_line: c.start_line,
                end_line: c.end_line,
            }),
            warnings,
        })
    }

    /// The indexed files, or those `glob` names — a pushing client's diff
    /// base when no glob is given, an agent's listing when one is.
    pub async fn files(&self, id: &ProjectId, glob: Option<&str>) -> Result<Vec<FileRecord>> {
        // Compiled before the lease: a broken glob is refused, not ignored.
        let filter = glob
            .map(|g| {
                CompiledFilter::new(&SearchFilter {
                    paths: vec![g.to_string()],
                    ..SearchFilter::default()
                })
            })
            .transpose()?;
        let mut files = self.file_manifest(id).await?;
        if let Some(filter) = filter {
            files.retain(|file| filter.matches_file(&file.path, file.lang));
        }
        Ok(files)
    }
}

/// What a read is about: a file, the lines asked, and that file's chunks.
struct Wanted {
    path: String,
    start: u32,
    end: u32,
    /// The chunk named by id, or the one the lines overlap most.
    chunk: Option<Chunk>,
    chunks: Vec<Chunk>,
}

impl Wanted {
    async fn resolve(lease: &Lease, member: &BucketHandle, target: &Locator) -> Result<Self> {
        let id = &lease.project.id;
        let (path, start, end) = match target {
            Locator::Chunk(_) => {
                let chunk = locate(lease, member, target).await?;
                (chunk.path, chunk.start_line, chunk.end_line)
            }
            Locator::Lines { path, start, end } => (sanitize_rel(path)?, *start, *end),
        };
        let chunks = member.bucket.chunks_of_file(id, &path).await?;
        if chunks.is_empty() {
            return Err(Error::NotFound(format!(
                "`{path}` is not indexed in project `{id}`; a read with a glob lists the files \
                 that are"
            )));
        }
        let chunk = most_overlapping(&chunks, start, end).cloned();
        Ok(Self {
            path,
            start,
            end,
            chunk,
            chunks,
        })
    }
}

/// A file's text from the daemon's disk, when the daemon walked it and can
/// still read it; `None` sends the read to the stored chunks.
async fn disk_text(
    project: &Project,
    path: &str,
    record: Option<&FileRecord>,
    warnings: &mut Vec<String>,
) -> Option<String> {
    let record = record.filter(|r| r.origin == Origin::File && !project.spec.push)?;
    let full = Path::new(&project.spec.root).join(path);
    let bytes = match tokio::fs::read(&full).await {
        Ok(bytes) if bytes.len() as u64 <= project.spec.max_file_bytes => bytes,
        Ok(_) => {
            warnings.push(format!(
                "`{path}` grew past the {}-byte limit; lines served from the index",
                project.spec.max_file_bytes
            ));
            return None;
        }
        Err(e) => {
            warnings.push(format!(
                "`{path}` cannot be read from disk ({e}); lines served from the index"
            ));
            return None;
        }
    };
    let Ok(text) = String::from_utf8(bytes) else {
        warnings.push(format!(
            "`{path}` is no longer UTF-8; lines served from the index"
        ));
        return None;
    };
    if ContentHash::of(text.as_bytes()) != record.content_hash {
        warnings.push(format!(
            "`{path}` changed since it was indexed; these are its lines now — run a sync \
             before searching it again"
        ));
    }
    Some(text)
}

/// Every line a source knows, 1-indexed; `None` for a line no chunk holds.
struct Page {
    lines: Vec<Option<String>>,
    source: ReadSource,
}

impl Page {
    fn from_text(text: &str) -> Self {
        Self {
            lines: text.lines().map(|l| Some(l.to_string())).collect(),
            source: ReadSource::Disk,
        }
    }

    /// The file as its chunks hold it. Overlapping windows agree on the lines
    /// they share, so the first chunk to cover a line is kept.
    fn from_chunks(chunks: &[Chunk]) -> Self {
        let last = chunks.iter().map(|c| c.end_line).max().unwrap_or(0);
        let mut lines: Vec<Option<String>> = vec![None; last as usize];
        for chunk in chunks {
            let first = chunk.start_line.max(1) as usize - 1;
            for (slot, line) in lines.iter_mut().skip(first).zip(chunk.text.lines()) {
                if slot.is_none() {
                    *slot = Some(line.to_string());
                }
            }
        }
        Self {
            lines,
            source: ReadSource::Index,
        }
    }

    fn total(&self) -> u32 {
        u32::try_from(self.lines.len()).unwrap_or(u32::MAX)
    }

    /// `[start - context, end + context]`, inside the file and the budget.
    fn window(
        &self,
        wanted: &Wanted,
        context: u32,
        max_lines: u32,
        warnings: &mut Vec<String>,
    ) -> Result<(u32, u32)> {
        let total = self.total();
        if wanted.start > total {
            return Err(Error::Invalid(format!(
                "`{}` has {total} lines; line {} is past its end",
                wanted.path, wanted.start
            )));
        }
        let from = wanted.start.saturating_sub(context).max(1);
        let mut to = wanted.end.saturating_add(context).min(total);
        if to - from + 1 > max_lines {
            to = from + max_lines - 1;
            warnings.push(format!(
                "cut at {max_lines} lines; read `{}:L{}` onwards for the rest",
                wanted.path,
                to + 1
            ));
        }
        Ok((from, to))
    }

    /// Lines `from..=to`. A line no chunk holds stays an empty line — the
    /// numbering must not shift — and is reported.
    fn text(&self, from: u32, to: u32, warnings: &mut Vec<String>) -> String {
        let mut out = Vec::with_capacity((to - from + 1) as usize);
        let mut gaps: Vec<(u32, u32)> = Vec::new();
        for number in from..=to {
            match self
                .lines
                .get(number as usize - 1)
                .and_then(Option::as_deref)
            {
                Some(line) => out.push(line),
                None => {
                    out.push("");
                    match gaps.last_mut() {
                        Some((_, end)) if *end + 1 == number => *end = number,
                        _ => gaps.push((number, number)),
                    }
                }
            }
        }
        if !gaps.is_empty() {
            let ranges: Vec<String> = gaps.iter().map(|(a, b)| format!("{a}-{b}")).collect();
            warnings.push(format!(
                "lines {} are in no chunk (blank, or below the chunker's minimum) and read empty",
                ranges.join(", ")
            ));
        }
        out.join("\n")
    }
}

/// The chunk `locator` names in `member`: by id, or the one of that file its
/// lines overlap most.
pub(crate) async fn locate(
    lease: &Lease,
    member: &BucketHandle,
    locator: &Locator,
) -> Result<Chunk> {
    let id = &lease.project.id;
    match locator {
        Locator::Chunk(chunk) => {
            let found = member
                .bucket
                .get_chunks(id, std::slice::from_ref(chunk))
                .await?;
            found
                .into_iter()
                .next()
                .ok_or_else(|| Error::NotFound(format!("project `{id}` has no chunk `{chunk}`")))
        }
        Locator::Lines { path, start, end } => {
            let path = sanitize_rel(path)?;
            let chunks = member.bucket.chunks_of_file(id, &path).await?;
            most_overlapping(&chunks, *start, *end)
                .cloned()
                .ok_or_else(|| {
                    Error::NotFound(format!(
                        "no chunk of `{path}` in project `{id}` covers lines {start}-{end}"
                    ))
                })
        }
    }
}

/// The chunk sharing the most lines with `start..=end`; the first one wins a
/// tie, so the same locator always names the same chunk.
fn most_overlapping(chunks: &[Chunk], start: u32, end: u32) -> Option<&Chunk> {
    let mut best: Option<(u32, &Chunk)> = None;
    for chunk in chunks {
        let overlap = overlap(chunk, start, end);
        let better = best.is_none_or(|(kept, _)| overlap > kept);
        if overlap > 0 && better {
            best = Some((overlap, chunk));
        }
    }
    best.map(|(_, chunk)| chunk)
}

fn overlap(chunk: &Chunk, start: u32, end: u32) -> u32 {
    let from = chunk.start_line.max(start);
    let to = chunk.end_line.min(end);
    if to < from { 0 } else { to - from + 1 }
}

#[cfg(test)]
mod tests {
    use findery_core::ids::ChunkId;
    use findery_core::model::{ChunkKind, Lang};

    use super::*;

    fn chunk(ordinal: u32, start: u32, end: u32, text: &str) -> Chunk {
        let project = ProjectId::new("demo").expect("id");
        Chunk {
            id: ChunkId::derive(&project, "a.rs", ordinal),
            project,
            path: "a.rs".into(),
            lang: Lang::Rust,
            ordinal,
            kind: ChunkKind::Function,
            symbol: None,
            breadcrumb: None,
            start_line: start,
            end_line: end,
            content_hash: ContentHash::of(&ordinal.to_le_bytes()),
            text: text.into(),
        }
    }

    fn wanted(start: u32, end: u32) -> Wanted {
        Wanted {
            path: "a.rs".into(),
            start,
            end,
            chunk: None,
            chunks: Vec::new(),
        }
    }

    #[test]
    fn a_line_range_names_the_chunk_it_overlaps_most() {
        let file = vec![
            chunk(0, 1, 10, ""),
            chunk(1, 11, 30, ""),
            chunk(2, 31, 40, ""),
        ];
        let pick = |start, end| most_overlapping(&file, start, end).map(|c| c.ordinal);
        assert_eq!(pick(8, 25), Some(1), "16 lines beat 3");
        assert_eq!(pick(5, 5), Some(0));
        assert_eq!(pick(1, 40), Some(1), "the longest wins a full cover");
        assert_eq!(pick(41, 50), None, "past the end names nothing");
    }

    #[test]
    fn chunks_rebuild_their_lines_and_a_gap_reads_empty_and_is_reported() {
        let page =
            Page::from_chunks(&[chunk(0, 1, 2, "fn a() {\n}"), chunk(1, 4, 5, "fn b() {\n}")]);
        let mut warnings = Vec::new();
        assert_eq!(page.text(1, 5, &mut warnings), "fn a() {\n}\n\nfn b() {\n}");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("lines 3-3"), "{warnings:?}");
    }

    #[test]
    fn context_stays_inside_the_file_and_a_long_range_is_cut_and_says_so() {
        let page = Page::from_text(&"x\n".repeat(50));
        let mut warnings = Vec::new();
        assert_eq!(
            page.window(&wanted(3, 4), 5, 100, &mut warnings)
                .expect("ok"),
            (1, 9)
        );
        assert_eq!(
            page.window(&wanted(48, 50), 5, 100, &mut warnings)
                .expect("ok"),
            (43, 50)
        );
        assert!(warnings.is_empty());
        assert_eq!(
            page.window(&wanted(1, 50), 0, 10, &mut warnings)
                .expect("ok"),
            (1, 10)
        );
        assert!(warnings[0].contains("a.rs:L11"), "{warnings:?}");
    }

    #[test]
    fn a_line_past_the_end_of_the_file_is_refused() {
        let page = Page::from_text("one\ntwo\n");
        let err = page
            .window(&wanted(9, 9), 0, 100, &mut Vec::new())
            .expect_err("past the end");
        assert_eq!(err.code(), "invalid");
        assert!(err.to_string().contains("2 lines"), "{err}");
    }
}
