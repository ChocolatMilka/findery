//! The `FINDERY_*` overlay: one table naming every variable and the setting it
//! overrides.
//!
//! INTENT: the overlay and the CLI's "this variable is shadowed by your file"
//! warning must agree on which variables exist. Held as one table rather than
//! two hand-kept lists, the warning cannot name a variable the overlay ignores,
//! nor miss one it honours — which is exactly how a setting comes to be
//! documented and inert.
//!
//! Entry/exit: [`Config::apply_env_with`] mutates a parsed configuration in
//! place and returns the first parse failure; nothing here touches the
//! filesystem, so the whole surface is testable with an injected lookup.

use super::sections::{EmbedderBackend, StoreBackend};
use crate::config::Config;
use crate::path_map::Mount;
use crate::{Error, Result};
use std::collections::BTreeMap;

/// One `FINDERY_*` variable, the dotted key it overrides, and how.
pub struct EnvOverride {
    /// The variable, as it appears in a shell.
    pub var: &'static str,
    /// The dotted configuration key it overrides (`server.bind`).
    pub key: &'static str,
    /// The overlay itself.
    apply: fn(&mut Config, EnvValue<'_>) -> Result<()>,
}

/// A variable's raw text, carrying the variable it came from so that a parse
/// failure can name the thing the user typed.
pub struct EnvValue<'a> {
    pub var: &'a str,
    pub raw: &'a str,
}

impl EnvValue<'_> {
    fn num<T: std::str::FromStr>(&self) -> Result<T> {
        self.raw
            .trim()
            .parse::<T>()
            .map_err(|_| Error::Config(format!("{}: `{}` is not a number", self.var, self.raw)))
    }

    fn flag(&self) -> Result<bool> {
        match self.raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => Err(Error::Config(format!(
                "{}: `{}` is not a boolean",
                self.var, self.raw
            ))),
        }
    }

    /// An absent secret is an empty string; the overlay clears the field.
    fn optional(&self) -> Option<String> {
        (!self.raw.is_empty()).then(|| self.raw.to_string())
    }

    /// `rust,md,ts` — dots and case are tolerated, empties dropped.
    fn extensions(&self) -> Vec<String> {
        self.raw
            .split(',')
            .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|e| !e.is_empty())
            .collect()
    }

    /// `Name: value[;Name: value]*`.
    fn headers(&self) -> Result<BTreeMap<String, String>> {
        let mut out = BTreeMap::new();
        for entry in self.raw.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            let (name, value) = entry.split_once(':').ok_or_else(|| {
                Error::Config(format!("{}: `{entry}` is not `Name: value`", self.var))
            })?;
            if name.trim().is_empty() {
                return Err(Error::Config(format!(
                    "{}: `{entry}` has an empty name",
                    self.var
                )));
            }
            out.insert(name.trim().to_string(), value.trim().to_string());
        }
        Ok(out)
    }

    /// `host=container[;host=container]*`.
    fn mounts(&self) -> Result<Vec<Mount>> {
        let mut out = Vec::new();
        for entry in self.raw.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            let (host, container) = entry.split_once('=').ok_or_else(|| {
                Error::Config(format!("{}: `{entry}` is not `host=container`", self.var))
            })?;
            if host.trim().is_empty() || container.trim().is_empty() {
                return Err(Error::Config(format!(
                    "{}: `{entry}` has an empty side",
                    self.var
                )));
            }
            out.push(Mount {
                host: host.trim().into(),
                container: container.trim().into(),
            });
        }
        Ok(out)
    }
}

/// Every environment variable that overrides the configuration file.
pub const ENV_OVERRIDES: &[EnvOverride] = &[
    EnvOverride {
        var: "FINDERY_BIND",
        key: "server.bind",
        apply: |c, v| {
            c.server.bind = v.raw.to_string();
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_SERVER_URL",
        key: "server.url",
        apply: |c, v| {
            c.server.url = v.raw.to_string();
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_API_TOKEN",
        key: "server.api_token",
        apply: |c, v| {
            c.server.api_token = v.optional();
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_JOB_CONCURRENCY",
        key: "server.job_concurrency",
        apply: |c, v| {
            c.server.job_concurrency = v.num()?;
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_EMBED_BACKEND",
        key: "embedder.backend",
        apply: |c, v| {
            c.embedder.backend = EmbedderBackend::parse(v.raw)
                .map_err(|e| Error::Config(format!("{}: {e}", v.var)))?;
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_EMBED_BASE_URL",
        key: "embedder.base_url",
        apply: |c, v| {
            c.embedder.base_url = v.raw.to_string();
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_EMBED_MODEL",
        key: "embedder.model",
        apply: |c, v| {
            c.embedder.model = v.raw.to_string();
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_EMBED_API_KEY",
        key: "embedder.api_key",
        apply: |c, v| {
            c.embedder.api_key = v.optional();
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_EMBED_DIM",
        key: "embedder.dim",
        apply: |c, v| {
            c.embedder.dim = v.num()?;
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_EMBED_BATCH_SIZE",
        key: "embedder.batch_size",
        apply: |c, v| {
            c.embedder.batch_size = v.num()?;
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_EMBED_CONCURRENCY",
        key: "embedder.concurrency",
        apply: |c, v| {
            c.embedder.concurrency = v.num()?;
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_EMBED_TIMEOUT_SECS",
        key: "embedder.timeout_secs",
        apply: |c, v| {
            c.embedder.timeout_secs = v.num()?;
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_EMBED_QUERY_INSTRUCTION",
        key: "embedder.query_instruction",
        apply: |c, v| {
            c.embedder.query_instruction = v.raw.to_string();
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_EMBED_PATH",
        key: "embedder.path",
        apply: |c, v| {
            c.embedder.path = v.raw.to_string();
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_EMBED_AUTH_HEADER",
        key: "embedder.auth_header",
        apply: |c, v| {
            c.embedder.auth_header = v.raw.to_string();
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_EMBED_AUTH_PREFIX",
        key: "embedder.auth_prefix",
        apply: |c, v| {
            c.embedder.auth_prefix = v.raw.to_string();
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_EMBED_HEADERS",
        key: "embedder.extra_headers",
        apply: |c, v| {
            c.embedder.extra_headers = v.headers()?;
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_EMBED_SEND_DIMENSIONS",
        key: "embedder.send_dimensions",
        apply: |c, v| {
            c.embedder.send_dimensions = v.flag()?;
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_EMBED_VERIFY_DIM",
        key: "embedder.verify_dim",
        apply: |c, v| {
            c.embedder.verify_dim = v.flag()?;
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_EMBED_MAX_RETRIES",
        key: "embedder.max_retries",
        apply: |c, v| {
            c.embedder.max_retries = v.num()?;
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_EMBED_NORMALIZE",
        key: "embedder.normalize",
        apply: |c, v| {
            c.embedder.normalize = v.flag()?;
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_STORE_BACKEND",
        key: "store.backend",
        apply: |c, v| {
            c.store.backend =
                StoreBackend::parse(v.raw).map_err(|e| Error::Config(format!("{}: {e}", v.var)))?;
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_DATA_DIR",
        key: "store.data_dir",
        apply: |c, v| {
            // Exports follow the bucket unless they were pinned too, or moving
            // the data directory strands them at the default.
            let followed_default = c.store.export_dir == format!("{}/exports", c.store.data_dir);
            c.store.data_dir = v.raw.to_string();
            if followed_default {
                c.store.export_dir = format!("{}/exports", c.store.data_dir);
            }
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_EXPORT_DIR",
        key: "store.export_dir",
        apply: |c, v| {
            c.store.export_dir = v.raw.to_string();
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_BUCKET",
        key: "store.default_bucket",
        apply: |c, v| {
            c.store.default_bucket = v.raw.trim().to_string();
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_QDRANT_URL",
        key: "store.qdrant.url",
        apply: |c, v| {
            c.store.qdrant.url = v.raw.to_string();
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_QDRANT_API_KEY",
        key: "store.qdrant.api_key",
        apply: |c, v| {
            c.store.qdrant.api_key = v.optional();
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_QDRANT_COLLECTION_PREFIX",
        key: "store.qdrant.collection_prefix",
        apply: |c, v| {
            c.store.qdrant.collection_prefix = v.raw.to_string();
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_CHUNK_MAX_CHARS",
        key: "chunk.max_chars",
        apply: |c, v| {
            c.chunk.max_chars = v.num()?;
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_CHUNK_MIN_CHARS",
        key: "chunk.min_chars",
        apply: |c, v| {
            c.chunk.min_chars = v.num()?;
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_CHUNK_OVERLAP_CHARS",
        key: "chunk.overlap_chars",
        apply: |c, v| {
            c.chunk.overlap_chars = v.num()?;
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_SEARCH_DEFAULT_K",
        key: "search.default_k",
        apply: |c, v| {
            c.search.default_k = v.num()?;
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_SEARCH_MAX_K",
        key: "search.max_k",
        apply: |c, v| {
            c.search.max_k = v.num()?;
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_INCLUDE_HIDDEN",
        key: "project_defaults.include_hidden",
        apply: |c, v| {
            c.project_defaults.include_hidden = v.flag()?;
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_EXTENSIONS",
        key: "project_defaults.extensions",
        apply: |c, v| {
            c.project_defaults.extensions = v.extensions();
            Ok(())
        },
    },
    EnvOverride {
        var: "FINDERY_MOUNTS",
        key: "mounts",
        apply: |c, v| {
            c.mounts = v.mounts()?;
            Ok(())
        },
    },
];

/// The variable that overrides `key`, if one does.
pub fn env_var_for(key: &str) -> Option<&'static str> {
    ENV_OVERRIDES.iter().find(|e| e.key == key).map(|e| e.var)
}

impl Config {
    /// Overlays `FINDERY_*` environment variables onto the parsed configuration.
    ///
    /// Docker Compose sets these; they must win over the baked-in file so one
    /// image can serve every deployment shape.
    pub fn apply_env(&mut self) -> Result<()> {
        self.apply_env_with(|k| std::env::var(k).ok())
    }

    /// Same as [`Config::apply_env`] with an injected lookup, for tests.
    pub fn apply_env_with(&mut self, get: impl Fn(&str) -> Option<String>) -> Result<()> {
        for entry in ENV_OVERRIDES {
            if let Some(raw) = get(entry.var) {
                (entry.apply)(
                    self,
                    EnvValue {
                        var: entry.var,
                        raw: &raw,
                    },
                )?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row of the table must reach a field, and every key must be unique —
    /// a duplicated key is a variable that silently overrides nothing.
    #[test]
    fn the_table_names_each_key_once_and_can_be_looked_up_both_ways() {
        let mut seen = std::collections::BTreeSet::new();
        for entry in ENV_OVERRIDES {
            assert!(seen.insert(entry.key), "`{}` is claimed twice", entry.key);
            assert_eq!(env_var_for(entry.key), Some(entry.var));
            assert!(entry.var.starts_with("FINDERY_"), "{}", entry.var);
        }
        assert_eq!(env_var_for("nope.not.a.key"), None);
    }

    /// The overlay is table-driven, so a row that cannot be applied is a row
    /// nobody can use: apply every variable with a placeholder and require the
    /// result to be either applied or a named parse failure — never a panic.
    #[test]
    fn every_row_is_applied_or_refused_by_name() {
        for entry in ENV_OVERRIDES {
            let mut cfg = Config::default();
            let result = cfg.apply_env_with(|k| (k == entry.var).then(|| "1".to_string()));
            if let Err(Error::Config(message)) = result {
                assert!(message.starts_with(entry.var), "{message}");
            }
        }
    }

    #[test]
    fn the_default_bucket_is_overridable_without_touching_the_data_dir() {
        let mut cfg = Config::default();
        cfg.apply_env_with(|k| (k == "FINDERY_BUCKET").then(|| " work ".to_string()))
            .expect("env overlay");
        assert_eq!(cfg.store.default_bucket, "work");
        assert_eq!(cfg.store.data_dir, Config::default().store.data_dir);
    }
}
