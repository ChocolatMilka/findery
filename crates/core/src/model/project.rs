//! Files and projects: what a project is, and what it knows about its files.
//!
//! INTENT: split from `model/mod.rs`, which had grown to hold the whole
//! vocabulary. The cut is the one the code already made — a section header —
//! and it is a real one: everything here is `Serialize + Deserialize` because
//! it crosses the wire, and everything here is about a *project*, while what
//! stayed behind is about chunks and runs.

use crate::ids::{ContentHash, ProjectId};
use crate::model::Lang;
use crate::model::search::SearchFilter;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Files and projects
// ---------------------------------------------------------------------------

/// Where a bucket file comes from, and therefore who may delete it.
///
/// INTENT: two sides can feed the index — the daemon walking a directory it can
/// see, or a client walking it and pushing the contents. Each may only prune
/// what it put there. Without this distinction a client sync would sweep away
/// the daemon's files, and vice versa.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// The daemon walked it. Pruned by a daemon-side sync.
    File,
    /// A client walked it and pushed its contents. Pruned by that client's
    /// `prune` call, never by a daemon-side sync.
    Pushed,
    /// Added by hand through the API. Nothing prunes it automatically.
    Inline,
}

impl Origin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Pushed => "pushed",
            Self::Inline => "inline",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "file" => Some(Self::File),
            "pushed" => Some(Self::Pushed),
            "inline" => Some(Self::Inline),
            _ => None,
        }
    }
}

/// Known state of an indexed file. What sync compares against the disk.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileRecord {
    pub path: String,
    pub lang: Lang,
    pub origin: Origin,
    pub content_hash: ContentHash,
    pub size: u64,
    /// mtime in nanoseconds since the epoch — the fast path of sync.
    pub mtime_ns: i64,
    pub chunk_count: u32,
    pub indexed_at: i64,
}

/// What a client declares when registering a project.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectSpec {
    /// Root as the server sees it (container path when dockerized).
    pub root: String,
    /// Include globs; empty means everything that passes the extension filter.
    #[serde(default)]
    pub include: Vec<String>,
    /// Exclude globs, applied after `include`.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Retained extensions, without the leading dot.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Honour `.gitignore` / `.ignore` during the walk.
    #[serde(default = "default_true")]
    pub follow_gitignore: bool,
    /// Descend into dot-directories and index dotfiles.
    ///
    /// INTENT: defaults to true, because `.github/workflows`, `.cargo/config.toml`
    /// and `.claude/docs` are exactly the files a developer asks about. Skipping
    /// them silently is the opposite of control — and the default `exclude` list
    /// is what keeps the secret-bearing dotfiles out.
    #[serde(default = "default_true")]
    pub include_hidden: bool,
    /// Per-file ceiling; above it the file is skipped and reported.
    #[serde(default = "default_max_file_bytes")]
    pub max_file_bytes: u64,
    /// The daemon cannot see `root`, so contents arrive by push.
    ///
    /// Set at registration by testing the path, not by the caller: whether the
    /// daemon can read a directory is a fact about the deployment, not an
    /// opinion the client should have to hold.
    #[serde(default)]
    pub push: bool,
    /// Named restrictions a search can ask for by name.
    ///
    /// INTENT: they narrow the SAME index — a scope is a filter applied before
    /// scoring, never a second index and never a limit applied after the KNN.
    /// Declared at registration so the daemon can resolve a name for a client
    /// that never read the manifest it came from.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub scopes: BTreeMap<String, SearchFilter>,
    /// The retrieval intent this project asks for by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<crate::prompt::QueryTask>,
}

impl ProjectSpec {
    /// The instruction a query in this project gets when the request says
    /// nothing. Mirrors `SearchRequest::resolved_instruction`, and the two are
    /// read in that order by `SearchRequest::resolve`.
    pub fn resolved_instruction(&self) -> Option<&str> {
        match (&self.instruction, self.task) {
            (Some(free), _) if !free.trim().is_empty() => Some(free),
            (_, Some(task)) => Some(task.instruction()),
            _ => None,
        }
    }

    /// The scope a name refers to, or the sentence that says which names exist.
    pub fn scope(&self, name: &str) -> Result<&SearchFilter> {
        self.scopes.get(name).ok_or_else(|| {
            Error::Invalid(if self.scopes.is_empty() {
                format!("project has no scope `{name}`; a scope is declared in the manifest")
            } else {
                format!(
                    "unknown scope `{name}`; this project declares: {}",
                    self.scopes.keys().cloned().collect::<Vec<_>>().join(", ")
                )
            })
        })
    }
}

fn default_true() -> bool {
    true
}

fn default_max_file_bytes() -> u64 {
    2 * 1024 * 1024
}

impl Default for ProjectSpec {
    fn default() -> Self {
        Self {
            root: String::new(),
            include: Vec::new(),
            exclude: Vec::new(),
            extensions: vec!["rs".into(), "md".into()],
            follow_gitignore: true,
            include_hidden: true,
            max_file_bytes: default_max_file_bytes(),
            push: false,
            scopes: BTreeMap::new(),
            instruction: None,
            task: None,
        }
    }
}

/// Bucket a project joins when nothing names one.
pub const DEFAULT_BUCKET: &str = "default";

/// True when `name` can name a bucket.
///
/// INTENT: a bucket becomes a directory name under the data directory, so the
/// alphabet is deliberately narrow. Refusing here — rather than escaping at the
/// filesystem call — is what keeps `findery bucket rm ../../etc` an error
/// instead of a deletion.
pub fn is_bucket_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// A project registered with the daemon.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    /// Free-form display name.
    pub name: String,
    /// Bucket holding this project's chunks and vectors.
    ///
    /// INTENT: the bucket is a property of the project rather than a flag on
    /// every command, so `findery search` in a directory never has to be told
    /// which index it is standing in. `init --bucket` is the only place a
    /// choice is made.
    ///
    /// It is stamped when the project is read out of its bucket, never stored:
    /// which bucket a project is in *is* which file holds its row, and a column
    /// could disagree with that.
    #[serde(default = "default_bucket")]
    pub bucket: String,
    pub spec: ProjectSpec,
    /// Host-side root, so clients can cite paths the user can actually open.
    pub host_root: String,
    pub created_at: i64,
    pub updated_at: i64,
}

fn default_bucket() -> String {
    DEFAULT_BUCKET.to_string()
}
