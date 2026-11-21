//! The domain: projects, files, chunks, search, sync reports.
//!
//! INTENT: every type here is `Serialize + Deserialize` because it is EXACTLY
//! what the HTTP API and the MCP API expose. There is no second DTO family
//! derived from this one — one vocabulary, from the chunker up to Claude Code.

use crate::ids::{ChunkId, ContentHash, ProjectId};
pub mod filter;
mod project;
mod search;

pub use project::{DEFAULT_BUCKET, FileRecord, Origin, Project, ProjectSpec, is_bucket_name};
pub use search::{Resolved, ScoredChunk, SearchFilter, SearchHit, SearchMode, SearchRequest};

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
    /// JavaScript, and TypeScript with it: types are erased long before a brace
    /// is ever counted.
    #[serde(rename = "javascript")]
    JavaScript,
    Python,
    Php,
    Json,
    /// The Axle language (`.axle`).
    Axle,
    /// Java (`.java`).
    Java,
    /// Kotlin sources and scripts (`.kt`, `.kts`).
    Kotlin,
    /// Go (`.go`).
    Go,
    Other,
}

impl Lang {
    /// Every language, in declaration order. The registry, the MCP schema and
    /// the round-trip tests are checked against it.
    pub const ALL: &'static [Lang] = &[
        Lang::Rust,
        Lang::Markdown,
        Lang::Toml,
        Lang::JavaScript,
        Lang::Python,
        Lang::Php,
        Lang::Json,
        Lang::Axle,
        Lang::Java,
        Lang::Kotlin,
        Lang::Go,
        Lang::Other,
    ];

    /// Infers the language from the extension. Everything else is `Other` and
    /// goes through the generic chunker.
    ///
    /// INTENT: `.jsx` and `.tsx` are absent on purpose — JSX text may hold an
    /// apostrophe (`<p>don't</p>`), which a lexical scanner reads as a string
    /// opener and lets run across the markup. They take the text fallback.
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
            "js" | "mjs" | "cjs" | "ts" | "mts" | "cts" => Self::JavaScript,
            "py" | "pyi" => Self::Python,
            "php" => Self::Php,
            "json" | "jsonc" => Self::Json,
            "axle" => Self::Axle,
            "java" => Self::Java,
            "kt" | "kts" => Self::Kotlin,
            "go" => Self::Go,
            _ => Self::Other,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Markdown => "markdown",
            Self::Toml => "toml",
            Self::JavaScript => "javascript",
            Self::Python => "python",
            Self::Php => "php",
            Self::Json => "json",
            Self::Axle => "axle",
            Self::Java => "java",
            Self::Kotlin => "kotlin",
            Self::Go => "go",
            Self::Other => "other",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "rust" => Some(Self::Rust),
            "markdown" => Some(Self::Markdown),
            "toml" => Some(Self::Toml),
            "javascript" | "js" | "typescript" | "ts" => Some(Self::JavaScript),
            "python" => Some(Self::Python),
            "php" => Some(Self::Php),
            "json" => Some(Self::Json),
            "axle" => Some(Self::Axle),
            "java" => Some(Self::Java),
            "kotlin" | "kt" => Some(Self::Kotlin),
            "go" | "golang" => Some(Self::Go),
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
    /// Free-standing `fn`, `def` or `function`.
    Function,
    /// A `fn` / `function` / `def` inside a class, an `impl` or a `trait`.
    Method,
    /// A `struct`, or the `class` of JavaScript, PHP and Python.
    Struct,
    Enum,
    Trait,
    /// Whole `impl` block (small enough to stay together).
    Impl,
    /// Inline `mod`, or a module docstring.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_round_trips_through_its_name_and_its_serde_form() {
        for lang in Lang::ALL {
            assert_eq!(Lang::parse(lang.as_str()), Some(*lang), "{lang:?}");
            let json = serde_json::to_string(lang).expect("a unit variant serializes");
            assert_eq!(json, format!("\"{}\"", lang.as_str()), "{lang:?}");
            let back: Lang = serde_json::from_str(&json).expect("its own form parses");
            assert_eq!(back, *lang);
        }
    }

    #[test]
    fn java_kotlin_and_go_are_read_from_their_extensions() {
        assert_eq!(Lang::from_path("src/App.java"), Lang::Java);
        assert_eq!(Lang::from_path("src/App.kt"), Lang::Kotlin);
        assert_eq!(Lang::from_path("build.gradle.kts"), Lang::Kotlin);
        assert_eq!(Lang::from_path("cmd/main.go"), Lang::Go);
        assert_eq!(Lang::from_path("cmd/main_test.GO"), Lang::Go);
    }

    #[test]
    fn an_unknown_language_name_is_refused() {
        assert_eq!(Lang::parse("golang-ish"), None);
        assert_eq!(Lang::parse("Java"), None);
    }
}
