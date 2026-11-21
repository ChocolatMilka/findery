//! Where each bucket's files are, for buckets whose name does not say — and
//! the two kinds of unfinished business that must survive a restart.
//!
//! A bucket can now live inside the project it indexes, or on a shared drive, so
//! its location cannot be derived from its name — and the daemon has to open
//! every bucket at boot. This is a *discovery index*, not a second source of
//! truth: it says where a bucket's files are, never which project lives in
//! which bucket. Named buckets are still found by scanning, so losing this file
//! costs only the out-of-place ones, and an entry whose directory provably holds
//! none of its files (legacy base or tagged member) is pruned.
//!
//! Two more records live beside the locations, because nothing on disk could
//! say them:
//!
//! * `detached` — projects taken off the daemon whose files stay on disk.
//!   `removing = true` marks a `findery rm` that started and has to be
//!   finished before anything opens.
//! * `removing_families` / `removing_members` — buckets, and single member
//!   files, whose removal started. The tombstone is written *before* the first
//!   destructive step and cleared after the last, so a removal cut short by a
//!   crash or a held file is resumed, never half-forgotten.
//!
//! None of these is pruned: a tombstone whose files are gone is a removal that
//! finished its deletes and still has to say so. Locations are pruned only by
//! the boot's lenient [`Registry::load`], and only on proof; every
//! read-modify-write goes through the strict [`Registry::read`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use findery_core::config::StoreConfig;
use findery_core::ids::ProjectId;
use findery_core::model::is_bucket_name;
use findery_core::{Error, Result};

use crate::files::layout::{FamilyPath, family_absent};

/// Serialises a read-modify-write of the registry file.
///
/// Registrations are concurrent handlers that `await` in the middle of this, so
/// two can interleave, both read the old file, and the second write drops the
/// first entry — a bucket not reopened at boot. Process-wide because the file is.
fn guard() -> &'static Mutex<()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(Mutex::default)
}

/// The buckets that do not live where their name says, the detached
/// projects, and the removals in flight.
///
/// INTENT: every field defaults, so a `buckets.toml` written before a field
/// existed still reads — it simply holds none of those records yet.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Registry {
    /// Buckets whose removal started and has not finished, by name.
    // Declared first: TOML wants plain values before tables.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub removing_families: BTreeSet<String>,
    /// Member files (paths) whose removal — `findery bucket rm --model` —
    /// started and has not finished.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub removing_members: BTreeSet<String>,
    /// Bucket name → the base path of its family (`dir/stem.db`).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub buckets: BTreeMap<String, String>,
    /// Project id → where its files stay while it is detached.
    ///
    /// Keyed by the id as text, since that is what a TOML key is; every key is
    /// checked with [`ProjectId::new`] on load, and the typed accessors are the
    /// way in.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub detached: BTreeMap<String, DetachedEntry>,
}

/// Where a detached project's index stays.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetachedEntry {
    /// The bucket it lived in.
    pub family: String,
    /// That bucket's base path.
    pub base: String,
    /// True once `findery rm` started on it: the removal is finished at the
    /// next boot instead of the project being left detached.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removing: bool,
}

impl Registry {
    /// Reads the registry at boot, dropping location entries whose family is
    /// provably gone, and records that are not well-formed.
    ///
    /// Lenient: a file that cannot be read costs its records for this run and
    /// says so — never the boot. Nothing here writes the file back, so a
    /// registry that is only unreadable for a moment loses nothing.
    pub fn load(cfg: &StoreConfig) -> (Self, Vec<String>) {
        match Self::read_checked(cfg) {
            Ok((mut registry, mut warnings)) => {
                registry.prune_gone(&mut warnings);
                (registry, warnings)
            }
            Err(e) => (
                Self::default(),
                vec![format!(
                    "{}; out-of-place buckets will not be found until it is fixed",
                    e.detail()
                )],
            ),
        }
    }

    /// Reads the registry as it is on disk, pruning nothing.
    ///
    /// Strict: a file that exists and cannot be read or parsed is an error.
    /// Every read that may lead to a write goes through here, so a transient
    /// sharing violation or a bad hand edit is refused rather than read as
    /// "nothing recorded" and written back empty.
    pub fn read(cfg: &StoreConfig) -> Result<Self> {
        Self::read_checked(cfg).map(|(registry, _)| registry)
    }

    fn read_checked(cfg: &StoreConfig) -> Result<(Self, Vec<String>)> {
        let path = registry_path(cfg);
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Self::default(), Vec::new()));
            }
            Err(e) => {
                return Err(Error::Store(format!("cannot read {}: {e}", path.display())));
            }
        };
        let registry: Self = toml::from_str(&source).map_err(|e| {
            Error::Store(format!("{} is not a valid registry ({e})", path.display()))
        })?;
        Ok(registry.well_formed(&path))
    }

    /// Drops the records whose key is not what it claims to be.
    fn well_formed(mut self, path: &Path) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        self.detached.retain(|id, _| {
            let valid = ProjectId::new(id).is_ok();
            if !valid {
                warnings.push(format!(
                    "{}: `{id}` is not a project id; its detached entry is ignored",
                    path.display()
                ));
            }
            valid
        });
        self.removing_families.retain(|name| {
            let valid = is_bucket_name(name);
            if !valid {
                warnings.push(format!(
                    "{}: `{name}` is not a bucket name; its removal tombstone is ignored",
                    path.display()
                ));
            }
            valid
        });
        (self, warnings)
    }

    /// Forgets the locations whose family is provably gone.
    ///
    /// On the family, not on the base file: adoption renames `bucket.db` to its
    /// tagged member, and an entry pruned for that would forget where a
    /// project-local bucket lives. And only on proof: a directory that is not
    /// there may be a drive not mounted yet.
    fn prune_gone(&mut self, warnings: &mut Vec<String>) {
        self.buckets.retain(|name, file| {
            let gone = FamilyPath::from_base(Path::new(file.as_str()))
                .is_ok_and(|family| family_absent(&family));
            if gone {
                warnings.push(format!(
                    "bucket `{name}` was recorded at {file}, which is no longer there"
                ));
            }
            !gone
        });
    }

    /// Reads, changes and writes the registry as one step.
    ///
    /// The one read-modify-write path: every writer below goes through it, so
    /// two concurrent changes serialize instead of the second dropping the
    /// first. The file is replaced atomically (written beside, renamed over).
    pub fn update<T>(cfg: &StoreConfig, change: impl FnOnce(&mut Self) -> T) -> Result<T> {
        let _serialised = guard().lock();
        let mut registry = Self::read(cfg)?;
        let out = change(&mut registry);
        registry.save(cfg)?;
        Ok(out)
    }

    /// Records where a bucket's family is, replacing what was there.
    pub fn record(cfg: &StoreConfig, name: &str, file: &str) -> Result<()> {
        Self::update(cfg, |r| {
            r.buckets.insert(name.to_string(), file.to_string());
        })
    }

    /// Forgets a bucket. A name that was never recorded is not an error: the
    /// caller describes an end state, not a history.
    pub fn forget(cfg: &StoreConfig, name: &str) -> Result<()> {
        Self::update(cfg, |r| {
            r.buckets.remove(name);
        })
    }

    /// The path recorded for a bucket, when it has one.
    pub fn file_of(&self, name: &str) -> Option<&str> {
        self.buckets.get(name).map(String::as_str)
    }

    /// Records a project as detached, replacing any earlier entry for it.
    pub fn detach(cfg: &StoreConfig, id: &ProjectId, entry: DetachedEntry) -> Result<()> {
        Self::update(cfg, |r| {
            r.detached.insert(id.to_string(), entry);
        })
    }

    /// Forgets a detached project, returning what was recorded for it.
    pub fn reattach(cfg: &StoreConfig, id: &ProjectId) -> Result<Option<DetachedEntry>> {
        Self::update(cfg, |r| r.detached.remove(id.as_str()))
    }

    /// Where a detached project's files stay, when it is detached.
    pub fn detached_entry(&self, id: &ProjectId) -> Option<&DetachedEntry> {
        self.detached.get(id.as_str())
    }

    /// Writes the tombstone of a bucket removal. Called before the first
    /// destructive step.
    pub fn mark_removing(cfg: &StoreConfig, family: &str) -> Result<()> {
        Self::update(cfg, |r| {
            r.removing_families.insert(family.to_string());
        })
    }

    /// Clears the tombstone of a bucket removal. Called after the last step.
    pub fn clear_removing(cfg: &StoreConfig, family: &str) -> Result<()> {
        Self::update(cfg, |r| {
            r.removing_families.remove(family);
        })
    }

    /// True while a removal of `family` is unfinished.
    pub fn is_removing(&self, family: &str) -> bool {
        self.removing_families.contains(family)
    }

    /// True when the registry records nothing at all.
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
            && self.detached.is_empty()
            && self.removing_families.is_empty()
            && self.removing_members.is_empty()
    }

    fn save(&self, cfg: &StoreConfig) -> Result<()> {
        let path = registry_path(cfg);
        if self.is_empty() {
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
        // leave a half-file that `load` reads as corrupt, costing every record —
        // tombstones included, which is a removal nobody finishes.
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
    use findery_core::embedding::recipe::Recipe;

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

    /// Adoption renames a project-local `bucket.db` to its tagged member. The
    /// entry names the family, so it must survive that — pruning it would
    /// forget the one bucket nothing else can find.
    #[tokio::test]
    async fn an_entry_survives_its_base_file_becoming_a_member() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_in(dir.path());
        let base = dir.path().join("project/.findery/bucket.db");
        let bucket = crate::Bucket::open_file(&base, "proj").expect("create");
        bucket.claim_recipe("mock:8").await.expect("claim");
        bucket.close().await.expect("close");
        Registry::record(&cfg, "proj", &base.display().to_string()).expect("record");

        let family = FamilyPath::from_base(&base).expect("family");
        let outcome =
            crate::adopt_legacy(&family, std::time::Duration::from_secs(2)).expect("adopt");
        assert!(
            matches!(outcome, crate::AdoptOutcome::Adopted { .. }),
            "{outcome:?}"
        );
        assert!(!base.exists());
        assert!(
            family
                .member(&Recipe::parse("mock:8").expect("r"))
                .is_file()
        );

        let (registry, warnings) = Registry::load(&cfg);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            registry.file_of("proj"),
            Some(base.display().to_string().as_str()),
            "the entry still names the base path"
        );
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

    /// The bug this locks: an unreadable registry read as empty, then written
    /// back by the next lifecycle step — every tombstone and every detached
    /// entry gone. A write over a file that cannot be read is refused.
    #[test]
    fn a_write_over_an_unreadable_registry_is_refused_and_leaves_its_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_in(dir.path());
        let garbage = b"removing_families = [\"work\"\n[detached.axle\nfamily =".to_vec();
        std::fs::write(registry_path(&cfg), &garbage).expect("write");

        let entry = DetachedEntry {
            family: "other".into(),
            base: "x.db".into(),
            removing: false,
        };
        let err = Registry::detach(&cfg, &ProjectId::new("other").expect("id"), entry)
            .expect_err("refused");
        assert_eq!(err.code(), "store");
        assert!(err.to_string().contains("buckets.toml"), "{err}");
        assert_eq!(
            std::fs::read(registry_path(&cfg)).expect("read"),
            garbage,
            "the file is untouched"
        );
    }

    /// A location whose directory is not there may be a drive not mounted
    /// yet: neither a write nor the boot forgets it.
    #[test]
    fn a_location_under_an_absent_directory_is_never_pruned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_in(dir.path());
        let offline = dir.path().join("offline-share/team/bucket.db");
        let recorded = offline.display().to_string();
        Registry::record(&cfg, "team", &recorded).expect("record");
        let entry = DetachedEntry {
            family: "x".into(),
            base: "x.db".into(),
            removing: false,
        };
        Registry::detach(&cfg, &ProjectId::new("other").expect("id"), entry).expect("detach");

        assert_eq!(
            Registry::read(&cfg).expect("read").file_of("team"),
            Some(recorded.as_str())
        );
        let (booted, warnings) = Registry::load(&cfg);
        assert_eq!(booted.file_of("team"), Some(recorded.as_str()));
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// Detached projects and removal tombstones survive a restart, and the
    /// file goes away once nothing is recorded.
    #[test]
    fn detached_entries_and_tombstones_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_in(dir.path());
        let axle = ProjectId::new("axle").expect("id");
        let entry = DetachedEntry {
            family: "axle".into(),
            base: "D:/work/axle/.findery/bucket.db".into(),
            removing: true,
        };
        Registry::detach(&cfg, &axle, entry.clone()).expect("detach");
        Registry::mark_removing(&cfg, "discord").expect("tombstone");

        let (registry, warnings) = Registry::load(&cfg);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(registry.detached_entry(&axle), Some(&entry));
        assert!(registry.is_removing("discord"));
        assert!(!registry.is_removing("axle"));

        assert_eq!(
            Registry::reattach(&cfg, &axle).expect("reattach"),
            Some(entry)
        );
        Registry::clear_removing(&cfg, "discord").expect("clear");
        let (registry, _) = Registry::load(&cfg);
        assert!(registry.is_empty());
        assert!(!registry_path(&cfg).exists(), "nothing left to record");
    }

    /// A `buckets.toml` written before detached entries and tombstones
    /// existed reads as it always did, with none of either.
    #[test]
    fn a_registry_written_before_tombstones_still_reads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_in(dir.path());
        let file = dir.path().join("team.db");
        std::fs::write(&file, b"x").expect("touch");
        let old = format!("[buckets]\nteam = {:?}\n", file.display().to_string());
        std::fs::write(registry_path(&cfg), old).expect("write");

        let (registry, warnings) = Registry::load(&cfg);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(registry.file_of("team").is_some());
        assert!(registry.detached.is_empty());
        assert!(registry.removing_families.is_empty());
    }

    /// A hand-edited key that is not a project id, or a tombstone that is not a
    /// bucket name, is dropped with a warning rather than trusted.
    #[test]
    fn malformed_records_are_ignored_and_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_in(dir.path());
        std::fs::write(
            registry_path(&cfg),
            "removing_families = [\"../etc\", \"ok\"]\n\n\
             [detached.\"../x\"]\nfamily = \"x\"\nbase = \"x.db\"\n",
        )
        .expect("write");

        let (registry, warnings) = Registry::load(&cfg);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(registry.detached.is_empty());
        assert!(registry.is_removing("ok"));
        assert!(!registry.is_removing("../etc"));
    }
}
