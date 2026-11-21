//! Which files make up a bucket on disk.
//!
//! A **family** is a bucket as the rest of findery names it: a name and a base
//! path `dir/stem.db` — what `bucket_path`, a project manifest and
//! `buckets.toml` already hold. A **member** is one file of the family per
//! vector space, `dir/{stem}.{slug}-{tag}.db`, spelled only by
//! [`findery_core::config::bucket_member_path`]. The base file itself, when it
//! exists, is the *legacy* member: the one file a bucket had before members
//! existed, whose recipe is only known once it is read.
//!
//! INTENT: membership is decided by the file name alone and parsed back by
//! [`findery_core::config::parse_member_name`], the inverse of the one
//! function that builds it. A directory listing is then the whole truth about
//! a family — nothing to keep in sync, nothing to disagree — and a journal
//! (`-wal`, `-shm`) or another family's file (`bucket.foo.db`) can never be
//! mistaken for a member.

use std::path::{Path, PathBuf};

use findery_core::config::{StoreConfig, bucket_path, buckets_dir, parse_member_name};
use findery_core::embedding::recipe::{Recipe, RecipeTag};
use findery_core::model::{DEFAULT_BUCKET, is_bucket_name};
use findery_core::{Error, Result};

use crate::files::journal::{self, JOURNAL_SUFFIXES};

/// Where a family's files live: a directory and the stem its file names share.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FamilyPath {
    dir: PathBuf,
    stem: String,
    /// The base file's name: `{stem}.db` unless the manifest named another
    /// extension, which the legacy member then keeps.
    base_name: String,
}

impl FamilyPath {
    /// The family whose base path is `base` (`dir/stem.db`).
    ///
    /// Tolerant of being handed a member instead: `dir/stem.slug-tag.db`
    /// normalizes back to `dir/stem.db`, so a caller that only kept the path
    /// of the file it had open still finds the family. A base that happens to
    /// be spelled like a member of a shorter stem is read as that member —
    /// the price of not keeping a second record of which name is the base.
    pub fn from_base(base: &Path) -> Result<Self> {
        let file_name = base
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty())
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "bucket path {} names no file (or not a UTF-8 one)",
                    base.display()
                ))
            })?;
        let dir = base.parent().map(Path::to_path_buf).unwrap_or_default();
        // The same stem `bucket_member_path` derives, so a member built from
        // this base and a member parsed back here agree on it.
        let stem = Path::new(file_name)
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("bucket");
        if let Some((family_stem, _)) = stem.rsplit_once('.')
            && parse_member_name(family_stem, file_name).is_some()
        {
            return Ok(Self {
                dir,
                stem: family_stem.to_string(),
                base_name: format!("{family_stem}.db"),
            });
        }
        Ok(Self {
            dir,
            stem: stem.to_string(),
            base_name: file_name.to_string(),
        })
    }

    /// The directory every member sits in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The stem every member's file name starts with.
    pub fn stem(&self) -> &str {
        &self.stem
    }

    /// The base path: what manifests and the registry record, and the file
    /// the legacy member lives in.
    pub fn base(&self) -> PathBuf {
        self.dir.join(&self.base_name)
    }

    /// The member file of `recipe`, whether or not it exists yet.
    pub fn member(&self, recipe: &Recipe) -> PathBuf {
        findery_core::config::bucket_member_path(&self.base(), recipe)
    }

    /// `read_dir("")` fails; a bare file name lives in the working directory.
    fn dir_or_cwd(&self) -> &Path {
        if self.dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            &self.dir
        }
    }
}

/// One file of a family.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub path: PathBuf,
    /// The recipe tag the file name carries. `None` for the legacy member,
    /// whose name carries none: its recipe has to be read from the file.
    pub tag: Option<RecipeTag>,
    /// True for the base file itself.
    pub legacy: bool,
}

/// Every member of a family on disk: the legacy file first, then by tag.
///
/// A missing directory is an empty family, not an error — a project whose
/// `.findery/` was deleted has no files, which is a state, not a failure.
pub fn scan(family: &FamilyPath) -> Result<Vec<Member>> {
    let entries = match std::fs::read_dir(family.dir_or_cwd()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(Error::Store(format!(
                "cannot list {}: {e}",
                family.dir_or_cwd().display()
            )));
        }
    };
    let mut members = Vec::new();
    for entry in entries {
        let entry = entry
            .map_err(|e| Error::Store(format!("cannot list {}: {e}", family.dir.display())))?;
        let Ok(name) = entry.file_name().into_string() else {
            // Not a name this crate could have written.
            continue;
        };
        if !entry.path().is_file() {
            continue;
        }
        if name == family.base_name {
            members.push(Member {
                path: entry.path(),
                tag: None,
                legacy: true,
            });
        } else if let Some(tag) = parse_member_name(&family.stem, &name) {
            members.push(Member {
                path: entry.path(),
                tag: Some(tag),
                legacy: false,
            });
        }
    }
    members.sort_by_key(|m| (!m.legacy, m.tag));
    Ok(members)
}

/// True when the family has at least one file on disk, legacy or member.
///
/// INTENT: absence has to be *proven*. A directory that cannot be listed
/// (permissions, a drive not mounted yet) answers `true`, because the callers
/// prune on `false` — and pruning a family that is merely unreachable for a
/// moment forgets where an index lives, which no later boot can undo.
pub fn family_exists(family: &FamilyPath) -> bool {
    match scan(family) {
        Ok(members) => !members.is_empty(),
        Err(_) => true,
    }
}

/// True only when the family is provably gone: its directory lists, and holds
/// none of its files.
///
/// INTENT: what a registry entry is pruned on. A directory that is not there at
/// all — an unmounted drive, an offline share — proves nothing, and neither
/// does one that cannot be listed: forgetting where an index lives is not
/// something a later boot can undo.
pub fn family_absent(family: &FamilyPath) -> bool {
    if !family.dir_or_cwd().is_dir() {
        return false;
    }
    matches!(scan(family), Ok(members) if members.is_empty())
}

/// Every bucket family under the data directory, default first then names in
/// order.
///
/// Read from the filesystem rather than from a registry: a bucket *is* its
/// files, so a registry is one more thing that can disagree with the disk. A
/// directory under `buckets/` is a family as soon as it holds any member —
/// the legacy `bucket.db` or a tagged one.
pub fn list_families(cfg: &StoreConfig) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let default = FamilyPath::from_base(Path::new(&bucket_path(&cfg.data_dir, DEFAULT_BUCKET)))?;
    if family_exists(&default) {
        out.push(DEFAULT_BUCKET.to_string());
    }
    let dir = PathBuf::from(buckets_dir(&cfg.data_dir));
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // No `buckets/` directory is the ordinary single-bucket install.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    let mut named = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !is_bucket_name(&name) || name == DEFAULT_BUCKET || !entry.path().is_dir() {
            continue;
        }
        let family = FamilyPath::from_base(Path::new(&bucket_path(&cfg.data_dir, &name)))?;
        if family_exists(&family) {
            named.push(name);
        }
    }
    named.sort();
    out.extend(named);
    Ok(out)
}

/// Deletes a database together with its journals, as a step of a removal.
/// Returns whether the database itself was there.
///
/// A missing file of the set is skipped, not an error: the caller describes
/// an end state. A file another process holds is [`Error::busy_removal`]
/// naming the file and `retry`, the command to run once it is released; any
/// other failure is a store error. The database goes first and the journals after,
/// because a `-wal` with no database is inert while a database whose `-wal`
/// went first has lost its last committed pages.
pub fn remove_file_set(path: &Path, retry: &str) -> Result<bool> {
    let existed = remove_one(path, retry)?;
    for suffix in JOURNAL_SUFFIXES {
        remove_one(&journal::with_suffix(path, suffix), retry)?;
    }
    Ok(existed)
}

fn remove_one(path: &Path, retry: &str) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) if journal::is_held(&e) => {
            Err(Error::busy_removal(&path.display().to_string(), &e, retry))
        }
        Err(e) => Err(Error::Store(format!(
            "cannot delete {}: {e}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipe(raw: &str) -> Recipe {
        Recipe::parse(raw).expect("recipe")
    }

    fn touch(path: &Path) {
        std::fs::write(path, b"x").expect("touch");
    }

    #[test]
    fn a_base_path_names_its_directory_and_stem() {
        let f = FamilyPath::from_base(Path::new("/p/.findery/bucket.db")).expect("family");
        assert_eq!(f.dir(), Path::new("/p/.findery"));
        assert_eq!(f.stem(), "bucket");
        assert_eq!(f.base(), PathBuf::from("/p/.findery/bucket.db"));
        let r = recipe("mock:8");
        assert_eq!(
            f.member(&r),
            PathBuf::from(format!("/p/.findery/bucket.mock-{}.db", r.tag()))
        );
    }

    #[test]
    fn a_member_path_normalizes_back_to_its_base() {
        let r = recipe("harrier-oss-v1-0.6b:1024");
        let base = Path::new("/d/team.v2.db");
        let member = FamilyPath::from_base(base).expect("family").member(&r);
        let back = FamilyPath::from_base(&member).expect("family");
        assert_eq!(back.base(), base);
        assert_eq!(back.stem(), "team.v2");
        assert_eq!(back, FamilyPath::from_base(base).expect("family"));
    }

    #[test]
    fn a_base_with_another_extension_keeps_it_for_its_legacy_file() {
        let f = FamilyPath::from_base(Path::new("/d/findery.sqlite")).expect("family");
        assert_eq!(f.stem(), "findery");
        assert_eq!(f.base(), PathBuf::from("/d/findery.sqlite"));
    }

    #[test]
    fn a_path_with_no_file_name_is_refused() {
        for bad in ["", "/", ".."] {
            assert_eq!(
                FamilyPath::from_base(Path::new(bad)).map_err(|e| e.code()),
                Err("invalid"),
                "{bad:?}"
            );
        }
    }

    /// The whole point of parsing names: a directory holding journals, a
    /// longer stem's members and stray files lists exactly the family's own.
    #[test]
    fn a_scan_sees_members_and_nothing_that_merely_looks_like_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let family = FamilyPath::from_base(&dir.path().join("bucket.db")).expect("family");
        let (a, b) = (recipe("mock:8"), recipe("harrier-oss-v1-0.6b:1024"));
        let member_b = family.member(&b);
        touch(&family.member(&a));
        touch(&member_b);
        touch(&family.base());
        for noise in [
            "bucket.db-wal".to_string(),
            "bucket.db-shm".to_string(),
            format!("bucket.mock-{}.db-wal", a.tag()),
            format!("bucket.mock-{}.db-shm", a.tag()),
            format!("bucket.foo.mock-{}.db", a.tag()),
            "bucket.foo.db".to_string(),
            "buckets.toml".to_string(),
        ] {
            touch(&dir.path().join(noise));
        }
        std::fs::create_dir(dir.path().join(format!("bucket.dir-{}.db", a.tag()))).expect("mkdir");

        let members = scan(&family).expect("scan");
        assert_eq!(members.len(), 3, "{members:?}");
        assert!(members[0].legacy && members[0].tag.is_none());
        let mut tags: Vec<_> = vec![a.tag(), b.tag()];
        tags.sort();
        assert_eq!(
            members[1..].iter().map(|m| m.tag).collect::<Vec<_>>(),
            tags.into_iter().map(Some).collect::<Vec<_>>()
        );
        assert!(members.iter().any(|m| m.path == member_b));
    }

    #[test]
    fn a_family_with_no_directory_is_empty_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let family = FamilyPath::from_base(&dir.path().join("gone/bucket.db")).expect("family");
        assert!(scan(&family).expect("scan").is_empty());
        assert!(!family_exists(&family));
        assert!(
            !family_absent(&family),
            "an absent directory proves nothing"
        );
    }

    #[test]
    fn a_listed_directory_with_no_member_proves_the_family_gone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let family = FamilyPath::from_base(&dir.path().join("bucket.db")).expect("family");
        touch(&dir.path().join("unrelated.txt"));
        assert!(family_absent(&family));
        touch(&family.member(&recipe("mock:8")));
        assert!(!family_absent(&family));
    }

    /// A named bucket is listed as soon as its directory holds any member, so
    /// a bucket whose base was renamed to a member does not vanish.
    #[test]
    fn a_directory_with_only_a_tagged_member_is_still_a_family() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = StoreConfig {
            data_dir: dir.path().display().to_string(),
            ..StoreConfig::default()
        };
        let r = recipe("mock:8");
        let work = dir.path().join("buckets/work");
        std::fs::create_dir_all(&work).expect("mkdir");
        touch(
            &FamilyPath::from_base(&work.join("bucket.db"))
                .expect("f")
                .member(&r),
        );
        std::fs::create_dir_all(dir.path().join("buckets/empty")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join("buckets/Not A Name")).expect("mkdir");
        touch(&dir.path().join("buckets/Not A Name/bucket.db"));
        touch(&dir.path().join("bucket.db"));

        assert_eq!(
            list_families(&cfg).expect("list"),
            vec![DEFAULT_BUCKET.to_string(), "work".to_string()]
        );
    }

    #[test]
    fn removing_a_file_set_takes_the_journals_and_tolerates_absence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("x.db");
        touch(&db);
        touch(&journal::with_suffix(&db, "-wal"));
        assert!(remove_file_set(&db, "retry").expect("remove"));
        assert!(!db.exists());
        assert!(!journal::with_suffix(&db, "-wal").exists());
        assert!(!remove_file_set(&db, "retry").expect("absent is an end state"));
    }

    #[test]
    fn a_file_set_that_cannot_be_removed_is_an_error_naming_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A non-empty directory where the file should be: `remove_file`
        // refuses it on every platform.
        let db = dir.path().join("x.db");
        std::fs::create_dir(&db).expect("mkdir");
        touch(&db.join("keep"));
        let err = remove_file_set(&db, "findery bucket rm x").expect_err("must refuse");
        assert!(err.to_string().contains("x.db"), "{err}");
    }

    /// Windows refuses to delete a file another handle holds; the refusal is
    /// `Busy`, naming the file and the command to run again.
    #[cfg(windows)]
    #[test]
    fn a_file_held_open_is_busy_with_a_retry_hint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("held.db");
        let conn = rusqlite::Connection::open(&db).expect("open");
        conn.execute_batch("CREATE TABLE t (x)").expect("write");
        let err = remove_file_set(&db, "findery bucket rm held").expect_err("held");
        assert_eq!(err.code(), "busy");
        assert!(err.to_string().contains("findery bucket rm held"), "{err}");
        drop(conn);
        assert!(remove_file_set(&db, "retry").expect("released"));
    }
}
