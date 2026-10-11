//! Where each bucket's file is, for buckets whose name does not say.
//!
//! INTENT: a bucket used to be a name, and a name was a directory under the data
//! directory — discoverable by listing it. It can now be a file anywhere: inside
//! the project it indexes, on a shared drive, on a volume that moves. Nothing
//! about such a path can be derived from a name, and the daemon has to open
//! every bucket at boot to know which projects it serves, so the location has to
//! be written down somewhere.
//!
//! **This is a discovery index, not a second source of truth.** It records where
//! a bucket's *file* is — a fact about the filesystem that has no other home —
//! and never which project lives in which bucket, which is and stays "which file
//! holds the row". It is also self-healing in both directions:
//!
//! * named and default buckets are still found by scanning the data directory,
//!   so losing this file costs only the out-of-place ones;
//! * an entry whose file has gone is pruned at load and reported, never fatal,
//!   because a bucket that was deleted between two boots is an ordinary event.

use findery_core::config::StoreConfig;
use findery_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The buckets that do not live where their name says.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Registry {
    /// Bucket name → the database file that holds it.
    pub buckets: BTreeMap<String, String>,
}

impl Registry {
    /// Reads the registry, dropping entries whose file has gone.
    ///
    /// A missing or unreadable file is not an error: it is the ordinary state of
    /// an install that has never used an out-of-place bucket, and a corrupt one
    /// costs those locations and nothing else.
    pub fn load(cfg: &StoreConfig) -> (Self, Vec<String>) {
        let path = registry_path(cfg);
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return (Self::default(), Vec::new());
            }
            Err(e) => {
                return (
                    Self::default(),
                    vec![format!("cannot read {}: {e}", path.display())],
                );
            }
        };
        let mut registry: Self = match toml::from_str(&source) {
            Ok(registry) => registry,
            Err(e) => {
                return (
                    Self::default(),
                    vec![format!(
                        "{} is not a valid registry ({e}); out-of-place buckets will not be \
                         found until it is rewritten",
                        path.display()
                    )],
                );
            }
        };
        let mut warnings = Vec::new();
        registry
            .buckets
            .retain(|name, file| match Path::new(file).is_file() {
                true => true,
                false => {
                    warnings.push(format!(
                        "bucket `{name}` was recorded at {file}, which is no longer there"
                    ));
                    false
                }
            });
        (registry, warnings)
    }

    /// Records where a bucket's file is, replacing what was there.
    ///
    /// The daemon is the only writer, so a read-modify-write cannot race: there
    /// is one process, and it does this between two of its own operations.
    pub fn record(cfg: &StoreConfig, name: &str, file: &str) -> Result<()> {
        let (mut registry, _) = Self::load(cfg);
        registry.buckets.insert(name.to_string(), file.to_string());
        registry.save(cfg)
    }

    /// Forgets a bucket. Removing a name that was never recorded is not an
    /// error — the caller is describing an end state, not a history.
    pub fn forget(cfg: &StoreConfig, name: &str) -> Result<()> {
        let (mut registry, _) = Self::load(cfg);
        if registry.buckets.remove(name).is_none() {
            return Ok(());
        }
        registry.save(cfg)
    }

    /// The path recorded for a bucket, when it has one.
    pub fn file_of(&self, name: &str) -> Option<&str> {
        self.buckets.get(name).map(String::as_str)
    }

    fn save(&self, cfg: &StoreConfig) -> Result<()> {
        let path = registry_path(cfg);
        if self.buckets.is_empty() {
            // Leaving an empty file behind would suggest a registry where there
            // is none, and the next `load` would read it back as one.
            return match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(Error::Store(format!(
                    "cannot remove {}: {e}",
                    path.display()
                ))),
            };
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Store(format!("cannot create {}: {e}", parent.display())))?;
        }
        let rendered = toml::to_string_pretty(self)
            .map_err(|e| Error::Store(format!("cannot render the registry: {e}")))?;
        std::fs::write(&path, rendered)
            .map_err(|e| Error::Store(format!("cannot write {}: {e}", path.display())))
    }
}

/// The file the registry lives in, beside the data directory's own settings.
pub fn registry_path(cfg: &StoreConfig) -> PathBuf {
    PathBuf::from(cfg.data_dir.trim_end_matches(['/', '\\'])).join("buckets.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_in(dir: &Path) -> StoreConfig {
        StoreConfig {
            data_dir: dir.display().to_string(),
            ..StoreConfig::default()
        }
    }

    /// A recorded location comes back, and two boots agree about it.
    #[test]
    fn a_location_survives_being_written_and_read_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_in(dir.path());
        let file = dir.path().join("elsewhere.db");
        std::fs::write(&file, b"not really a database").expect("touch");

        Registry::record(&cfg, "team", &file.display().to_string()).expect("record");
        let (registry, warnings) = Registry::load(&cfg);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            registry.file_of("team"),
            Some(file.display().to_string().as_str())
        );

        Registry::forget(&cfg, "team").expect("forget");
        let (registry, _) = Registry::load(&cfg);
        assert_eq!(registry.file_of("team"), None);
    }

    /// A bucket deleted between two boots is an ordinary event, and it must not
    /// stop the daemon from starting or hide the buckets that are still there.
    #[test]
    fn an_entry_whose_file_is_gone_is_dropped_and_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_in(dir.path());
        let alive = dir.path().join("alive.db");
        std::fs::write(&alive, b"x").expect("touch");

        Registry::record(&cfg, "alive", &alive.display().to_string()).expect("record");
        Registry::record(
            &cfg,
            "gone",
            &dir.path().join("gone.db").display().to_string(),
        )
        .expect("record");

        let (registry, warnings) = Registry::load(&cfg);
        assert!(registry.file_of("alive").is_some());
        assert_eq!(registry.file_of("gone"), None);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("gone"), "{:?}", warnings);
    }

    /// Forgetting a bucket that was never recorded is a description of an end
    /// state, not an error to report.
    #[test]
    fn forgetting_an_unknown_bucket_is_not_a_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_in(dir.path());
        Registry::forget(&cfg, "never").expect("no-op");
        assert!(!registry_path(&cfg).exists(), "and it left no file behind");
    }

    /// A registry nobody can parse costs the locations it held, not the boot —
    /// and says so, because a silently ignored file is a setting that does
    /// nothing.
    #[test]
    fn an_unreadable_registry_costs_the_locations_and_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_in(dir.path());
        std::fs::write(registry_path(&cfg), b"this is not toml {{{").expect("write");

        let (registry, warnings) = Registry::load(&cfg);
        assert!(registry.buckets.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("not a valid registry"), "{warnings:?}");
    }
}
