//! A bucket **family** and its **members**: one bucket as findery names it, and
//! the one file per vector space it holds.
//!
//! INTENT: a family is what a manifest, `buckets.toml` and `findery bucket ls`
//! call a bucket — a name and a base path. Its members are the files that hold
//! one recipe's vectors each, so switching model never refuses, renames or
//! overwrites anything: the other model's file stays beside the new one, and
//! switching back costs nothing. The resolver finds a member by the recipe tag
//! its file name carries; the file's own `meta.embedding_recipe` is compared to
//! the recipe asked for at every operation, which is what catches a renamed
//! file or a tag collision (CONV-013: the cache key is the recipe, never a
//! guess about it).
//!
//! Ownership: the engine's family map owns every [`BucketFamily`]; a family owns
//! its members; a member is created only under the family's `create` mutex, by
//! [`BucketFamily::ensure_member`], and only for a write. Once closed, a family
//! never creates a file again.

pub use crate::storage::member::{BucketHandle, shown};

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::{Mutex, RwLock};

use findery_core::api::BucketInfo;
use findery_core::config::StoreConfig;
use findery_core::embedding::recipe::{Recipe, RecipeTag};
use findery_core::ids::ProjectId;
use findery_core::model::Project;
use findery_core::{Error, Result};

use findery_store::{Bucket, FamilyPath};

use crate::storage::blocking::blocking;
use crate::storage::member::MemberSpec;

/// One bucket: a name, a base path, and the member files open under it.
pub struct BucketFamily {
    /// The name the bucket answers to.
    pub name: String,
    /// Its base path, which every member's file name is derived from.
    pub base: FamilyPath,
    /// True when `base` is where the name alone derives it.
    pub at_derived_path: bool,
    members: RwLock<BTreeMap<RecipeTag, Arc<BucketHandle>>>,
    /// The base file when it records no recipe yet, open: its rows are read
    /// like any member's, and the first write claims it instead of creating a
    /// member beside it.
    unclaimed: Mutex<Option<Bucket>>,
    /// Held while a member is created or removed, so two writes under one new
    /// model create one file, and a removal never races a creation.
    create: tokio::sync::Mutex<()>,
    /// Set by [`BucketFamily::close`]: a closed family creates nothing.
    closed: AtomicBool,
}

impl BucketFamily {
    /// A family with no member open yet.
    pub(crate) fn new(cfg: &StoreConfig, name: &str, base: FamilyPath) -> Self {
        let derived = findery_core::config::bucket_path(&cfg.data_dir, name);
        let at_derived_path = base.base() == Path::new(&derived);
        Self {
            name: name.to_string(),
            base,
            at_derived_path,
            members: RwLock::new(BTreeMap::new()),
            unclaimed: Mutex::new(None),
            create: tokio::sync::Mutex::new(()),
            closed: AtomicBool::new(false),
        }
    }

    /// Every open member, by tag.
    pub fn members(&self) -> Vec<Arc<BucketHandle>> {
        self.members.read().values().cloned().collect()
    }

    /// The member holding `tag`'s vectors, when it is open.
    pub fn member(&self, tag: &RecipeTag) -> Option<Arc<BucketHandle>> {
        self.members.read().get(tag).cloned()
    }

    /// Every open file that holds project rows: the members, and the base file
    /// no write has claimed yet. A closed family has none to read, and says so
    /// rather than answering "no rows".
    pub(crate) fn row_sources(&self) -> Result<Vec<Bucket>> {
        if self.is_closed() {
            return Err(self.closed_error());
        }
        let mut sources: Vec<Bucket> = self.members().iter().map(|m| m.bucket.clone()).collect();
        sources.extend(self.unclaimed.lock().clone());
        Ok(sources)
    }

    /// True once [`BucketFamily::close`] ran.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn closed_error(&self) -> Error {
        Error::NotFound(format!("bucket `{}` was closed", self.name))
    }

    /// Adds an opened member. A tagged member replaces a legacy one of the same
    /// tag — the tagged file is the one adoption could not overwrite, and so the
    /// one to serve.
    pub(crate) fn insert(&self, handle: BucketHandle) {
        let mut members = self.members.write();
        if let Some(existing) = members.get(&handle.tag)
            && !existing.legacy
            && handle.legacy
        {
            return;
        }
        members.insert(handle.tag, Arc::new(handle));
    }

    /// Keeps the base file, open, as claimable by the first write.
    pub(crate) fn set_unclaimed(&self, base: Bucket) {
        *self.unclaimed.lock() = Some(base);
    }

    /// The member for `recipe`, created when the family has none. Writes only:
    /// a read never creates a file, and neither does a closed family.
    ///
    /// A base file that records no recipe is claimed rather than shadowed by a
    /// new file beside it. A new member gets its tag as namespace before its
    /// vector store is built, so two models of one width never share a Qdrant
    /// collection.
    pub async fn ensure_member(
        &self,
        cfg: &StoreConfig,
        recipe: &Recipe,
    ) -> Result<Arc<BucketHandle>> {
        let tag = recipe.tag();
        if let Some(member) = self.member(&tag) {
            return Ok(member);
        }
        let _creating = self.create.lock().await;
        // Checked behind the lock: a close that ran while this one waited
        // must not be followed by a file nobody will ever close.
        if self.is_closed() {
            return Err(self.closed_error());
        }
        if let Some(member) = self.member(&tag) {
            return Ok(member);
        }
        let claimed = self.unclaimed.lock().take();
        let legacy = claimed.is_some();
        let opened = match claimed.clone() {
            Some(base) => self.open_member(cfg, base, recipe, true).await,
            None => self.create_member(cfg, recipe).await,
        };
        let handle = match opened {
            Ok(handle) => handle,
            Err(e) => {
                // A claim that failed half-way leaves the base file claimable.
                if let Some(base) = claimed {
                    self.set_unclaimed(base);
                }
                return Err(e);
            }
        };
        tracing::info!(bucket = %self.name, %recipe, legacy, path = %handle.bucket.path().display(), "member created");
        let handle = Arc::new(handle);
        self.members.write().insert(tag, handle.clone());
        // A file that already recorded another recipe is not re-stamped: it is
        // listed with its conflict, and this write is refused with the repair.
        handle.ensure_serves(recipe)?;
        Ok(handle)
    }

    /// Opens, creating it, the member file `recipe`'s tag names.
    async fn create_member(&self, cfg: &StoreConfig, recipe: &Recipe) -> Result<BucketHandle> {
        let (file, name) = (self.base.member(recipe), self.name.clone());
        let bucket =
            blocking(move || findery_store::open_bucket_file(&file.display().to_string(), &name))
                .await?;
        self.open_member(cfg, bucket, recipe, false).await
    }

    /// Stamps a file for `recipe` and wraps it as a member.
    async fn open_member(
        &self,
        cfg: &StoreConfig,
        bucket: Bucket,
        recipe: &Recipe,
        legacy: bool,
    ) -> Result<BucketHandle> {
        let fresh = bucket.get_meta("embedding_recipe").await?.is_none();
        bucket.claim_recipe(recipe.as_str()).await?;
        if fresh {
            // Only a file that held no vectors yet: one that did keeps the
            // collections it already has.
            bucket.set_vector_namespace(recipe.tag().as_str()).await?;
        }
        let spec = MemberSpec {
            tag: recipe.tag(),
            recipe: recipe.clone(),
            legacy,
        };
        BucketHandle::open(cfg, self, bucket, spec).await
    }

    /// The most recently updated row of `id` across every file of the family.
    pub async fn freshest_row(&self, id: &ProjectId) -> Result<Option<Project>> {
        let mut best: Option<Project> = None;
        for source in self.row_sources()? {
            let Some(row) = source.get_project(id).await? else {
                continue;
            };
            if best
                .as_ref()
                .is_none_or(|kept| row.updated_at > kept.updated_at)
            {
                best = Some(row);
            }
        }
        Ok(best)
    }

    /// Every project id a row exists for, in any file of the family.
    pub async fn project_ids(&self) -> Result<BTreeSet<ProjectId>> {
        let mut ids = BTreeSet::new();
        for source in self.row_sources()? {
            for project in source.list_projects().await? {
                ids.insert(project.id);
            }
        }
        Ok(ids)
    }

    /// Writes `project`'s row into every file of the family, so scopes,
    /// excludes and the pin never diverge between the models of one bucket.
    pub async fn put_row_everywhere(&self, project: &Project) -> Result<()> {
        for source in self.row_sources()? {
            source.put_project(project).await?;
        }
        Ok(())
    }

    /// Closes every file of the family and forgets them, returning their
    /// paths. The OS lock goes with the close, whatever handle clones are
    /// still alive; one file that cannot close leaves none of the others open.
    pub async fn close(&self) -> Result<Vec<String>> {
        self.closed.store(true, Ordering::SeqCst);
        let mut files: Vec<Bucket> = std::mem::take(&mut *self.members.write())
            .into_values()
            .map(|member| member.bucket.clone())
            .collect();
        files.extend(self.unclaimed.lock().take());
        let name = self.name.as_str();
        every(files, |bucket| async move {
            let report = bucket.close().await?;
            if let Some(note) = report.incomplete_checkpoint {
                tracing::warn!(bucket = %name, "{note}");
            }
            Ok(shown(bucket.path()))
        })
        .await
    }

    /// Closes one member and deletes its file set, under the lock that
    /// creates members: a write under that model meanwhile waits, then finds
    /// no file and creates a fresh one. `retry` is what a held file's refusal
    /// tells the user to run again.
    pub(crate) async fn remove_member(
        &self,
        tag: &RecipeTag,
        retry: &str,
    ) -> Result<Option<String>> {
        let _removing = self.create.lock().await;
        let Some(member) = self.members.write().remove(tag) else {
            return Ok(None);
        };
        member.bucket.close().await?;
        let (path, retry) = (member.bucket.path().to_path_buf(), retry.to_string());
        let shown_path = shown(&path);
        blocking(move || findery_store::remove_file_set(&path, &retry)).await?;
        Ok(Some(shown_path))
    }

    /// What `GET /v1/buckets` reports, seen from the member serving `serving`.
    pub async fn info(&self, serving: Option<&Recipe>, is_default: bool) -> Result<BucketInfo> {
        let active = serving.and_then(|r| self.member(&r.tag()));
        let serving_tag = active.as_ref().map(|m| m.tag);
        let mut members = Vec::new();
        for member in self.members() {
            members.push(member.info(serving_tag == Some(member.tag)).await?);
        }
        let chunks = members.iter().filter(|m| m.serving).map(|m| m.chunks).sum();
        let disk_bytes = members.iter().map(|m| m.disk_bytes).sum();
        Ok(BucketInfo {
            name: self.name.clone(),
            path: shown(&self.base.base()),
            recipe: active.as_ref().map(|m| m.stored_recipe.clone()),
            // The payload only: the listing is a field, not an error, so the
            // variant's prefix would be noise in it.
            conflict: active.as_ref().and_then(|m| m.recipe_conflict()),
            projects: self.project_ids().await?.len() as u64,
            chunks,
            disk_bytes,
            is_default,
            members,
        })
    }
}

/// Runs `step` on every item, the failing ones included, then answers the
/// first failure — or every result.
pub(crate) async fn every<I, T, F, Fut>(items: Vec<I>, step: F) -> Result<Vec<T>>
where
    F: Fn(I) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut done = Vec::with_capacity(items.len());
    let mut first_failure = None;
    for item in items {
        match step(item).await {
            Ok(value) => done.push(value),
            Err(e) => {
                tracing::warn!(error = %e, "a step failed; the others still run");
                first_failure.get_or_insert(e);
            }
        }
    }
    match first_failure {
        Some(e) => Err(e),
        None => Ok(done),
    }
}

#[cfg(test)]
mod tests {
    use findery_core::config::StoreBackend;

    use super::*;

    fn store_in(dir: &Path, backend: StoreBackend) -> StoreConfig {
        StoreConfig {
            data_dir: dir.display().to_string(),
            backend,
            ..StoreConfig::default()
        }
    }

    fn family(cfg: &StoreConfig, dir: &Path) -> BucketFamily {
        let base = FamilyPath::from_base(&dir.join("p/.findery/bucket.db")).expect("family");
        BucketFamily::new(cfg, "p", base)
    }

    fn recipe(raw: &str) -> Recipe {
        Recipe::parse(raw).expect("recipe")
    }

    /// A new member names its Qdrant collections after its tag before the store
    /// is built: two models of one width must never write one collection.
    #[tokio::test]
    async fn a_new_member_gets_a_qdrant_namespace_of_its_own() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = store_in(dir.path(), StoreBackend::Qdrant);
        let fam = family(&cfg, dir.path());
        let harrier = recipe("harrier-oss-v1-0.6b:1024");
        let member = fam.ensure_member(&cfg, &harrier).await.expect("created");
        assert_eq!(member.store.backend(), "qdrant");
        assert_eq!(
            member.bucket.vector_namespace().await.expect("read"),
            harrier.tag().to_string()
        );
        assert_eq!(member.bucket.path(), fam.base.member(&harrier));
        assert!(!member.legacy && !member.conflicted());
    }

    /// Two writes under one new model meet one file.
    #[tokio::test]
    async fn concurrent_writes_under_one_model_create_one_member() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = store_in(dir.path(), StoreBackend::Sqlite);
        let fam = Arc::new(family(&cfg, dir.path()));
        let mock = recipe("mock:8");
        let (a, b) = tokio::join!(
            fam.ensure_member(&cfg, &mock),
            fam.ensure_member(&cfg, &mock)
        );
        let (a, b) = (a.expect("a"), b.expect("b"));
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(fam.members().len(), 1);
    }

    /// A base file with no recipe is claimed by the first write, never
    /// shadowed by a second file beside it — and its rows count meanwhile.
    #[tokio::test]
    async fn the_first_write_claims_an_unclaimed_base_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = store_in(dir.path(), StoreBackend::Sqlite);
        let fam = family(&cfg, dir.path());
        let base = fam.base.base();
        fam.set_unclaimed(Bucket::open_file(&base, "p").expect("create"));

        let mock = recipe("mock:8");
        let member = fam.ensure_member(&cfg, &mock).await.expect("claimed");
        assert!(member.legacy);
        assert_eq!(member.bucket.path(), base);
        assert_eq!(member.stored_recipe, "mock:8");
        assert!(!fam.base.member(&mock).exists(), "no second file");
        assert_eq!(
            fam.row_sources().expect("open").len(),
            1,
            "claimed, not counted twice"
        );
    }

    /// The bug this locks: a family closed by a `detach` while a registration
    /// still held it created a file on a family nobody would ever close.
    #[tokio::test]
    async fn a_closed_family_never_creates_a_member() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = store_in(dir.path(), StoreBackend::Sqlite);
        let fam = family(&cfg, dir.path());
        fam.close().await.expect("close");

        let mock = recipe("mock:8");
        let Err(err) = fam.ensure_member(&cfg, &mock).await else {
            panic!("a closed family creates nothing");
        };
        assert_eq!(err.code(), "not_found");
        assert!(!fam.base.member(&mock).exists(), "no file was created");
    }

    /// One file that cannot close must not leave the others open and locked.
    #[tokio::test]
    async fn every_step_runs_even_after_one_fails() {
        let tried = Mutex::new(Vec::new());
        let outcome = every(vec![1, 2, 3], |n| {
            tried.lock().push(n);
            async move {
                if n == 1 {
                    Err(Error::Store("held".into()))
                } else {
                    Ok(n)
                }
            }
        })
        .await;
        assert_eq!(outcome.map_err(|e| e.code()), Err("store"));
        assert_eq!(*tried.lock(), [1, 2, 3]);
    }

    /// `bucket ls` lists every file of a bucket, and marks the one the default
    /// model reads as serving; the family-level counters follow that one.
    #[tokio::test]
    async fn a_listing_names_each_member_and_marks_the_serving_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = store_in(dir.path(), StoreBackend::Sqlite);
        let fam = family(&cfg, dir.path());
        let (a, b) = (recipe("mock:8"), recipe("other:8"));
        fam.ensure_member(&cfg, &a).await.expect("a");
        fam.ensure_member(&cfg, &b).await.expect("b");

        let info = fam.info(Some(&b), false).await.expect("info");
        assert_eq!(info.members.len(), 2);
        let serving: Vec<_> = info.members.iter().filter(|m| m.serving).collect();
        assert_eq!(serving.len(), 1);
        assert_eq!(serving[0].recipe, "other:8");
        assert_eq!(serving[0].tag, b.tag().to_string());
        assert_eq!(info.recipe.as_deref(), Some("other:8"));
        assert!(
            info.members
                .iter()
                .all(|m| !m.legacy && m.conflict.is_none())
        );

        // A model with no file in this bucket serves nothing here.
        let none = fam
            .info(Some(&recipe("third:8")), false)
            .await
            .expect("info");
        assert!(none.members.iter().all(|m| !m.serving));
        assert_eq!(none.recipe, None);
    }

    /// A file that holds another recipe than its name promises is listed with
    /// its conflict, and the write that met it is refused with both repairs.
    #[tokio::test]
    async fn a_member_holding_another_recipe_refuses_to_serve() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = store_in(dir.path(), StoreBackend::Sqlite);
        let fam = family(&cfg, dir.path());
        let named = recipe("mock:8");
        let path = fam.base.member(&named);
        let bucket =
            findery_store::open_bucket_file(&path.display().to_string(), "p").expect("create");
        bucket.claim_recipe("other:8").await.expect("claim");
        bucket.close().await.expect("close");

        let Err(err) = fam.ensure_member(&cfg, &named).await else {
            panic!("a file holding another recipe is refused");
        };
        assert_eq!(err.code(), "recipe_mismatch");
        let text = err.to_string();
        assert!(
            text.contains("other:8") && text.contains("mock:8"),
            "{text}"
        );
        let listed = fam.member(&named.tag()).expect("listed");
        assert!(listed.conflicted() && listed.recipe_conflict().is_some());
    }
}
