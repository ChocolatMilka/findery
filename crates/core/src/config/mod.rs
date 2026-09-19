//! Daemon configuration: TOML file first, `FINDERY_*` environment variables on top.
//!
//! INTENT: this module parses and validates but never reads a file — the
//! binaries own the filesystem. `from_toml_str` + `apply_env` keeps L0 pure and
//! keeps the whole configuration surface testable without a tempdir.

mod sections;

pub use sections::{
    ChunkConfig, EmbedderBackend, EmbedderConfig, ProjectDefaults, QdrantConfig, SearchConfig,
    ServerConfig, StoreBackend, StoreConfig, default_data_dir,
};

use crate::path_map::{Mount, PathMapper};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Root configuration of the daemon.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub embedder: EmbedderConfig,
    pub store: StoreConfig,
    pub chunk: ChunkConfig,
    pub search: SearchConfig,
    pub project_defaults: ProjectDefaults,
    /// Bind mounts used to translate container paths back to host paths.
    #[serde(default)]
    pub mounts: Vec<Mount>,
}

impl Config {
    /// Parses a TOML document. An empty string yields the defaults, which are
    /// deliberately a working single-node setup.
    pub fn from_toml_str(src: &str) -> Result<Self> {
        toml::from_str(src).map_err(|e| Error::Config(format!("invalid TOML: {e}")))
    }

    /// Overlays `FINDERY_*` environment variables onto the parsed configuration.
    ///
    /// Docker Compose sets these; they must win over the baked-in file so one
    /// image can serve every deployment shape.
    pub fn apply_env(&mut self) -> Result<()> {
        self.apply_env_with(|k| std::env::var(k).ok())
    }

    /// Same as [`Config::apply_env`] with an injected lookup, for tests.
    pub fn apply_env_with(&mut self, get: impl Fn(&str) -> Option<String>) -> Result<()> {
        if let Some(v) = get("FINDERY_BIND") {
            self.server.bind = v;
        }
        if let Some(v) = get("FINDERY_API_TOKEN") {
            self.server.api_token = if v.is_empty() { None } else { Some(v) };
        }
        if let Some(v) = get("FINDERY_JOB_CONCURRENCY") {
            self.server.job_concurrency = parse_num("FINDERY_JOB_CONCURRENCY", &v)?;
        }

        if let Some(v) = get("FINDERY_EMBED_BACKEND") {
            self.embedder.backend = EmbedderBackend::parse(&v)?;
        }
        if let Some(v) = get("FINDERY_EMBED_BASE_URL") {
            self.embedder.base_url = v;
        }
        if let Some(v) = get("FINDERY_EMBED_MODEL") {
            self.embedder.model = v;
        }
        if let Some(v) = get("FINDERY_EMBED_API_KEY") {
            self.embedder.api_key = if v.is_empty() { None } else { Some(v) };
        }
        if let Some(v) = get("FINDERY_EMBED_DIM") {
            self.embedder.dim = parse_num("FINDERY_EMBED_DIM", &v)?;
        }
        if let Some(v) = get("FINDERY_EMBED_BATCH_SIZE") {
            self.embedder.batch_size = parse_num("FINDERY_EMBED_BATCH_SIZE", &v)?;
        }
        if let Some(v) = get("FINDERY_EMBED_CONCURRENCY") {
            self.embedder.concurrency = parse_num("FINDERY_EMBED_CONCURRENCY", &v)?;
        }
        if let Some(v) = get("FINDERY_EMBED_TIMEOUT_SECS") {
            self.embedder.timeout_secs = parse_num("FINDERY_EMBED_TIMEOUT_SECS", &v)?;
        }
        if let Some(v) = get("FINDERY_EMBED_QUERY_INSTRUCTION") {
            self.embedder.query_instruction = v;
        }
        if let Some(v) = get("FINDERY_EMBED_PATH") {
            self.embedder.path = v;
        }
        if let Some(v) = get("FINDERY_EMBED_AUTH_HEADER") {
            self.embedder.auth_header = v;
        }
        if let Some(v) = get("FINDERY_EMBED_AUTH_PREFIX") {
            self.embedder.auth_prefix = v;
        }
        if let Some(v) = get("FINDERY_EMBED_HEADERS") {
            self.embedder.extra_headers = parse_headers(&v)?;
        }
        if let Some(v) = get("FINDERY_EMBED_SEND_DIMENSIONS") {
            self.embedder.send_dimensions = parse_bool("FINDERY_EMBED_SEND_DIMENSIONS", &v)?;
        }
        if let Some(v) = get("FINDERY_EMBED_VERIFY_DIM") {
            self.embedder.verify_dim = parse_bool("FINDERY_EMBED_VERIFY_DIM", &v)?;
        }
        if let Some(v) = get("FINDERY_EMBED_MAX_RETRIES") {
            self.embedder.max_retries = parse_num("FINDERY_EMBED_MAX_RETRIES", &v)?;
        }
        if let Some(v) = get("FINDERY_EMBED_NORMALIZE") {
            self.embedder.normalize = parse_bool("FINDERY_EMBED_NORMALIZE", &v)?;
        }

        if let Some(v) = get("FINDERY_STORE_BACKEND") {
            self.store.backend = StoreBackend::parse(&v)?;
        }
        if let Some(v) = get("FINDERY_DATA_DIR") {
            // Keep exports next to the catalog unless they were pinned too,
            // otherwise moving the data directory strands them at the default.
            let moved_default = self.store.export_dir == format!("{}/exports", self.store.data_dir);
            self.store.data_dir = v;
            if moved_default {
                self.store.export_dir = format!("{}/exports", self.store.data_dir);
            }
        }
        if let Some(v) = get("FINDERY_EXPORT_DIR") {
            self.store.export_dir = v;
        }
        if let Some(v) = get("FINDERY_QDRANT_URL") {
            self.store.qdrant.url = v;
        }
        if let Some(v) = get("FINDERY_QDRANT_API_KEY") {
            self.store.qdrant.api_key = if v.is_empty() { None } else { Some(v) };
        }
        if let Some(v) = get("FINDERY_QDRANT_COLLECTION_PREFIX") {
            self.store.qdrant.collection_prefix = v;
        }

        if let Some(v) = get("FINDERY_CHUNK_MAX_CHARS") {
            self.chunk.max_chars = parse_num("FINDERY_CHUNK_MAX_CHARS", &v)?;
        }
        if let Some(v) = get("FINDERY_CHUNK_MIN_CHARS") {
            self.chunk.min_chars = parse_num("FINDERY_CHUNK_MIN_CHARS", &v)?;
        }
        if let Some(v) = get("FINDERY_CHUNK_OVERLAP_CHARS") {
            self.chunk.overlap_chars = parse_num("FINDERY_CHUNK_OVERLAP_CHARS", &v)?;
        }

        if let Some(v) = get("FINDERY_INCLUDE_HIDDEN") {
            self.project_defaults.include_hidden = parse_bool("FINDERY_INCLUDE_HIDDEN", &v)?;
        }
        if let Some(v) = get("FINDERY_EXTENSIONS") {
            self.project_defaults.extensions = v
                .split(',')
                .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
                .filter(|e| !e.is_empty())
                .collect();
        }

        if let Some(v) = get("FINDERY_MOUNTS") {
            self.mounts = parse_mounts(&v)?;
        }
        Ok(())
    }

    /// Rejects configurations that would fail later, at a point where the
    /// failure is expensive and confusing.
    pub fn validate(&self) -> Result<()> {
        if self.embedder.base_url.trim().is_empty() {
            return Err(Error::Config("embedder.base_url must not be empty".into()));
        }
        if self.embedder.batch_size == 0 {
            return Err(Error::Config("embedder.batch_size must be > 0".into()));
        }
        if self.embedder.concurrency == 0 {
            return Err(Error::Config("embedder.concurrency must be > 0".into()));
        }
        if self.chunk.max_chars < 200 {
            return Err(Error::Config("chunk.max_chars must be >= 200".into()));
        }
        if self.chunk.overlap_chars >= self.chunk.max_chars {
            return Err(Error::Config(
                "chunk.overlap_chars must be < chunk.max_chars".into(),
            ));
        }
        if self.search.max_k == 0 || self.search.default_k == 0 {
            return Err(Error::Config(
                "search.default_k and search.max_k must be > 0".into(),
            ));
        }
        if self.search.default_k > self.search.max_k {
            return Err(Error::Config(
                "search.default_k must be <= search.max_k".into(),
            ));
        }
        if matches!(self.store.backend, StoreBackend::Qdrant) && self.store.qdrant.url.is_empty() {
            return Err(Error::Config(
                "store.qdrant.url is required for the qdrant backend".into(),
            ));
        }
        Ok(())
    }

    /// Builds the host/container translator declared by `mounts`.
    pub fn path_mapper(&self) -> PathMapper {
        PathMapper::new(self.mounts.clone())
    }

    /// The cache-qualifying prefix: any change here invalidates stored vectors.
    pub fn embedding_recipe(&self) -> String {
        format!("{}:{}", self.embedder.model, self.embedder.dim)
    }
}

// ---------------------------------------------------------------------------

fn parse_num<T: std::str::FromStr>(key: &str, raw: &str) -> Result<T> {
    raw.trim()
        .parse::<T>()
        .map_err(|_| Error::Config(format!("{key}: `{raw}` is not a valid number")))
}

fn parse_bool(key: &str, raw: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => Err(Error::Config(format!("{key}: `{other}` is not a boolean"))),
    }
}

/// Parses `FINDERY_EMBED_HEADERS`, formatted as `Name: value[;Name: value]*`.
fn parse_headers(raw: &str) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for entry in raw.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let (name, value) = entry.split_once(':').ok_or_else(|| {
            Error::Config(format!(
                "FINDERY_EMBED_HEADERS: `{entry}` is not `Name: value`"
            ))
        })?;
        if name.trim().is_empty() {
            return Err(Error::Config(format!(
                "FINDERY_EMBED_HEADERS: `{entry}` has an empty name"
            )));
        }
        out.insert(name.trim().to_string(), value.trim().to_string());
    }
    Ok(out)
}

/// Parses `FINDERY_MOUNTS`, formatted as `host=container[;host=container]*`.
fn parse_mounts(raw: &str) -> Result<Vec<Mount>> {
    let mut out = Vec::new();
    for entry in raw.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let (host, container) = entry.split_once('=').ok_or_else(|| {
            Error::Config(format!("FINDERY_MOUNTS: `{entry}` is not `host=container`"))
        })?;
        if host.trim().is_empty() || container.trim().is_empty() {
            return Err(Error::Config(format!(
                "FINDERY_MOUNTS: `{entry}` has an empty side"
            )));
        }
        out.push(Mount {
            host: host.trim().into(),
            container: container.trim().into(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_a_valid_single_node_setup() {
        let cfg = Config::default();
        cfg.validate().expect("defaults must validate");
        assert_eq!(cfg.store.backend, StoreBackend::Sqlite);
        assert_eq!(cfg.embedder.dim, 1024);
        // The default targets a native daemon: a loopback model endpoint and a
        // catalog in the user's data directory, not a container path.
        assert!(
            cfg.embedder.base_url.contains("127.0.0.1"),
            "{}",
            cfg.embedder.base_url
        );
        assert!(
            !cfg.store.data_dir.starts_with("/data"),
            "{}",
            cfg.store.data_dir
        );
        assert!(cfg.store.export_dir.starts_with(&cfg.store.data_dir));
    }

    #[test]
    fn moving_the_data_dir_takes_the_default_export_dir_with_it() {
        let mut cfg = Config::default();
        cfg.apply_env_with(|k| (k == "FINDERY_DATA_DIR").then(|| "/srv/index".to_string()))
            .expect("env overlay");
        assert_eq!(cfg.store.data_dir, "/srv/index");
        assert_eq!(cfg.store.export_dir, "/srv/index/exports");
    }

    #[test]
    fn a_pinned_export_dir_survives_a_data_dir_move() {
        let mut cfg = Config::default();
        cfg.store.export_dir = "/mnt/backup".into();
        cfg.apply_env_with(|k| (k == "FINDERY_DATA_DIR").then(|| "/srv/index".to_string()))
            .expect("env overlay");
        assert_eq!(cfg.store.export_dir, "/mnt/backup");
    }

    #[test]
    fn env_overrides_the_file() {
        let mut cfg = Config::from_toml_str("[embedder]\ndim = 1024\n").expect("parse");
        cfg.apply_env_with(|k| match k {
            "FINDERY_EMBED_DIM" => Some("2560".into()),
            "FINDERY_STORE_BACKEND" => Some("qdrant".into()),
            "FINDERY_QDRANT_URL" => Some("http://q:6333".into()),
            _ => None,
        })
        .expect("env overlay");
        assert_eq!(cfg.embedder.dim, 2560);
        assert_eq!(cfg.store.backend, StoreBackend::Qdrant);
        cfg.validate().expect("still valid");
    }

    #[test]
    fn mounts_parse_and_translate() {
        let mounts = parse_mounts("C:/Users/me=/workspaces; D:/x=/workspaces/x").expect("parse");
        assert_eq!(mounts.len(), 2);
        assert!(parse_mounts("nope").is_err());
    }

    #[test]
    fn a_remote_provider_is_pure_configuration() {
        let mut cfg = Config::default();
        cfg.apply_env_with(|k| match k {
            "FINDERY_EMBED_BASE_URL" => Some("https://api.openai.com".into()),
            "FINDERY_EMBED_MODEL" => Some("text-embedding-3-large".into()),
            "FINDERY_EMBED_API_KEY" => Some("sk-test".into()),
            "FINDERY_EMBED_DIM" => Some("3072".into()),
            "FINDERY_EMBED_SEND_DIMENSIONS" => Some("true".into()),
            "FINDERY_EMBED_HEADERS" => Some("OpenAI-Organization: org-1".into()),
            _ => None,
        })
        .expect("env overlay");
        cfg.validate().expect("valid");
        assert_eq!(
            cfg.embedder.endpoint(),
            "https://api.openai.com/v1/embeddings"
        );
        assert_eq!(
            cfg.embedder
                .extra_headers
                .get("OpenAI-Organization")
                .map(String::as_str),
            Some("org-1")
        );
        assert_eq!(cfg.embedding_recipe(), "text-embedding-3-large:3072");
    }

    #[test]
    fn endpoint_join_never_doubles_the_slash() {
        let mut cfg = Config::default();
        cfg.embedder.base_url = "http://tei:80/".into();
        cfg.embedder.path = "/v1/embeddings".into();
        assert_eq!(cfg.embedder.endpoint(), "http://tei:80/v1/embeddings");
    }

    /// The reference file shipped in `config/` must actually load.
    ///
    /// It is the file users copy and mount, and it claims to spell out the
    /// defaults. A drift between it and the code is a doc that lies (CONV-037),
    /// and it went unnoticed until someone ran the binary against it.
    #[test]
    fn the_shipped_reference_config_parses_and_matches_the_defaults() {
        let src = include_str!("../../../../config/findery.toml");
        let cfg = Config::from_toml_str(src).expect("the shipped reference config must parse");
        cfg.validate().expect("and must validate");

        let defaults = Config::default();
        assert_eq!(cfg.embedder.backend, defaults.embedder.backend);
        assert_eq!(cfg.embedder.base_url, defaults.embedder.base_url);
        assert_eq!(cfg.embedder.model, defaults.embedder.model);
        assert_eq!(cfg.embedder.dim, defaults.embedder.dim);
        assert_eq!(cfg.store.backend, defaults.store.backend);
        assert_eq!(cfg.chunk.max_chars, defaults.chunk.max_chars);
        assert_eq!(cfg.search.default_k, defaults.search.default_k);
        assert_eq!(
            cfg.project_defaults.extensions,
            defaults.project_defaults.extensions
        );
        assert_eq!(
            cfg.project_defaults.include_hidden,
            defaults.project_defaults.include_hidden
        );
    }

    /// A backend name must mean the same thing in TOML, in JSON and in an
    /// environment variable.
    #[test]
    fn backend_names_spell_the_same_in_every_format() {
        for text in ["openai_compat", "openai", "tei", "ollama", "llamacpp"] {
            let cfg = Config::from_toml_str(&format!(
                "[embedder]
backend = \"{text}\"
"
            ))
            .unwrap_or_else(|e| panic!("TOML must accept `{text}`: {e}"));
            assert_eq!(
                cfg.embedder.backend,
                EmbedderBackend::OpenAiCompat,
                "{text}"
            );

            let mut env = Config::default();
            env.apply_env_with(|k| (k == "FINDERY_EMBED_BACKEND").then(|| text.to_string()))
                .unwrap_or_else(|e| panic!("env must accept `{text}`: {e}"));
            assert_eq!(
                env.embedder.backend,
                EmbedderBackend::OpenAiCompat,
                "{text}"
            );
        }
        assert!(
            Config::from_toml_str(
                "[embedder]
backend = \"gpt\"
"
            )
            .is_err()
        );
    }

    /// Serializing must emit the spelling `parse` accepts, or `--check` prints
    /// a file that cannot be read back.
    #[test]
    fn a_serialized_config_round_trips() {
        let cfg = Config::default();
        let text = toml::to_string_pretty(&cfg).expect("serialize");
        assert!(text.contains("openai_compat"), "{text}");
        let back = Config::from_toml_str(&text).expect("re-parse");
        assert_eq!(back.embedder.backend, cfg.embedder.backend);
        assert_eq!(back.store.backend, cfg.store.backend);
    }

    #[test]
    fn unknown_backend_is_refused_not_ignored() {
        assert!(EmbedderBackend::parse("gpt").is_err());
        assert!(StoreBackend::parse("pinecone").is_err());
    }

    #[test]
    fn inconsistent_chunking_is_refused() {
        let mut cfg = Config::default();
        cfg.chunk.overlap_chars = cfg.chunk.max_chars;
        assert!(cfg.validate().is_err());
    }
}
