//! Moving files written under an older layout onto the current one.
//!
//! Two generations live here, oldest first:
//!
//! * [`crate::migrate_legacy_layout`] (in `migrate.rs`) — the vocabulary change
//!   (`catalog.db` → `bucket.db`, `catalogs/` → `buckets/`). A move it cannot
//!   make stops the boot, because nothing downstream ever looks at the old
//!   name again.
//! * [`adopt_legacy`] — the per-recipe members. A bucket's single `bucket.db`
//!   becomes the member of the recipe it records,
//!   `bucket.{slug}-{tag}.db`. A move it cannot make does **not** stop
//!   anything: the base file stays where it is as the *legacy* member, which
//!   the resolver consults before creating, so the index keeps serving and the
//!   move is retried at the next start.
//!
//! INTENT: both are "complete this rename if it is still owed", so they are
//! idempotent and resumable, and both refuse to rename a database whose
//! journal still holds committed pages — a rename is not atomic across the
//! file set, and pages detached from their database are lost.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use findery_core::config::{StoreConfig, bucket_path, buckets_dir};
use findery_core::embedding::recipe::Recipe;
use findery_core::model::{DEFAULT_BUCKET, is_bucket_name};
use findery_core::{Error, Result};

use crate::bucket::{read_project_ids_no_create, read_recipe};
use crate::files::journal::{self, JOURNAL_SUFFIXES, checkpoint_truncate, with_suffix};
use crate::files::layout::FamilyPath;
use crate::files::registry::Registry;

/// What [`adopt_legacy`] did with one family's base file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdoptOutcome {
    /// No base file: nothing was owed. What every run after the first sees.
    Absent,
    /// The base file records no recipe yet. It stays where it is as an
    /// unclaimed member, and the first write claims it.
    Unclaimed { path: PathBuf },
    /// The member the base file would become already exists. Nothing was
    /// touched — never overwrite — and the tagged member is the one to serve.
    Stale {
        legacy: PathBuf,
        target: PathBuf,
        recipe: String,
    },
    /// The base file is now the member of the recipe it records.
    Adopted {
        from: PathBuf,
        to: PathBuf,
        recipe: String,
    },
    /// The base file could not be moved, and stays where it is as the legacy
    /// member serving its recipe. Retried at the next start.
    KeptInPlace {
        path: PathBuf,
        recipe: Option<String>,
        reason: String,
    },
}

impl AdoptOutcome {
    /// True for the outcomes a user should hear about in `health`.
    pub fn needs_attention(&self) -> bool {
        matches!(self, Self::Stale { .. } | Self::KeptInPlace { .. })
    }
}

/// Renames a family's base file to the member of the recipe it records.
///
/// The algorithm, each step a reason to stop with the file untouched:
///
/// 1. open it read-write **without creating** (busy timeout 2 s) and read
///    `meta.embedding_recipe` — none: [`AdoptOutcome::Unclaimed`];
/// 2. the target member already exists: [`AdoptOutcome::Stale`];
/// 3. checkpoint TRUNCATE, **checked**, then close; a journal still on disk
///    afterwards means another connection has the file open;
/// 4. rename the database, then any journal that appeared meanwhile.
///
/// Every failure that leaves the base file in place — a lock (Windows os
/// error 32 or 5), a busy reader, a read-only volume, a recipe no member name
/// can be built from — is [`AdoptOutcome::KeptInPlace`], never an `Err`:
/// the file in place is a working legacy member, and refusing a boot over it
/// would take every other index down with it. `Err` means exactly one thing:
/// the database moved and a journal behind it could not follow, which is the
/// one state a person has to look at.
pub fn adopt_legacy(family: &FamilyPath, busy: Duration) -> Result<AdoptOutcome> {
    let legacy = family.base();
    if !legacy.is_file() {
        return Ok(AdoptOutcome::Absent);
    }
    let kept = |recipe: Option<&str>, reason: String| {
        Ok(AdoptOutcome::KeptInPlace {
            path: legacy.clone(),
            recipe: recipe.map(str::to_string),
            reason,
        })
    };

    let conn = match Connection::open_with_flags(
        &legacy,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .and_then(|c| c.busy_timeout(busy).map(|()| c))
    {
        Ok(conn) => conn,
        Err(e) => return kept(None, format!("it cannot be opened for writing: {e}")),
    };
    let raw = match read_recipe(&conn) {
        Ok(Some(raw)) => raw,
        Ok(None) => return Ok(AdoptOutcome::Unclaimed { path: legacy }),
        Err(e) => return kept(None, format!("its recipe cannot be read: {e}")),
    };
    let recipe = match Recipe::parse(&raw) {
        Ok(recipe) => recipe,
        Err(e) => {
            return kept(
                Some(&raw),
                format!("no member file can be named after it: {e}"),
            );
        }
    };
    let target = family.member(&recipe);
    if target.exists() {
        return Ok(AdoptOutcome::Stale {
            legacy,
            target,
            recipe: raw,
        });
    }

    match checkpoint_truncate(&conn) {
        Ok(row) if row.complete() => {}
        Ok(row) => return kept(Some(&raw), row.describe(&legacy)),
        Err(e) => {
            return kept(
                Some(&raw),
                format!("its journal cannot be checkpointed: {e}"),
            );
        }
    }
    if let Err((_, e)) = conn.close() {
        return kept(Some(&raw), format!("it cannot be closed: {e}"));
    }
    // The last connection to close deletes both journals. One still there is
    // another connection — another process — with the file open, and a rename
    // under it would leave that process writing a journal beside nothing.
    if let Some(open) = JOURNAL_SUFFIXES
        .iter()
        .map(|suffix| with_suffix(&legacy, suffix))
        .find(|p| p.exists())
    {
        return kept(
            Some(&raw),
            format!(
                "another connection still has it open ({} is still there)",
                open.display()
            ),
        );
    }
    if let Err(e) = std::fs::rename(&legacy, &target) {
        let why = if journal::is_held(&e) {
            format!("it is held by another process ({e})")
        } else {
            format!("it cannot be renamed to {}: {e}", target.display())
        };
        return kept(Some(&raw), why);
    }
    // Only a connection opened between the check above and the rename can
    // have left these; follow the database rather than strand them.
    for suffix in JOURNAL_SUFFIXES {
        let from = with_suffix(&legacy, suffix);
        if from.exists() {
            let to = with_suffix(&target, suffix);
            std::fs::rename(&from, &to).map_err(|e| {
                Error::Store(format!(
                    "{} was renamed to {}, but {} could not follow it ({e}); move it by hand \
                     (`mv {} {}`) before anything opens the database",
                    legacy.display(),
                    target.display(),
                    from.display(),
                    from.display(),
                    to.display()
                ))
            })?;
        }
    }
    Ok(AdoptOutcome::Adopted {
        from: legacy,
        to: target,
        recipe: raw,
    })
}

/// What adoption did for one bucket.
#[derive(Debug)]
pub struct AdoptNotice {
    /// The bucket name, or the directory when the directory itself failed.
    pub bucket: String,
    /// The base path examined.
    pub base: PathBuf,
    /// What adoption did with it; `Err` only when the database moved and a
    /// journal behind it could not follow.
    pub outcome: Result<AdoptOutcome>,
}

impl AdoptNotice {
    /// True for what a user should hear about: a kept or stale file, or a
    /// failure.
    pub fn needs_attention(&self) -> bool {
        match &self.outcome {
            Ok(outcome) => outcome.needs_attention(),
            Err(_) => true,
        }
    }

    /// One line, for a log or for `health`.
    pub fn describe(&self) -> String {
        let b = &self.bucket;
        match &self.outcome {
            Ok(AdoptOutcome::Absent) => format!("bucket `{b}`: nothing to adopt"),
            Ok(AdoptOutcome::Unclaimed { path }) => format!(
                "bucket `{b}`: {} records no recipe yet; the first sync claims it",
                path.display()
            ),
            Ok(AdoptOutcome::Stale {
                legacy,
                target,
                recipe,
            }) => format!(
                "bucket `{b}`: {} was left as it is because {} already holds `{recipe}`; \
                 delete {} once you no longer need it",
                legacy.display(),
                target.display(),
                legacy.display()
            ),
            Ok(AdoptOutcome::Adopted { from, to, recipe }) => format!(
                "bucket `{b}`: {} is now {} (`{recipe}`)",
                from.display(),
                to.display()
            ),
            Ok(AdoptOutcome::KeptInPlace {
                path,
                recipe,
                reason,
            }) => format!(
                "bucket `{b}`: {} stays where it is because {reason}; it keeps serving `{}`, \
                 and the move is retried at the next start",
                path.display(),
                recipe.as_deref().unwrap_or("no recipe yet")
            ),
            Err(e) => format!("bucket `{b}`: adopting {} failed: {e}", self.base.display()),
        }
    }
}

/// Adopts the base file of every bucket this data directory knows: the
/// default one, each `buckets/<name>/`, and each registry entry.
///
/// Returns one notice per bucket that had a base file (an [`Absent`]
/// outcome is not reported). Never fails: a bucket that cannot be adopted is
/// a notice, and the next bucket is still tried. Blocking — SQLite and the
/// filesystem, with a 2 s busy timeout per file — so an async caller runs it
/// on the blocking pool.
///
/// A bucket whose every project is detached is skipped, as the boot skips
/// opening it: `findery detach` promised those files stay put.
///
/// [`Absent`]: AdoptOutcome::Absent
pub fn adopt_all(cfg: &StoreConfig, registry: &Registry) -> Vec<AdoptNotice> {
    let mut bases = vec![(
        DEFAULT_BUCKET.to_string(),
        PathBuf::from(bucket_path(&cfg.data_dir, DEFAULT_BUCKET)),
    )];
    let mut notices = Vec::new();
    let dir = PathBuf::from(buckets_dir(&cfg.data_dir));
    match std::fs::read_dir(&dir) {
        Ok(entries) => {
            let mut named: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| is_bucket_name(n) && n != DEFAULT_BUCKET)
                .collect();
            named.sort();
            for name in named {
                let base = PathBuf::from(bucket_path(&cfg.data_dir, &name));
                bases.push((name, base));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => notices.push(AdoptNotice {
            bucket: dir.display().to_string(),
            base: dir.clone(),
            outcome: Err(Error::Store(format!("cannot list {}: {e}", dir.display()))),
        }),
    }
    for (name, file) in &registry.buckets {
        bases.push((name.clone(), PathBuf::from(file)));
    }

    let busy = cfg.peek_busy_timeout();
    let mut seen = HashSet::new();
    for (bucket, base) in bases {
        if !seen.insert(base.clone()) {
            continue;
        }
        let family = FamilyPath::from_base(&base);
        if bucket != DEFAULT_BUCKET && family.as_ref().is_ok_and(|f| released(f, registry, busy)) {
            continue;
        }
        let outcome = family.and_then(|family| adopt_legacy(&family, busy));
        if matches!(outcome, Ok(AdoptOutcome::Absent)) {
            continue;
        }
        notices.push(AdoptNotice {
            bucket,
            base,
            outcome,
        });
    }
    notices
}

/// True when the family holds projects and every one is detached. A file that
/// cannot be read is not released: adoption then reports it.
fn released(family: &FamilyPath, registry: &Registry, busy: Duration) -> bool {
    let Ok(members) = crate::files::layout::scan(family) else {
        return false;
    };
    let mut ids = Vec::new();
    for member in members {
        match read_project_ids_no_create(&member.path, busy) {
            Ok(found) => ids.extend(found),
            Err(_) => return false,
        }
    }
    !ids.is_empty() && ids.iter().all(|id| registry.detached.contains_key(id))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::files::journal::journal_holds_pages;
    use crate::files::migrate::migrate_legacy_layout;
    use crate::{Bucket, list_families, open_bucket};

    const BUSY: Duration = Duration::from_secs(2);

    fn config_in(dir: &Path) -> StoreConfig {
        StoreConfig {
            data_dir: dir.display().to_string(),
            ..StoreConfig::default()
        }
    }

    // -- the vocabulary migration ------------------------------------------

    /// An install created before the vocabulary change is moved onto the current
    /// names, once, and its content comes with it.
    #[tokio::test]
    async fn an_existing_install_is_moved_onto_the_current_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_in(dir.path());
        {
            let legacy = Bucket::open_file(&dir.path().join("catalog.db"), DEFAULT_BUCKET)
                .expect("legacy file");
            legacy.set_meta("k", "v").await.expect("write");
        }

        migrate_legacy_layout(&cfg).expect("migrates");
        assert!(!dir.path().join("catalog.db").exists());
        let moved = open_bucket(&cfg, DEFAULT_BUCKET).expect("moved");
        assert_eq!(moved.get_meta("k").await.expect("read"), Some("v".into()));
    }

    /// A migration that ran halfway leaves both names on disk. The current one
    /// wins, and nothing is overwritten.
    #[tokio::test]
    async fn a_half_finished_migration_never_overwrites_the_current_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_in(dir.path());
        {
            let legacy = Bucket::open_file(&dir.path().join("catalog.db"), DEFAULT_BUCKET)
                .expect("legacy file");
            legacy.set_meta("k", "old").await.expect("write");
            let current = open_bucket(&cfg, DEFAULT_BUCKET).expect("current file");
            current.set_meta("k", "current").await.expect("write");
        }

        migrate_legacy_layout(&cfg).expect("migrates");
        let current = open_bucket(&cfg, DEFAULT_BUCKET).expect("reopen");
        assert_eq!(
            current.get_meta("k").await.expect("read"),
            Some("current".into())
        );
    }

    /// A named bucket leaves `catalogs/` for `buckets/`, and is listed where the
    /// rest of the daemon expects to find it.
    #[tokio::test]
    async fn a_named_bucket_moves_out_of_the_legacy_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_in(dir.path());
        let legacy = dir.path().join("catalogs/work");
        std::fs::create_dir_all(&legacy).expect("legacy dir");
        Bucket::open_file(&legacy.join("catalog.db"), "work").expect("legacy bucket");

        migrate_legacy_layout(&cfg).expect("migrates");
        assert_eq!(list_families(&cfg).expect("list"), vec!["work".to_string()]);
        assert!(open_bucket(&cfg, "work").is_ok());
    }

    /// The failure this whole path exists to prevent. A move that cannot be made
    /// must stop the boot: nothing downstream ever looks at the old name again,
    /// and opening *creates* — so the alternative is a fresh empty database
    /// beside the real index, with the real index orphaned and the user told
    /// nothing.
    #[test]
    fn a_legacy_file_that_cannot_be_moved_stops_the_boot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_in(dir.path());
        Bucket::open_file(&dir.path().join("catalog.db"), DEFAULT_BUCKET).expect("legacy file");
        // A non-empty directory sits where the migrated file has to land, so the
        // rename cannot succeed.
        let blocked = dir.path().join("bucket.db");
        std::fs::create_dir_all(&blocked).expect("mkdir");
        std::fs::write(blocked.join("keep"), b"x").expect("write");

        let err = migrate_legacy_layout(&cfg).expect_err("must refuse");
        let text = err.to_string();
        assert!(text.contains("catalog.db"), "{text}");
        assert!(text.contains("mv "), "the refusal names the repair: {text}");
        assert!(
            dir.path().join("catalog.db").is_file(),
            "the index is still where it was"
        );
    }

    /// A live database copied with its journal — the shape a process that was
    /// killed leaves behind. Built in `source` and *copied* to `dest` under
    /// `name`, because the point is a file set nothing holds open; closed only
    /// afterwards, since a clean close checkpoints the journal away.
    fn killed_writer_fixture(dest: &Path, name: &str, recipe: Option<&str>) {
        let source = tempfile::tempdir().expect("tempdir");
        let conn = Connection::open(source.path().join("db")).expect("open");
        conn.pragma_update(None, "journal_mode", "WAL")
            .expect("wal");
        conn.execute_batch(
            "CREATE TABLE projects (id TEXT); INSERT INTO projects VALUES ('axle');
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .expect("schema");
        if let Some(recipe) = recipe {
            conn.execute("INSERT INTO meta VALUES ('embedding_recipe', ?1)", [recipe])
                .expect("recipe");
        }
        for suffix in ["", "-wal", "-shm"] {
            let from = source.path().join(format!("db{suffix}"));
            if from.is_file() {
                std::fs::copy(&from, dest.join(format!("{name}{suffix}"))).expect("copy");
            }
        }
        drop(conn);
        assert!(
            journal_holds_pages(&dest.join(name)),
            "the fixture needs committed pages in the journal"
        );
    }

    fn count_projects(db: &Path) -> i64 {
        Connection::open(db)
            .expect("reopen")
            .query_row("SELECT count(*) FROM projects", [], |r| r.get(0))
            .expect("count")
    }

    /// A database whose journal still holds committed pages is moved by first
    /// letting SQLite put those pages back. Refusing instead leaves the user to
    /// resolve it by hand, and a real install has exactly this shape: a journal
    /// from the last time the old daemon was killed.
    #[test]
    fn a_journal_holding_pages_is_consolidated_before_the_move() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_in(dir.path());
        killed_writer_fixture(dir.path(), "catalog.db", None);

        migrate_legacy_layout(&cfg).expect("migrates");

        assert!(dir.path().join("bucket.db").is_file());
        assert!(!dir.path().join("catalog.db").exists());
        assert_eq!(
            count_projects(&dir.path().join("bucket.db")),
            1,
            "the committed row came with it"
        );
    }

    /// An empty journal is what a cleanly closed database leaves, and it must
    /// not block an ordinary install from moving.
    #[test]
    fn an_empty_journal_does_not_block_the_move() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_in(dir.path());
        std::fs::write(dir.path().join("catalog.db"), b"db").expect("write");
        std::fs::write(dir.path().join("catalog.db-wal"), b"").expect("write");

        migrate_legacy_layout(&cfg).expect("migrates");
        assert!(dir.path().join("bucket.db").is_file());
        assert!(dir.path().join("bucket.db-wal").is_file());
        assert!(!dir.path().join("catalog.db-wal").exists());
    }

    /// A run interrupted between moving a bucket's directory and renaming the
    /// file inside it leaves a bucket no scan can see. The next run has to
    /// finish it, and `catalogs/` is already empty by then, so the recovery
    /// cannot depend on finding the old entry.
    #[test]
    fn a_bucket_left_half_migrated_is_finished_on_the_next_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_in(dir.path());
        let stranded = dir.path().join("buckets").join("discord");
        std::fs::create_dir_all(&stranded).expect("mkdir");
        Bucket::open_file(&stranded.join("catalog.db"), "discord").expect("legacy bucket");

        migrate_legacy_layout(&cfg).expect("migrates");

        assert!(stranded.join("bucket.db").is_file());
        assert!(!stranded.join("catalog.db").exists());
        assert_eq!(
            list_families(&cfg).expect("list"),
            vec!["discord".to_string()],
            "the bucket is visible again"
        );
    }

    /// Running the migration over an install that is already current changes
    /// nothing: it runs on every boot.
    #[test]
    fn a_migrated_install_is_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_in(dir.path());
        let bucket = open_bucket(&cfg, DEFAULT_BUCKET).expect("bucket");
        drop(bucket);

        migrate_legacy_layout(&cfg).expect("first");
        let before = std::fs::read(dir.path().join("bucket.db")).expect("read");
        migrate_legacy_layout(&cfg).expect("second");
        assert_eq!(
            std::fs::read(dir.path().join("bucket.db")).expect("read"),
            before,
            "an already-migrated install is untouched"
        );
    }

    // -- adoption into per-recipe members ----------------------------------

    /// A legacy base file stamped with `recipe`, closed cleanly.
    async fn legacy_bucket(base: &Path, recipe: Option<&str>) {
        let b = Bucket::open_file(base, "work").expect("create");
        b.set_meta("k", "v").await.expect("write");
        if let Some(recipe) = recipe {
            b.claim_recipe(recipe).await.expect("claim");
        }
        b.close().await.expect("close");
    }

    fn family(base: &Path) -> FamilyPath {
        FamilyPath::from_base(base).expect("family")
    }

    fn recipe(raw: &str) -> Recipe {
        Recipe::parse(raw).expect("recipe")
    }

    #[tokio::test]
    async fn a_legacy_file_becomes_the_member_of_its_recipe_with_its_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join("bucket.db");
        legacy_bucket(&base, Some("mock:8")).await;

        let outcome = adopt_legacy(&family(&base), BUSY).expect("adopt");
        let target = family(&base).member(&recipe("mock:8"));
        assert_eq!(
            outcome,
            AdoptOutcome::Adopted {
                from: base.clone(),
                to: target.clone(),
                recipe: "mock:8".into()
            }
        );
        assert!(!base.exists());
        for suffix in JOURNAL_SUFFIXES {
            assert!(!with_suffix(&base, suffix).exists(), "{suffix} left behind");
        }
        let moved = Bucket::open_existing(&target, "work").expect("member opens");
        assert_eq!(moved.get_meta("k").await.expect("read"), Some("v".into()));
        assert_eq!(
            moved.get_meta("embedding_recipe").await.expect("read"),
            Some("mock:8".into())
        );
    }

    #[test]
    fn journal_pages_are_folded_back_before_the_rename() {
        let dir = tempfile::tempdir().expect("tempdir");
        killed_writer_fixture(dir.path(), "bucket.db", Some("mock:8"));
        let base = dir.path().join("bucket.db");

        let outcome = adopt_legacy(&family(&base), BUSY).expect("adopt");
        let target = family(&base).member(&recipe("mock:8"));
        assert!(
            matches!(&outcome, AdoptOutcome::Adopted { to, .. } if *to == target),
            "{outcome:?}"
        );
        assert!(
            !journal_holds_pages(&target),
            "no journal travelled with pages"
        );
        assert!(!with_suffix(&base, "-wal").exists());
        assert_eq!(count_projects(&target), 1, "the committed row came with it");
    }

    #[tokio::test]
    async fn adopting_twice_changes_nothing_the_second_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join("bucket.db");
        legacy_bucket(&base, Some("mock:8")).await;
        adopt_legacy(&family(&base), BUSY).expect("first");
        let target = family(&base).member(&recipe("mock:8"));
        let before = std::fs::read(&target).expect("read");

        assert_eq!(
            adopt_legacy(&family(&base), BUSY).expect("second"),
            AdoptOutcome::Absent
        );
        assert_eq!(std::fs::read(&target).expect("read"), before);
    }

    /// Never overwrite: a member that already exists wins, and the legacy
    /// file is left for the user to decide about.
    #[tokio::test]
    async fn an_existing_member_is_never_overwritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join("bucket.db");
        legacy_bucket(&base, Some("mock:8")).await;
        let target = family(&base).member(&recipe("mock:8"));
        legacy_bucket(&target, Some("mock:8")).await;
        let (legacy_bytes, target_bytes) = (
            std::fs::read(&base).expect("read"),
            std::fs::read(&target).expect("read"),
        );

        let outcome = adopt_legacy(&family(&base), BUSY).expect("adopt");
        assert_eq!(
            outcome,
            AdoptOutcome::Stale {
                legacy: base.clone(),
                target: target.clone(),
                recipe: "mock:8".into()
            }
        );
        assert!(outcome.needs_attention());
        assert_eq!(std::fs::read(&base).expect("read"), legacy_bytes);
        assert_eq!(std::fs::read(&target).expect("read"), target_bytes);
    }

    /// The case adoption must survive on a real machine: another process (here
    /// another connection) has the file open. The file stays, still serves, and
    /// nothing is lost — the rename is only retried later.
    #[tokio::test]
    async fn a_file_held_open_elsewhere_is_kept_in_place_and_keeps_serving() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join("bucket.db");
        legacy_bucket(&base, Some("mock:8")).await;
        let holder = Connection::open(&base).expect("holder");
        let _: i64 = holder
            .query_row("SELECT count(*) FROM meta", [], |r| r.get(0))
            .expect("the holder has read the file, so it holds its journals");

        let outcome = adopt_legacy(&family(&base), BUSY).expect("never an error");
        let AdoptOutcome::KeptInPlace {
            path,
            recipe: kept_recipe,
            reason,
        } = &outcome
        else {
            panic!("expected KeptInPlace, got {outcome:?}");
        };
        assert_eq!(*path, base);
        assert_eq!(kept_recipe.as_deref(), Some("mock:8"));
        assert!(!reason.is_empty());
        assert!(outcome.needs_attention());
        assert!(!family(&base).member(&recipe("mock:8")).exists());

        let serving = Bucket::open_file(&base, "work").expect("still opens");
        assert_eq!(
            serving.get_meta("k").await.expect("serves"),
            Some("v".into())
        );
        drop(holder);
    }

    /// A reader holding a snapshot blocks the checkpoint: the row says `busy`,
    /// and moving on would rename a journal that still owns pages.
    #[tokio::test]
    async fn a_checkpoint_a_reader_blocks_keeps_the_file_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join("bucket.db");
        legacy_bucket(&base, Some("mock:8")).await;
        let reader = Connection::open(&base).expect("reader");
        reader
            .execute_batch("BEGIN; SELECT count(*) FROM meta;")
            .expect("snapshot");
        let writer = Bucket::open_file(&base, "work").expect("writer");
        writer
            .set_meta("late", "page")
            .await
            .expect("a page the reader does not see");

        let outcome = adopt_legacy(&family(&base), BUSY).expect("never an error");
        assert!(
            matches!(&outcome, AdoptOutcome::KeptInPlace { .. }),
            "{outcome:?}"
        );
        assert!(base.is_file());
        reader.execute_batch("COMMIT").expect("release");
        assert_eq!(
            writer.get_meta("late").await.expect("serves"),
            Some("page".into())
        );
    }

    #[tokio::test]
    async fn a_file_with_no_recipe_stays_as_an_unclaimed_member() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join("bucket.db");
        legacy_bucket(&base, None).await;
        let outcome = adopt_legacy(&family(&base), BUSY).expect("adopt");
        assert_eq!(outcome, AdoptOutcome::Unclaimed { path: base.clone() });
        assert!(!outcome.needs_attention());
        assert!(base.is_file());
    }

    /// A width-0 recipe is what the old double builder could write. No member
    /// can be named after it, so it stays where it is and says why.
    #[tokio::test]
    async fn a_recipe_no_member_can_be_named_after_is_kept_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join("bucket.db");
        legacy_bucket(&base, Some("model:0")).await;
        let outcome = adopt_legacy(&family(&base), BUSY).expect("adopt");
        assert!(
            matches!(&outcome, AdoptOutcome::KeptInPlace { recipe: Some(r), .. } if r == "model:0"),
            "{outcome:?}"
        );
        assert!(base.is_file());
    }

    fn add_project(db: &Path, id: &str) {
        Connection::open(db)
            .expect("open")
            .execute(
                "INSERT INTO projects (id, name, spec_json, host_root, created_at, updated_at)
                 VALUES (?1, ?1, '{}', '/x', 0, 0)",
                [id],
            )
            .expect("row");
    }

    fn detached(registry: &mut Registry, id: &str, family: &str) {
        registry.detached.insert(
            id.into(),
            crate::files::registry::DetachedEntry {
                family: family.into(),
                base: String::new(),
                removing: false,
            },
        );
    }

    /// `detach` promised the files stay put: a bucket whose every project is
    /// detached is neither opened nor renamed. One attached project is enough
    /// for the bucket to be served, and so adopted.
    #[tokio::test]
    async fn a_bucket_whose_projects_are_all_detached_is_left_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_in(dir.path());
        let gone = dir.path().join("buckets/gone/bucket.db");
        let mixed = dir.path().join("buckets/mixed/bucket.db");
        for (base, ids) in [(&gone, ["a", "b"]), (&mixed, ["c", "d"])] {
            legacy_bucket(base, Some("mock:8")).await;
            for id in ids {
                add_project(base, id);
            }
        }
        let mut registry = Registry::default();
        for (id, family) in [("a", "gone"), ("b", "gone"), ("c", "mixed")] {
            detached(&mut registry, id, family);
        }

        let notices = adopt_all(&cfg, &registry);
        let names: Vec<&str> = notices.iter().map(|n| n.bucket.as_str()).collect();
        assert_eq!(names, vec!["mixed"], "{notices:?}");
        assert!(gone.is_file(), "the released file did not move");
        assert!(!mixed.exists(), "the served one was adopted");
    }

    /// Every bucket the data directory knows is visited once: the default one,
    /// a named one, and one only the registry can find.
    #[tokio::test]
    async fn every_known_bucket_is_adopted_once_and_a_rerun_reports_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_in(dir.path());
        let default = dir.path().join("bucket.db");
        let named = dir.path().join("buckets/work/bucket.db");
        let elsewhere = dir.path().join("project/.findery/bucket.db");
        for base in [&default, &named, &elsewhere] {
            legacy_bucket(base, Some("mock:8")).await;
        }
        let mut registry = Registry::default();
        registry
            .buckets
            .insert("proj".into(), elsewhere.display().to_string());
        // The same file under a second name is visited once.
        registry
            .buckets
            .insert("again".into(), named.display().to_string());

        let notices = adopt_all(&cfg, &registry);
        assert_eq!(notices.len(), 3, "{notices:?}");
        for notice in &notices {
            assert!(
                matches!(notice.outcome, Ok(AdoptOutcome::Adopted { .. })),
                "{}",
                notice.describe()
            );
            assert!(!notice.needs_attention());
        }
        let names: Vec<&str> = notices.iter().map(|n| n.bucket.as_str()).collect();
        assert_eq!(names, vec![DEFAULT_BUCKET, "work", "proj"]);
        assert!(
            adopt_all(&cfg, &registry).is_empty(),
            "nothing left to adopt"
        );
        assert_eq!(
            list_families(&cfg).expect("list"),
            vec![DEFAULT_BUCKET.to_string(), "work".to_string()],
            "the adopted families are still listed"
        );
    }
}
