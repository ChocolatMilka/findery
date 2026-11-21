//! The configuration sections: one struct per concern, each with the defaults
//! that make it work out of the box.
//!
//! INTENT: split out of `config/mod.rs`, which owns parsing, the environment
//! overlay and validation. Those are one responsibility; describing every knob
//! of every subsystem is another (CONV-029).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::default_data_dir;
use crate::{Error, Result};

/// Where a client reaches the daemon when nothing tells it otherwise.
///
/// INTENT: the address a client dials and the address the daemon binds are two
/// halves of one deployment, so they live in one file and neither needs an
/// environment variable to be named. Loopback, because the documented setup runs
/// the daemon natively; a remote daemon is a `server.url` away.
pub const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:8077";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// `host:port` the HTTP API binds to.
    pub bind: String,
    /// Address a client dials to reach this daemon.
    ///
    /// The daemon itself ignores it: it is the setting that lets `findery` on
    /// this machine — and the MCP bridge configured alongside it — find the
    /// daemon without `FINDERY_SERVER_URL` in every shell.
    pub url: String,
    /// When set, every request must carry `Authorization: Bearer <token>`.
    ///
    /// A client reads the same value as its bearer token: one file names the
    /// daemon and the secret it asks for.
    pub api_token: Option<String>,
    /// Maximum accepted request body.
    pub max_body_bytes: usize,
    /// How many sync jobs may run at the same time.
    pub job_concurrency: usize,
    /// How many finished jobs stay queryable.
    pub job_history: usize,
    /// How long `detach` and `rm` wait for a project's running sync to stop
    /// before they give up with `busy`, changing nothing.
    pub drain_timeout_secs: u64,
    /// How long a drain still waits once it aborted a job that ignored its
    /// cancel flag. An aborted task lets go at its next `await`; one that does
    /// not within this is stuck, and waiting longer would only hide that.
    pub abort_grace_secs: u64,
    /// Longest a sync request may ask to wait for its job (`wait_ms`) before
    /// the job is answered as it stands.
    pub max_sync_wait_ms: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8077".into(),
            url: DEFAULT_SERVER_URL.into(),
            api_token: None,
            max_body_bytes: 32 * 1024 * 1024,
            job_concurrency: 2,
            job_history: 64,
            drain_timeout_secs: 15,
            abort_grace_secs: 2,
            max_sync_wait_ms: 120_000,
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
/// `qwen3.7-text-embedding` on DashScope; pointing `base_url` at a local
/// LM Studio, at api.openai.com or at a TEI behind TLS is a configuration
/// change, not a code change.
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
    /// Canonical name of the weights behind `model`, when the runtime's own
    /// name is not one. It replaces `model` as the recipe's identity, so LM
    /// Studio's `harrier-oss-v1-0.6b` and TEI's `microsoft/harrier-oss-v1-0.6b`
    /// — the same weights under two names — share their vectors. A different
    /// quantization (a GGUF `Q8_0`) is different weights, and must not claim
    /// the same name.
    pub weights: Option<String>,
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
    /// (qwen3.7-text-embedding, OpenAI text-embedding-3-*) honour it and return a
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
    /// Ceiling of one liveness probe — `findery health`, `doctor`, `model ls`,
    /// and the boot probe when the width is already known. A probe asks once:
    /// the retries and `timeout_secs` are for embedding work, and a runtime that
    /// hangs must not hold a diagnostic for minutes.
    pub probe_timeout_secs: u64,
    /// Inputs longer than this are truncated before being sent.
    pub max_input_chars: usize,
    /// L2-normalize vectors on arrival so cosine becomes a dot product.
    pub normalize: bool,
    /// Instruction prefix, applied to QUERIES only (the model is asymmetric).
    pub query_instruction: String,
    /// How this model wants a query; `None` is
    /// [`crate::embedding::prompt::DEFAULT_QUERY_TEMPLATE`]. Must contain `{query}`.
    pub query_template: Option<String>,
    /// How this model wants a document; `None` is the identity `{text}`. Must
    /// contain `{text}`. A non-identity template changes every stored vector,
    /// so it enters the recipe and opens its own vector space.
    pub document_template: Option<String>,
}

impl Default for EmbedderConfig {
    fn default() -> Self {
        Self {
            backend: EmbedderBackend::OpenAiCompat,
            base_url: "https://dashscope-intl.aliyuncs.com/compatible-mode".into(),
            path: "/v1/embeddings".into(),
            model: "qwen3.7-text-embedding".into(),
            weights: None,
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
            probe_timeout_secs: 10,
            max_input_chars: 24_000,
            normalize: true,
            query_instruction: crate::embedding::prompt::DEFAULT_QUERY_INSTRUCTION.into(),
            query_template: None,
            document_template: None,
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
    /// Vectors as BLOBs in the local SQLite bucket, searched from a warm
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
    /// Directory holding the buckets and their WAL.
    pub data_dir: String,
    /// Directory `POST /v1/export` writes into.
    pub export_dir: String,
    /// Bucket a project joins when `init` names none.
    ///
    /// INTENT: the default is the bucket every install already has, so a
    /// single-bucket user never learns the concept exists.
    pub default_bucket: String,
    /// How long a peek at another file's recipe or project ids — at boot, and
    /// during adoption — waits for a writer to let go, in milliseconds. Short
    /// on purpose: it runs once per file while the daemon starts.
    pub peek_busy_timeout_ms: u64,
    pub qdrant: QdrantConfig,
}

impl Default for StoreConfig {
    fn default() -> Self {
        let data_dir = default_data_dir();
        Self {
            backend: StoreBackend::Sqlite,
            export_dir: format!("{data_dir}/exports"),
            data_dir,
            default_bucket: crate::model::DEFAULT_BUCKET.to_string(),
            peek_busy_timeout_ms: 2_000,
            qdrant: QdrantConfig::default(),
        }
    }
}

impl StoreConfig {
    /// [`StoreConfig::peek_busy_timeout_ms`] as a duration.
    pub fn peek_busy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.peek_busy_timeout_ms)
    }
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
    /// Hard ceiling of a chunk, in **bytes** — the name is kept for the
    /// configuration files already written, but the measure is `str::len`, so a
    /// French or CJK comment reaches it sooner than its character count
    /// suggests. Roughly `bytes / 3.6` tokens on ASCII source.
    pub max_chars: usize,
    /// Below this, an anonymous chunk is merged with its anonymous neighbour
    /// instead of embedded alone: a run of `use` lines is noise as separate
    /// fragments. A chunk that carries a symbol is never merged away — its
    /// `kind` and its slot in the keyword index are exactly what a search for
    /// an exact identifier relies on. In bytes, like `max_chars`.
    pub min_chars: usize,
    /// Overlap when an oversized body has to be split into windows. In bytes.
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

/// What a project walks when it says nothing: one extension per language the
/// chunker registry knows. A file whose extension is absent is never walked, so
/// a language supported but not listed here is a language nobody sees.
pub const DEFAULT_EXTENSIONS: &[&str] = &[
    "rs", "md", "toml", "js", "mjs", "cjs", "ts", "py", "php", "json", "axle", "java", "kt", "kts",
    "go",
];

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
            extensions: DEFAULT_EXTENSIONS
                .iter()
                .map(|e| (*e).to_string())
                .collect(),
            // Two families here. Build output and vendored trees are noise.
            // The rest are secret-bearing paths: hidden entries ARE indexed by
            // default, so the block that keeps credentials out of the index has
            // to live in the defaults rather than in the walker's blindness.
            exclude: vec![
                "**/target/**".into(),
                "**/node_modules/**".into(),
                // A generated lockfile named `.json` slips through `*.lock`.
                "**/package-lock.json".into(),
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
