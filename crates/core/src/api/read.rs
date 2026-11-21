//! What an agent reads after a search pointed it somewhere: one chunk, or the
//! lines around a locator (`GET /v1/projects/{id}/chunks`), and the indexed
//! files a glob names (`GET /v1/projects/{id}/files?glob=`).
//!
//! INTENT: a hit is an excerpt, cut to a budget. The next question is almost
//! always "show me the rest" or "what is around it", and answering it from the
//! index costs one round trip instead of a file read the agent has to locate.

use serde::{Deserialize, Serialize};

use crate::ids::{ChunkId, ProjectId};
use crate::model::{ChunkKind, Lang, Locator};
use crate::{Error, Result};

/// `GET /v1/projects/{id}/chunks` — one target, named one way.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadQuery {
    /// `path:L12-L48`, `path:12-48` or `path:L12`: those lines of the file.
    /// A chunk id is accepted here too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locator: Option<String>,
    /// A chunk id: that chunk's lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Lines added before and after the target; `search.max_read_context`
    /// caps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u32>,
}

impl ReadQuery {
    /// A read of `locator` with `context` lines around it.
    pub fn at(locator: impl Into<String>, context: u32) -> Self {
        Self {
            locator: Some(locator.into()),
            id: None,
            context: Some(context),
        }
    }

    /// What is read, parsed before anything is opened (CONV-023).
    pub fn target(&self) -> Result<Locator> {
        match (self.locator.as_deref(), self.id.as_deref()) {
            (Some(raw), None) => Locator::parse(raw),
            (None, Some(raw)) => match Locator::parse(raw)? {
                chunk @ Locator::Chunk(_) => Ok(chunk),
                Locator::Lines { .. } => Err(Error::Invalid(format!(
                    "`{raw}` is a locator, not a chunk id; pass it as `locator`"
                ))),
            },
            (Some(_), Some(_)) => Err(Error::Invalid(
                "name the target once: `locator` or `id`, not both".into(),
            )),
            (None, None) => Err(Error::Invalid(
                "a read needs a target: `locator` (path:L12-L48) or `id` (a chunk id)".into(),
            )),
        }
    }
}

/// Where the text of a read came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadSource {
    /// The file on the daemon's disk, as it is now.
    Disk,
    /// The chunks stored in the bucket, as they were last indexed — the only
    /// copy of a pushed or inline document.
    Index,
}

impl ReadSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Disk => "disk",
            Self::Index => "index",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "disk" => Some(Self::Disk),
            "index" => Some(Self::Index),
            _ => None,
        }
    }
}

crate::serde_via_parse!(ReadSource, option);

/// The chunk a read is about: the one named by id, or the one its lines
/// overlap most.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadChunk {
    pub id: ChunkId,
    pub kind: ChunkKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
}

/// The answer to a read: a line range of one indexed file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadResponse {
    pub project: ProjectId,
    /// Relative to the project root.
    pub path: String,
    /// What the user can open.
    pub host_path: String,
    pub lang: Lang,
    /// 1-indexed, both inclusive: the lines `text` holds.
    pub start_line: u32,
    pub end_line: u32,
    /// The file's last line, as far as the source knows it.
    pub total_lines: u32,
    pub text: String,
    pub source: ReadSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk: Option<ReadChunk>,
    /// Why the answer is not exactly what was asked: a range cut to the
    /// budget, a file that changed since it was indexed, lines no chunk holds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl ReadResponse {
    /// `path:L12-L40`, the form a search prints and `like` accepts.
    pub fn locator(&self) -> String {
        format!("{}:L{}-L{}", self.path, self.start_line, self.end_line)
    }
}

/// `GET /v1/projects/{id}/files` — every indexed file, or those a glob names.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesQuery {
    /// A path glob relative to the project root (`crates/**/*.rs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_locator_or_an_id_names_the_target_and_round_trips() {
        let lines = ReadQuery::at("src/a.rs:L3-L9", 2);
        assert!(matches!(
            lines.target().expect("locator"),
            Locator::Lines {
                start: 3,
                end: 9,
                ..
            }
        ));
        let wire = serde_json::to_string(&lines).expect("serialize");
        let back: ReadQuery = serde_json::from_str(&wire).expect("parse");
        assert_eq!(back, lines);

        let by_id = ReadQuery {
            id: Some("b".repeat(64)),
            ..ReadQuery::default()
        };
        assert!(matches!(by_id.target().expect("id"), Locator::Chunk(_)));
    }

    #[test]
    fn a_read_with_no_target_two_targets_or_a_locator_as_id_is_refused() {
        let none = ReadQuery::default();
        assert!(
            none.target()
                .expect_err("none")
                .to_string()
                .contains("locator")
        );
        let both = ReadQuery {
            id: Some("c".repeat(64)),
            ..ReadQuery::at("a.rs:L1", 0)
        };
        assert_eq!(both.target().expect_err("both").code(), "invalid");
        let misplaced = ReadQuery {
            id: Some("a.rs:L1-L2".into()),
            ..ReadQuery::default()
        };
        assert_eq!(misplaced.target().expect_err("as id").code(), "invalid");
        assert!(serde_json::from_str::<ReadQuery>(r#"{"loc":"a.rs:L1"}"#).is_err());
    }

    #[test]
    fn the_read_source_round_trips_and_refuses_an_unknown_name() {
        for source in [ReadSource::Disk, ReadSource::Index] {
            assert_eq!(ReadSource::parse(source.as_str()), Some(source));
            let wire = serde_json::to_string(&source).expect("serialize");
            assert_eq!(serde_json::from_str::<ReadSource>(&wire).ok(), Some(source));
        }
        assert!(serde_json::from_str::<ReadSource>(r#""cache""#).is_err());
    }
}
