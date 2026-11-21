//! `detach` and `rm` of one project: off the server keeping its index, or
//! deleted with it.

use std::path::Path;
use std::sync::Arc;

use findery_core::api::{ProjectDetached, ProjectRemoved};
use findery_core::ids::ProjectId;
use findery_core::model::DEFAULT_BUCKET;
use findery_core::{Error, Result};

use findery_store::FamilyPath;
use findery_store::files::registry::{DetachedEntry, Registry};

use super::quoted;
use crate::Indexer;
use crate::lifecycle::removal;
use crate::storage::blocking::{read_registry, update_registry};
use crate::storage::families::{BucketFamily, shown};

impl Indexer {
    /// Takes a project off the daemon and keeps its index on disk.
    ///
    /// When no attached project is left in its bucket (and the bucket is not
    /// `default`), every member file is closed: unlocked, movable, deletable,
    /// and not reopened at the next start. `findery init` attaches it again,
    /// instantly — the files are simply reopened.
    pub async fn detach(&self, id: &ProjectId) -> Result<ProjectDetached> {
        let name = self.home_of(id)?;
        let family = self.family(&name)?;
        let drained = self.drain(id, "findery detach").await?;
        self.record_detached(id, &family, false).await?;
        self.forget_home(id);
        drained.commit(self);

        let others = self.attached_in(&name);
        let (closed, still_open) = if name == DEFAULT_BUCKET {
            let why = format!("`{DEFAULT_BUCKET}` is the bucket every install has");
            (Vec::new(), Some(why))
        } else if !others.is_empty() {
            (Vec::new(), Some(format!("shared with {}", quoted(&others))))
        } else {
            self.families.write().remove(&name);
            (family.close().await?, None)
        };
        tracing::info!(project = %id, bucket = %name, "detached");
        Ok(ProjectDetached {
            project: id.to_string(),
            closed,
            still_open,
        })
    }

    /// Writes `detached[id]`: where its files stay, and whether a removal of
    /// it is under way.
    async fn record_detached(
        &self,
        id: &ProjectId,
        family: &BucketFamily,
        removing: bool,
    ) -> Result<()> {
        let entry = DetachedEntry {
            family: family.name.clone(),
            base: shown(&family.base.base()),
            removing,
        };
        let key = id.to_string();
        update_registry(&self.cfg.store, move |r| {
            r.detached.insert(key, entry);
        })
        .await
    }

    /// Removes a project and its index — `findery rm`.
    ///
    /// Its rows and vectors go from every member of its bucket. When another
    /// project (attached or detached) has rows there, or it is `default`, that
    /// is all; otherwise every member's file set goes too, the legacy file
    /// included, and the manifest beside them stays. A detached project is
    /// removed the same way.
    pub async fn forget_project(&self, id: &ProjectId) -> Result<ProjectRemoved> {
        let registry = read_registry(&self.cfg.store).await?;
        let attached = self.home_of(id).ok();
        let drained = match &attached {
            Some(_) => Some(self.drain(id, "findery rm").await?),
            None => None,
        };
        let (family, opened_here) = match &attached {
            Some(name) => (self.family(name)?, false),
            None => (self.detached_family(&registry, id).await?, true),
        };
        let name = family.name.clone();

        // Tombstone first: from here on, a removal cut short is finished by
        // the next boot instead of leaving rows or files nobody points at.
        self.record_detached(id, &family, true).await?;
        self.forget_home(id);
        if let Some(drained) = drained {
            drained.commit(self);
        }
        let sharing = removal::remove_rows(&family, &registry, id).await;
        let sharing = match sharing {
            Ok(sharing) => sharing,
            Err(e) if opened_here => {
                family.close().await?;
                return Err(e);
            }
            Err(e) => return Err(e),
        };
        let mut files_deleted = Vec::new();
        if !sharing.rows_only {
            // Out of the map before it closes: no request meets a closed family.
            self.families.write().remove(&name);
            family.close().await?;
            let retry = format!("findery rm -p {id}");
            files_deleted = removal::delete_family_files(&self.cfg.store, &family, &retry).await?;
        } else if opened_here {
            family.close().await?;
        }
        // Registry last: a removal that failed above keeps its tombstone.
        let (key, rows_only) = (id.to_string(), sharing.rows_only);
        update_registry(&self.cfg.store, move |r| {
            r.detached.remove(&key);
            if !rows_only {
                r.buckets.remove(&name);
            }
        })
        .await?;
        tracing::info!(project = %id, bucket = %family.name, rows_only, "removed");
        Ok(ProjectRemoved {
            project: id.to_string(),
            deleted: true,
            files_deleted,
            rows_only,
            shared_with: sharing
                .shared_with
                .iter()
                .map(ToString::to_string)
                .collect(),
        })
    }

    /// The family of a detached project, opened from where `detach` left it.
    async fn detached_family(
        &self,
        registry: &Registry,
        id: &ProjectId,
    ) -> Result<Arc<BucketFamily>> {
        let entry = registry
            .detached_entry(id)
            .ok_or_else(|| Error::NotFound(format!("project `{id}`")))?;
        let base = FamilyPath::from_base(Path::new(&entry.base))?;
        let family =
            removal::open_for_removal(&self.cfg.store, Some(&self.pool), &entry.family, &base)
                .await?;
        Ok(Arc::new(family))
    }
}
