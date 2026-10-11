//! The domain: projects, files, chunks, search, sync reports.
//!
//! INTENT: every type here is `Serialize + Deserialize` because it is EXACTLY
//! what the HTTP API and the MCP API expose. There is no second DTO family
//! derived from this one — one vocabulary, from the chunker up to Claude Code.

use crate::ids::{ChunkId, ContentHash, ProjectId};
mod search;

pub use search::{ScoredChunk, SearchFilter, SearchHit, SearchMode, SearchRequest};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Language
// ---------------------------------------------------------------------------

/// Language of an indexed file. Drives chunker selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lang {
    Rust,
    Markdown,
    Toml,
    Other,
}

impl Lang {
    /// Infers the language from the extension. Everything else is `Other` and
    /// goes through the generic chunker.
    pub fn from_path(path: &str) -> Self {
        let ext = path
            .rsplit('.')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        match ext.as_str() {
            "rs" => Self::Rust,
            "md" | "markdown" | "mdx" => Self::Markdown,
            "toml" => Self::Toml,
            _ => Self::Other,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Markdown => "markdown",
            Self::Toml => "toml",
            Self::Other => "other",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "rust" => Some(Self::Rust),
            "markdown" => Some(Self::Markdown),
            "toml" => Some(Self::Toml),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Chunks
// ---------------------------------------------------------------------------

/// Syntactic nature of a chunk. Doubles as a search filter ("functions only")
/// and as a reranking signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkKind {
    /// Whole file (small file kept in one piece).
    File,
    /// Free-standing `fn`.
    Function,
    /// `fn` inside an `impl` or a `trait`.
    Method,
    Struct,
    Enum,
    Trait,
    /// Whole `impl` block (small enough to stay together).
    Impl,
    /// Inline `mod`.
    Module,
    /// `macro_rules!` or a procedural macro entry point.
    Macro,
    /// Item under `#[cfg(test)]`.
    Test,
    /// Constant, static or type alias.
    Const,
    /// Markdown section delimited by a heading.
    DocSection,
    /// Plain text window (fallback, or an oversized body split again).
    Text,
}

impl ChunkKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Function => "function",
            Self::Method => "method",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Impl => "impl",
            Self::Module => "module",
            Self::Macro => "macro",
            Self::Test => "test",
            Self::Const => "const",
            Self::DocSection => "doc_section",
            Self::Text => "text",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "file" => Some(Self::File),
            "function" => Some(Self::Function),
            "method" => Some(Self::Method),
            "struct" => Some(Self::Struct),
            "enum" => Some(Self::Enum),
            "trait" => Some(Self::Trait),
            "impl" => Some(Self::Impl),
            "module" => Some(Self::Module),
            "macro" => Some(Self::Macro),
            "test" => Some(Self::Test),
            "const" => Some(Self::Const),
            "doc_section" => Some(Self::DocSection),
            "text" => Some(Self::Text),
            _ => None,
        }
    }
}

/// Output of a `Chunker`: a fragment WITHOUT project identity.
///
/// INTENT: the chunker knows neither the project nor the hash — it splits, full
/// stop. Identity is stamped by the indexing layer (L2), the single writer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawChunk {
    pub kind: ChunkKind,
    /// Short symbol name (`derive`, `ChunkId`, `Installation`).
    pub symbol: Option<String>,
    /// Readable semantic path (`impl ChunkId > fn derive`, `# API > ## Sync`).
    pub breadcrumb: Option<String>,
    /// 1-indexed line bounds, both inclusive.
    pub start_line: u32,
    pub end_line: u32,
    pub text: String,
}

/// An indexed fragment, as stored in the bucket and returned by search.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Chunk {
    pub id: ChunkId,
    pub project: ProjectId,
    /// Path relative to the project root, always with `/` separators.
    pub path: String,
    pub lang: Lang,
    /// Position of the chunk inside its file (0-indexed). Together with `path`
    /// this is the positional key that makes `ChunkId` stable.
    pub ordinal: u32,
    pub kind: ChunkKind,
    pub symbol: Option<String>,
    pub breadcrumb: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    /// Fingerprint of the embedded text — the vector cache key.
    pub content_hash: ContentHash,
    pub text: String,
}

impl Chunk {
    /// The text actually sent to the embedding model.
    ///
    /// INTENT: path and breadcrumb carry a lot of signal in code search ("where
    /// is rate limiting handled" finds `middleware/rate_limit.rs` through the
    /// path as much as through the body), so they are always prefixed.
    pub fn embed_input(&self) -> String {
        let mut out = String::with_capacity(self.text.len() + 128);
        out.push_str("path: ");
        out.push_str(&self.path);
        out.push('\n');
        out.push_str("lang: ");
        out.push_str(self.lang.as_str());
        out.push('\n');
        if let Some(bc) = &self.breadcrumb {
            out.push_str("symbol: ");
            out.push_str(bc);
            out.push('\n');
        }
        out.push_str("---\n");
        out.push_str(&self.text);
        out
    }

    /// Citable reference for an agent answer: `path:start-end`.
    pub fn locator(&self) -> String {
        format!("{}:{}-{}", self.path, self.start_line, self.end_line)
    }
}

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

// ---------------------------------------------------------------------------
// Synchronization
// ---------------------------------------------------------------------------

/// Breadth of a synchronization run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    /// Re-embeds only what changed hash. The default.
    #[default]
    Incremental,
    /// Ignores known state and rebuilds everything. The vector cache still
    /// applies: a `Full` run over an unchanged repository costs no embedding.
    Full,
    /// Touches only the listed paths — targeted add or refresh.
    Paths(Vec<String>),
}

impl SyncMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Incremental => "incremental",
            Self::Full => "full",
            Self::Paths(_) => "paths",
        }
    }
}

/// What a sync run actually did. Every field is a measured fact, never an
/// estimate.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncReport {
    pub project: String,
    pub mode: String,
    pub files_scanned: u64,
    pub files_unchanged: u64,
    pub files_added: u64,
    pub files_updated: u64,
    pub files_removed: u64,
    pub files_skipped: u64,
    pub chunks_written: u64,
    pub chunks_deleted: u64,
    /// Vectors genuinely computed by the model.
    pub embeddings_computed: u64,
    /// Vectors served from the content-addressed cache.
    pub embeddings_cached: u64,
    pub duration_ms: u64,
    /// Non-fatal failures (unreadable file, too large, not UTF-8).
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// Snapshot of the index, for `status` and `/healthz`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexStats {
    pub project: String,
    /// Bucket holding it, so a `status` on one of several indexes says which.
    pub bucket: String,
    pub files: u64,
    pub chunks: u64,
    pub vectors: u64,
    pub dim: usize,
    pub model: String,
    pub store_backend: String,
    pub last_sync_at: Option<i64>,
    pub disk_bytes: u64,
}
