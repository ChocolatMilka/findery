//! A chunk as a caller names it: by where it is, or by its id.
//!
//! The forms accepted are the ones a search prints, so a hit can be pasted
//! back as `like`: `src/retry.rs:12-48`, `src/retry.rs:L12-L48`,
//! `src/retry.rs:L12`, or the 64-hex chunk id.

use crate::ids::ChunkId;
use crate::{Error, Result};

/// Length of a chunk id: a blake3 digest in hex.
const CHUNK_ID_LEN: usize = 64;

/// Where a chunk is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Locator {
    Chunk(ChunkId),
    /// A line range of a file, relative to the project root. The chunk meant
    /// is the one of that file that overlaps the range most.
    Lines {
        path: String,
        start: u32,
        end: u32,
    },
}

impl Locator {
    /// Parses one of the forms in the module docs.
    pub fn parse(raw: &str) -> Result<Self> {
        let text = raw.trim();
        if is_chunk_id(text) {
            return Ok(Self::Chunk(ChunkId::from_hex(text)));
        }
        let parsed = text
            .rsplit_once(':')
            .and_then(|(path, lines)| Some((path, parse_lines(lines)?)));
        match parsed {
            Some((path, (start, end))) if !path.is_empty() => Ok(Self::Lines {
                path: path.to_string(),
                start,
                end,
            }),
            _ => Err(Error::Invalid(format!(
                "`{text}` is not a locator; write `path:L12-L48`, `path:12-48`, `path:L12` or a \
                 chunk id"
            ))),
        }
    }
}

fn is_chunk_id(text: &str) -> bool {
    text.len() == CHUNK_ID_LEN && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `12-48`, `L12-L48` or `L12`; a range that ends before it starts is refused.
fn parse_lines(raw: &str) -> Option<(u32, u32)> {
    let (start, end) = match raw.split_once('-') {
        Some((start, end)) => (line(start)?, line(end)?),
        None => {
            let only = line(raw)?;
            (only, only)
        }
    };
    (start >= 1 && start <= end).then_some((start, end))
}

fn line(raw: &str) -> Option<u32> {
    let digits = raw.strip_prefix('L').unwrap_or(raw);
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_printed_form_and_a_chunk_id_parse() {
        let lines = |path: &str, start, end| Locator::Lines {
            path: path.into(),
            start,
            end,
        };
        assert_eq!(
            Locator::parse("src/a.rs:12-48").expect("printed form"),
            lines("src/a.rs", 12, 48)
        );
        assert_eq!(
            Locator::parse("src/a.rs:L12-L48").expect("L form"),
            lines("src/a.rs", 12, 48)
        );
        assert_eq!(
            Locator::parse(" src/a.rs:L7 ").expect("one line"),
            lines("src/a.rs", 7, 7)
        );
        let id = "a".repeat(64);
        assert_eq!(
            Locator::parse(&id).expect("id"),
            Locator::Chunk(ChunkId::from_hex(id))
        );
    }

    #[test]
    fn a_path_without_lines_or_a_backwards_range_is_refused() {
        for raw in [
            "src/a.rs",
            "src/a.rs:48-12",
            ":1-2",
            "src/a.rs:L0",
            "src/a.rs:x",
        ] {
            let err = Locator::parse(raw).expect_err(raw);
            assert_eq!(err.code(), "invalid", "{raw}");
        }
    }
}
