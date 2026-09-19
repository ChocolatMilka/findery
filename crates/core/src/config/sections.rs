//! The configuration sections: one struct per concern, each with the defaults
//! that make it work out of the box.
//!
//! INTENT: split out of `config/mod.rs`, which owns parsing, the environment
//! overlay and validation. Those are one responsibility; describing every knob
//! of every subsystem is another (CONV-029).

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// `host:port` the HTTP API binds to.
    pub bind: String,
    /// When set, every request must carry `Authorization: Bearer <token>`.
    pub api_token: Option<String>,
    /// Maximum accepted request body.
    pub max_body_bytes: usize,
    /// How many sync jobs may run at the same time.
    pub job_concurrency: usize,
    /// How many finished jobs stay queryable.
    pub job_history: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8077".into(),
            api_token: None,
            max_body_bytes: 32 * 1024 * 1024,
            job_concurrency: 2,
            job_history: 64,
        }
    }
}

/// Which embedding runtime the daemon talks to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EmbedderBackend {
    /// Any OpenAI-compatible `/v1/embeddings` endpoint: TEI, llama.cpp server,
    /// vLLM, Ollama, LM Studio, Infinity.
    #[default]
    OpenAiCompat,
    /// Deterministic hash-based vectors. Tests and offline smoke runs only.
    Mock,
}

impl EmbedderBackend {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "openai" | "openai_compat" | "tei" | "llamacpp" | "ollama" => Ok(Self::OpenAiCompat),
            "mock" => Ok(Self::Mock),
            other => Err(Error::Config(format!("unknown embedder backend `{other}`"))),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenAiCompat => "openai_compat",
            Self::Mock => "mock",
        }
    }
}

crate::serde_via_parse!(EmbedderBackend, result);

/// How the daemon reaches an embedding model.
///
/// INTENT: nothing about the model is compiled in. Backend, URL, route, auth
/// header, model name and width are all data. The defaults happen to describe
/// the bundled local Qwen3 container; pointing `base_url` at api.openai.com,
/// api.voyageai.com or a remote TEI behind TLS is a configuration change, not a
/// code change.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmbedderConfig {
    pub backend: EmbedderBackend,
    /// Base URL, without the route below. Local or remote, http or https.
    pub base_url: String,
    /// Route appended to `base_url`. OpenAI, TEI, vLLM, Ollama and LM Studio
    /// all serve `/v1/embeddings`; some gateways do not.
    pub path: String,
    /// Model identifier forwarded to the runtime, verbatim.
    pub model: String,
    /// Secret for the auth header. Prefer `FINDERY_EMBED_API_KEY` over the file.
    pub api_key: Option<String>,
    /// Header carrying the secret. Azure OpenAI wants `api-key`, most others
    /// `Authorization`.
    pub auth_header: String,
    /// Prefix prepended to the secret inside that header.
    pub auth_prefix: String,
    /// Extra static headers (gateway routing, API versioning, tenancy).
    #[serde(default)]
    pub extra_headers: BTreeMap<String, String>,
    /// Expected vector width. `0` means: ask the runtime at startup.
    pub dim: usize,
    /// Send `dimensions` in the request body. Matryoshka-capable models
    /// (Qwen3-Embedding, OpenAI text-embedding-3-*) honour it and return a
    /// truncated vector; models that do not will reject the field.
    pub send_dimensions: bool,
    /// Probe the runtime once at startup and fail fast when the returned width
    /// disagrees with `dim`. Half a corpus at the wrong width is unsearchable
    /// and silent, so this defaults to on.
    pub verify_dim: bool,
    /// Inputs per HTTP request.
    pub batch_size: usize,
    /// Concurrent in-flight batches.
    pub concurrency: usize,
    pub timeout_secs: u64,
    /// Retries on a timeout, a connection error or a 429/5xx.
    pub max_retries: u32,
    /// Inputs longer than this are truncated before being sent.
    pub max_input_chars: usize,
    /// L2-normalize vectors on arrival so cosine becomes a dot product.
    pub normalize: bool,
    /// Instruction prefix, applied to QUERIES only (Qwen3 is asymmetric).
    pub query_instruction: String,
}

impl Default for EmbedderConfig {
    fn default() -> Self {
        Self {
            backend: EmbedderBackend::OpenAiCompat,
            base_url: "http://127.0.0.1:8080".into(),
            path: "/v1/embeddings".into(),
            model: "Qwen/Qwen3-Embedding-0.6B".into(),
            api_key: None,
            auth_header: "Authorization".into(),
            auth_prefix: "Bearer ".into(),
            extra_headers: BTreeMap::new(),
            dim: 1024,
            send_dimensions: false,
            verify_dim: true,
            batch_size: 32,
            concurrency: 2,
            timeout_secs: 120,
            max_retries: 3,
            max_input_chars: 24_000,
            normalize: true,
            query_instruction: crate::prompt::DEFAULT_QUERY_INSTRUCTION.into(),
        }
    }
}

impl EmbedderConfig {
    /// Full endpoint URL, with exactly one slash at the join.
    pub fn endpoint(&self) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            self.path.trim_start_matches('/')
        )
    }
}

/// Where vectors are written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StoreBackend {
    /// Vectors as BLOBs in the local SQLite catalog, searched from a warm
    /// in-memory matrix. Zero extra service; the default.
    #[default]
    Sqlite,
    /// Qdrant over its REST API. For large corpora or a shared index.
    Qdrant,
}

impl StoreBackend {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "sqlite" | "local" => Ok(Self::Sqlite),
            "qdrant" => Ok(Self::Qdrant),
            other => Err(Error::Config(format!("unknown store backend `{other}`"))),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::Qdrant => "qdrant",
        }
    }
}

crate::serde_via_parse!(StoreBackend, result);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StoreConfig {
    pub backend: StoreBackend,
    /// Directory holding `catalog.db` and its WAL.
    pub data_dir: String,
    /// Directory `POST /v1/export` writes into.
    pub export_dir: String,
    pub qdrant: QdrantConfig,
}

impl Default for StoreConfig {
    fn default() -> Self {
        let data_dir = default_data_dir();
        Self {
            backend: StoreBackend::Sqlite,
            export_dir: format!("{data_dir}/exports"),
            data_dir,
            qdrant: QdrantConfig::default(),
        }
    }
}

/// Where the catalog lives when nothing says otherwise.
///
/// INTENT: the default targets a daemon running natively on a developer
/// machine, because that is the setup with no bind mount, no path translation
/// and therefore no limit on which disk a project may live on. Compose
/// overrides it with `/data`.
pub fn default_data_dir() -> String {
    let home = |var: &str, suffix: &str| {
        std::env::var(var).ok().filter(|v| !v.is_empty()).map(|v| {
            let base = v.replace('\\', "/");
            format!("{}{suffix}", base.trim_end_matches('/'))
        })
    };
    home("LOCALAPPDATA", "/findery")
        .or_else(|| home("XDG_DATA_HOME", "/findery"))
        .or_else(|| home("HOME", "/.local/share/findery"))
        .unwrap_or_else(|| "./.findery".to_string())
}

/// Where the catalog lives for a process that has not loaded a configuration.
///
/// INTENT: clients need to *name* the data directory — in a diagnostic, in an
/// error pointing at the daemon's log — without opening it. Reading
/// `FINDERY_DATA_DIR` here is what keeps those messages pointing at the
/// directory the daemon will actually use; [`default_data_dir`] alone would
/// send the reader to a file that does not exist.
///
/// It is deliberately not the whole overlay: a configuration *file* can also
/// move the data directory, and only the daemon reads that. Clients that need
/// certainty ask the daemon.
pub fn data_dir_from_env() -> String {
    std::env::var("FINDERY_DATA_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .map(|v| v.replace('\\', "/").trim_end_matches('/').to_string())
        .unwrap_or_else(default_data_dir)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QdrantConfig {
    pub url: String,
    pub api_key: Option<String>,
    /// One collection per project: `<prefix><project_id>`.
    pub collection_prefix: String,
    pub timeout_secs: u64,
    /// Store vectors as int8 on the Qdrant side (4x smaller, ~1% recall loss).
    pub scalar_quantization: bool,
}

impl Default for QdrantConfig {
    fn default() -> Self {
        Self {
            // Loopback, like the embedder default: the documented setup runs
            // the daemon natively. Compose overrides this with `http://qdrant:6333`.
            url: "http://127.0.0.1:6333".into(),
            api_key: None,
            collection_prefix: "findery_".into(),
            timeout_secs: 30,
            scalar_quantization: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChunkConfig {
    /// Hard ceiling of a chunk, in characters. Roughly `chars / 3.6` tokens on
    /// Rust source.
    pub max_chars: usize,
    /// Below this, a chunk is merged with its neighbour instead of embedded
    /// alone: a three-line `impl Display` is noise on its own.
    pub min_chars: usize,
    /// Overlap when an oversized body has to be split into windows.
    pub overlap_chars: usize,
    /// Index `#[cfg(test)]` items as well.
    pub include_tests: bool,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            max_chars: 2_400,
            min_chars: 120,
            overlap_chars: 200,
            include_tests: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
    pub default_k: usize,
    pub max_k: usize,
    pub default_mode: crate::model::SearchMode,
    /// Reciprocal-rank-fusion constant. 60 is the value from the original
    /// Cormack et al. paper and holds up well here.
    pub rrf_k: f32,
    /// Candidates fetched per leg before fusion, as a multiple of `k`.
    pub candidate_multiplier: usize,
    /// Maximum characters of chunk text returned per hit; 0 means unlimited.
    /// This is the single most effective token-budget knob for MCP clients.
    pub max_snippet_chars: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            default_k: 10,
            max_k: 100,
            default_mode: crate::model::SearchMode::Hybrid,
            rrf_k: 60.0,
            candidate_multiplier: 4,
            max_snippet_chars: 2_400,
        }
    }
}

/// Defaults applied to a project that does not override them.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectDefaults {
    pub extensions: Vec<String>,
    pub exclude: Vec<String>,
    pub follow_gitignore: bool,
    /// Index dotfiles and dot-directories. On by default: `.github/`,
    /// `.cargo/` and `.claude/` carry real answers.
    pub include_hidden: bool,
    pub max_file_bytes: u64,
}

impl Default for ProjectDefaults {
    fn default() -> Self {
        Self {
            extensions: vec!["rs".into(), "md".into()],
            // Two families here. Build output and vendored trees are noise.
            // The rest are secret-bearing paths: hidden entries ARE indexed by
            // default, so the block that keeps credentials out of the index has
            // to live in the defaults rather than in the walker's blindness.
            exclude: vec![
                "**/target/**".into(),
                "**/node_modules/**".into(),
                "**/.git/**".into(),
                "**/vendor/**".into(),
                "**/*.lock".into(),
                "**/.env".into(),
                "**/.env.*".into(),
                "**/.ssh/**".into(),
                "**/.aws/**".into(),
                "**/.gnupg/**".into(),
                "**/.netrc".into(),
                "**/.npmrc".into(),
                "**/*.pem".into(),
                "**/*.p12".into(),
                "**/id_rsa*".into(),
                "**/id_ed25519*".into(),
            ],
            follow_gitignore: true,
            include_hidden: true,
            max_file_bytes: 2 * 1024 * 1024,
        }
    }
}
