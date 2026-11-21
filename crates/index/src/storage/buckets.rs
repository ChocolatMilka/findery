//! The family registry: which buckets the daemon has open, which one each
//! project calls home, and what each reports.
//!
//! INTENT: a bucket is its files, so the daemon's map of them is rebuilt from
//! the filesystem and then maintained only by the daemon's own writes. One
//! writer means the map cannot drift from the disk, and nothing reads a
//! project's bucket out of a column — a project's home IS the family holding
//! its row.
//!
//! Entry/exit: [`Indexer::new`] fills the map; every later mutation goes
//! through [`Indexer::register`], [`Indexer::ensure_bucket_at`] and the
//! lifecycle verbs (`detach`, `forget_project`, `forget_bucket`). A bad bucket
//! name, or a name or path that conflicts with an open bucket, is caller input,
//! so it is refused as `Invalid`.

use std::path::Path;
use std::sync::Arc;

use findery_core::api::BucketInfo;
use findery_core::config::bucket_path;
use findery_core::ids::ProjectId;
use findery_core::model::{DEFAULT_BUCKET, is_bucket_name};
use findery_core::{Error, Result};

use findery_store::FamilyPath;

use crate::Indexer;
use crate::storage::blocking::{read_registry, update_registry};
use crate::storage::boot::{self, Opener};
use crate::storage::families::{BucketFamily, shown};

impl Indexer {
    /// Every open family, default first then names in order.
    pub fn families(&self) -> Vec<Arc<BucketFamily>> {
        let mut out: Vec<_> = self.families.read().values().cloned().collect();
        out.sort_by_key(|f| (f.name != DEFAULT_BUCKET, f.name.clone()));
        out
    }

    /// One open family by name.
    pub fn family(&self, name: &str) -> Result<Arc<BucketFamily>> {
        self.families
            .read()
            .get(name)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("bucket `{name}`")))
    }

    /// The family an attached project lives in.
    pub fn home_of(&self, id: &ProjectId) -> Result<String> {
        self.homes
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("project `{id}`")))
    }

    /// The attached projects living in `family`.
    pub(crate) fn attached_in(&self, family: &str) -> Vec<ProjectId> {
        let mut ids: Vec<ProjectId> = self
            .homes
            .read()
            .iter()
            .filter(|(_, home)| home.as_str() == family)
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }

    /// Opens a family where its name says, or reports the one already open.
    pub async fn ensure_bucket(&self, name: &str) -> Result<Arc<BucketFamily>> {
        self.ensure_bucket_at(name, None).await
    }

    /// Opens the family at `at`, or where its name says when `at` is `None`:
    /// every member file already there is opened, and none is created — the
    /// first write under a model creates that model's file.
    ///
    /// Opening here rather than behind a separate command is what lets
    /// `findery init --bucket work` be the whole gesture: naming a bucket you
    /// have never used is a decision, not an error.
    pub async fn ensure_bucket_at(
        &self,
        name: &str,
        at: Option<&str>,
    ) -> Result<Arc<BucketFamily>> {
        if !is_bucket_name(name) {
            return Err(Error::Invalid(format!("`{name}` is not a bucket name")));
        }
        let file = match at {
            Some(path) => path.to_string(),
            None => bucket_path(&self.cfg.store.data_dir, name),
        };
        let base = FamilyPath::from_base(Path::new(&file))?;
        // Bound before the `if`: the guard must not live across an `await`.
        let existing = self.families.read().get(name).cloned();
        if let Some(existing) = existing {
            // Refused, not moved under the feet of whoever reads through it.
            if at.is_some() && existing.base != base {
                return Err(Error::Invalid(format!(
                    "bucket `{name}` is already open at {}; remove it first with \
                     `findery bucket rm {name}` to move it to {file}",
                    existing.base.base().display()
                )));
            }
            return Ok(existing);
        }
        self.refuse_second_name(name, &base)?;
        let registry = read_registry(&self.cfg.store).await?;
        if registry.is_removing(name) {
            // A removal the last start could not finish: serving these files
            // again would hand back a bucket the user asked to delete.
            return Err(Error::Busy(format!(
                "the removal of bucket `{name}` is unfinished; nothing was changed — restart the \
                 daemon to finish it, then use the name again"
            )));
        }

        let scan = boot::scan_async(&self.cfg.store, name, &base).await?;
        let opener = Opener {
            cfg: &self.cfg.store,
            pool: Some(&self.pool),
            removing: &registry.removing_members,
        };
        let (family, broken) = opener.open(scan).await;
        for reason in broken {
            self.report_broken(reason);
        }
        // Remembered only when the path is not the one the name derives: the
        // registry exists for the locations nothing else could find.
        if !family.at_derived_path {
            let (key, file) = (name.to_string(), shown(&base.base()));
            update_registry(&self.cfg.store, move |r| {
                r.buckets.insert(key, file);
            })
            .await?;
        }

        // Opening is an `await`, so two registrations can both find the name
        // absent and both open the files. The first to reach the map wins; the
        // second closes what it opened rather than replacing what others read.
        let winner = {
            let mut open = self.families.write();
            match open.get(name) {
                Some(winner) => Some(winner.clone()),
                None => {
                    let family = Arc::new(family);
                    open.insert(name.to_string(), family.clone());
                    return Ok(family);
                }
            }
        };
        family.close().await?;
        winner.ok_or_else(|| Error::Internal(format!("bucket `{name}` lost its winner")))
    }

    /// One file is one bucket: a second name over a family's files would be two
    /// writers, and a `bucket rm` unlinking the files the other still serves.
    fn refuse_second_name(&self, name: &str, base: &FamilyPath) -> Result<()> {
        let holder = self
            .families
            .read()
            .iter()
            .find(|(other, family)| other.as_str() != name && family.base == *base)
            .map(|(other, _)| other.clone());
        match holder {
            Some(holder) => Err(Error::Invalid(format!(
                "bucket `{name}` points at {}, which is already open as `{holder}`; one file is \
                 one bucket",
                base.base().display()
            ))),
            None => Ok(()),
        }
    }

    /// Remembers which family holds a project.
    pub(crate) fn remember_home(&self, id: &ProjectId, family: &str) {
        self.homes.write().insert(id.clone(), family.to_string());
    }

    /// Forgets a project's home.
    pub(crate) fn forget_home(&self, id: &ProjectId) {
        self.homes.write().remove(id);
    }

    /// Keeps a reason a bucket could not be fully opened, for `health`: a
    /// member that failed to open is a project's index silently missing.
    pub(crate) fn report_broken(&self, reason: String) {
        tracing::warn!("{reason}");
        let mut broken = self.broken.lock();
        if !broken.contains(&reason) {
            broken.push(reason);
        }
    }

    /// What the daemon serves, in the shape the API reports it: each family,
    /// seen from the member of the default model.
    pub async fn list_buckets(&self) -> Result<Vec<BucketInfo>> {
        let serving = self.default_recipe().ok();
        let mut out = Vec::new();
        for family in self.families() {
            let is_default = family.name == self.cfg.store.default_bucket;
            out.push(family.info(serving.as_ref(), is_default).await?);
        }
        Ok(out)
    }

    /// Creates a bucket — its member of the default model — or reports the one
    /// that is already there.
    pub async fn create_bucket(&self, name: &str) -> Result<BucketInfo> {
        let family = self.ensure_bucket(name).await?;
        let recipe = self.pool.resolve_recipe(&self.pool.default_alias()).await?;
        family.ensure_member(&self.cfg.store, &recipe).await?;
        family
            .info(Some(&recipe), name == self.cfg.store.default_bucket)
            .await
    }
}
