//! A read as the model reads it: where the lines are, then the lines; or the
//! files a glob names.

use findery_core::api::ReadResponse;
use findery_core::ids::ProjectId;
use findery_core::model::FileRecord;

use super::{fenced, notes};

/// Files a listing prints before it asks for a narrower glob.
const SHOWN_FILES: usize = 200;

/// `src/net.rs:L10-L48 of 120 · method Client > retry · from disk`, the host
/// path to open, then the lines.
pub fn lines(read: &ReadResponse) -> String {
    let mut out = notes(&read.warnings);
    let what = read
        .chunk
        .as_ref()
        .map(|chunk| match &chunk.symbol {
            Some(symbol) => format!(" · {} {symbol}", chunk.kind.as_str()),
            None => format!(" · {}", chunk.kind.as_str()),
        })
        .unwrap_or_default();
    out.push_str(&format!(
        "{} of {}{what} · from {}\nopen {}\n",
        read.locator(),
        read.total_lines,
        source_name(read),
        read.host_path
    ));
    out.push_str(&fenced(&read.path, read.lang, &read.text, &read.locator()));
    out
}

fn source_name(read: &ReadResponse) -> &'static str {
    match read.source {
        findery_core::api::ReadSource::Disk => "disk",
        findery_core::api::ReadSource::Index => "the index",
    }
}

/// The files a glob matched, one per line.
pub fn files(project: &ProjectId, glob: &str, files: &[FileRecord]) -> String {
    if files.is_empty() {
        return format!(
            "No indexed file of `{project}` matches `{glob}`. Globs are relative to the project \
             root (`src/**/*.rs`, `**/*.md`); `**` alone lists everything."
        );
    }
    let mut out = format!(
        "{} indexed file(s) of `{project}` match `{glob}`\n",
        files.len()
    );
    for file in files.iter().take(SHOWN_FILES) {
        out.push_str(&format!(
            "{} · {} · {} chunk(s)\n",
            file.path,
            file.lang.as_str(),
            file.chunk_count
        ));
    }
    let rest = files.len().saturating_sub(SHOWN_FILES);
    if rest > 0 {
        out.push_str(&format!("… and {rest} more; narrow the glob\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use findery_core::api::{ReadChunk, ReadSource};
    use findery_core::ids::{ChunkId, ContentHash};
    use findery_core::model::{ChunkKind, Lang, Origin};

    use super::*;

    fn project() -> ProjectId {
        ProjectId::new("axle").expect("id")
    }

    #[test]
    fn a_read_names_its_range_its_chunk_and_where_the_lines_came_from() {
        let read = ReadResponse {
            project: project(),
            path: "src/net.rs".into(),
            host_path: "C:/work/axle/src/net.rs".into(),
            lang: Lang::Rust,
            start_line: 10,
            end_line: 12,
            total_lines: 40,
            text: "fn a() {\n}\n".into(),
            source: ReadSource::Disk,
            chunk: Some(ReadChunk {
                id: ChunkId::derive(&project(), "src/net.rs", 0),
                kind: ChunkKind::Function,
                symbol: Some("a".into()),
                start_line: 10,
                end_line: 11,
            }),
            warnings: vec!["`src/net.rs` changed since it was indexed".into()],
        };
        let text = lines(&read);
        assert!(text.starts_with("note: `src/net.rs` changed"), "{text}");
        assert!(
            text.contains("src/net.rs:L10-L12 of 40 · function a · from disk"),
            "{text}"
        );
        assert!(
            text.contains("open C:/work/axle/src/net.rs\n```rust\nfn a() {\n}\n```"),
            "{text}"
        );
    }

    #[test]
    fn a_listing_is_capped_and_an_empty_one_explains_globs() {
        let record = |i: usize| FileRecord {
            path: format!("src/f{i}.rs"),
            lang: Lang::Rust,
            origin: Origin::File,
            content_hash: ContentHash::of(b"x"),
            size: 1,
            mtime_ns: 0,
            chunk_count: 2,
            indexed_at: 0,
        };
        let many: Vec<FileRecord> = (0..205).map(record).collect();
        let text = files(&project(), "src/**", &many);
        assert!(text.contains("src/f0.rs · rust · 2 chunk(s)"), "{text}");
        assert!(text.contains("… and 5 more"), "{text}");
        assert!(files(&project(), "x/**", &[]).contains("relative to the project root"));
    }
}
