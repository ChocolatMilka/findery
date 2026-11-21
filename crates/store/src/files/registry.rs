//! Where each bucket's file is, for buckets whose name does not say.
//!
//! A bucket can now live inside the project it indexes, or on a shared drive, so
//! its location cannot be derived from its name — and the daemon has to open
//! every bucket at boot. This is a *discovery index*, not a second source of
//! truth: it says where a bucket's file is, never which project lives in which
//! bucket. Named buckets are still found by scanning, so losing this file costs
//! only the out-of-place ones, and an entry whose file is gone is pruned.

use findery_core::config::StoreConfig;
use findery_core::{Error, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Serialises a read-modify-write of the registry file.
///
/// Registrations are concurrent handlers that `await` in the middle of this, so
/// two can interleave, both read the old file, and the second write drops the
/// first entry — a bucket not reopened at boot. Process-wide because the file is.
fn guard() -> &'static Mutex<()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(Mutex::default)
}

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
    /// A missing or unreadable file costs those locations and nothing else: not
    /// an error, since never having used an out-of-place bucket is ordinary.
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
    pub fn record(cfg: &StoreConfig, name: &str, file: &str) -> Result<()> {
        let _serialised = guard().lock();
        let (mut registry, _) = Self::load(cfg);
        registry.buckets.insert(name.to_string(), file.to_string());
        registry.save(cfg)
    }

    /// Forgets a bucket. A name that was never recorded is not an error: the
    /// caller describes an end state, not a history.
    pub fn forget(cfg: &StoreConfig, name: &str) -> Result<()> {
        let _serialised = guard().lock();
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
            // An empty file would suggest a registry where there is none.
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
        // Written beside and renamed over: an interrupted write would otherwise
        // leave a half-file that `load` reads as corrupt, costing every location.
        let staging = path.with_extension("toml.new");
        std::fs::write(&staging, rendered)
            .map_err(|e| Error::Store(format!("cannot write {}: {e}", staging.display())))?;
        std::fs::rename(&staging, &path)
            .map_err(|e| Error::Store(format!("cannot replace {}: {e}", path.display())))
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

    /// Registrations arrive from concurrent HTTP handlers, and the file is a
    /// read-modify-write of the whole thing. Without a guard, two that overlap
    /// both read the old contents and the second write drops the first entry —
    /// which is a bucket that is not reopened at boot, and a project the user can
    /// no longer search.
    #[test]
    fn concurrent_records_do_not_lose_each_other() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_in(dir.path());
        let names = ["a", "b", "c", "d", "e", "f", "g", "h"];
        for name in names {
            std::fs::write(dir.path().join(format!("{name}.db")), b"x").expect("touch");
        }

        std::thread::scope(|scope| {
            for name in names {
                let file = dir.path().join(format!("{name}.db")).display().to_string();
                let cfg = &cfg;
                scope.spawn(move || Registry::record(cfg, name, &file).expect("record"));
            }
        });

        let (registry, warnings) = Registry::load(&cfg);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            registry.buckets.len(),
            names.len(),
            "every record survived: {:?}",
            registry.buckets
        );
    }

    /// A bucket deleted between two boots must not stop the boot, nor hide the
    /// buckets that are still there.
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
