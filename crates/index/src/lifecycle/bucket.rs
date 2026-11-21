//! `bucket rm`: a whole bucket, or one model's member file of it.

use std::path::Path;
use std::sync::Arc;

use findery_core::api::BucketRemoved;
use findery_core::config::bucket_path;
use findery_core::ids::ProjectId;
use findery_core::{Error, Result};

use findery_store::FamilyPath;
use findery_store::files::registry::Registry;

use crate::Indexer;
use crate::lifecycle::removal;
use crate::storage::blocking::{read_registry, update_registry};
use crate::storage::families::{BucketFamily, BucketHandle, shown};

impl Indexer {
    /// Deletes a bucket, its projects and their vectors — or, with `model`,
    /// only that model's member file.
    ///
    /// INTENT: it goes through the daemon rather than through the CLI's own
    /// filesystem call, because the daemon is the only process that knows every
    /// handle on the files — and on Windows a file still open cannot be removed
    /// at all. Nothing here needs an embedder: this is the door a recipe
    /// conflict used to lock.
    pub async fn forget_bucket(&self, name: &str, model: Option<&str>) -> Result<BucketRemoved> {
        // Before the first destructive step, not after the last: a refusal that
        // arrives once vectors are dropped already cost the user their index.
        // One model's file is removable from any bucket, `default` included.
        if model.is_none() {
            findery_store::ensure_removable(name)?;
        }
        let registry = read_registry(&self.cfg.store).await?;
        let (family, opened_here) = self.family_for_removal(name, &registry).await?;
        let removed = match model {
            Some(model) => self.forget_member(&family, model).await,
            None => self.forget_whole_bucket(&family).await,
        };
        // A family opened for this removal goes back to closed, whatever the
        // removal did; a second close is a no-op.
        let closed = if opened_here {
            family.close().await.map(drop)
        } else {
            Ok(())
        };
        let removed = removed?;
        closed?;
        Ok(removed)
    }

    /// The family `bucket rm` works on: the one the daemon serves, else — its
    /// projects all detached — the one on disk, opened for the removal only.
    async fn family_for_removal(
        &self,
        name: &str,
        registry: &Registry,
    ) -> Result<(Arc<BucketFamily>, bool)> {
        if let Ok(family) = self.family(name) {
            return Ok((family, false));
        }
        let base = FamilyPath::from_base(Path::new(&recorded_base(
            &self.cfg.store.data_dir,
            name,
            registry,
        )))?;
        let listed = {
            let base = base.clone();
            crate::storage::blocking::blocking(move || findery_store::scan(&base)).await?
        };
        if listed.is_empty() {
            return Err(Error::NotFound(format!("bucket `{name}`")));
        }
        let family =
            removal::open_for_removal(&self.cfg.store, Some(&self.pool), name, &base).await?;
        Ok((Arc::new(family), true))
    }

    /// `bucket rm <name>`: every project drained, the tombstone written, the
    /// vectors dropped, the files deleted, the registry entry last.
    async fn forget_whole_bucket(&self, family: &BucketFamily) -> Result<BucketRemoved> {
        let name = family.name.clone();
        let by = format!("findery bucket rm {name}");
        let attached = self.attached_in(&name);
        let drained = self.drain_all(&attached, &by).await?;
        let tombstone = name.clone();
        update_registry(&self.cfg.store, move |r| {
            r.removing_families.insert(tombstone);
        })
        .await?;
        for id in &attached {
            self.forget_home(id);
        }
        for drained in drained {
            drained.commit(self);
        }
        // Out of the map first, so no new request reaches it; then closed,
        // which releases the OS lock for every clone at once.
        self.families.write().remove(&name);
        let projects = removal::drop_every_vector(family).await?;
        family.close().await?;
        let deleted = removal::delete_family_files(&self.cfg.store, family, &by).await?;
        // Last: a delete that failed above leaves the tombstone and the entry,
        // so the next boot finishes it instead of orphaning the files.
        let gone = name.clone();
        update_registry(&self.cfg.store, move |r| {
            r.buckets.remove(&gone);
            r.removing_families.remove(&gone);
            r.detached.retain(|_, entry| entry.family != gone);
        })
        .await?;
        tracing::info!(bucket = %name, "removed");
        Ok(BucketRemoved {
            removed: !deleted.is_empty(),
            projects_removed: projects.len() as u64,
            projects: projects.iter().map(ToString::to_string).collect(),
        })
    }

    /// `bucket rm <name> --model <m>`: one member file, found by the model
    /// alias, its tag or its recipe. The bucket's last member is refused: that
    /// is the bucket itself.
    ///
    /// Only the projects whose model writes this member are drained, and the
    /// drains are released, not committed: the projects stay, on their other
    /// members, and a sync under another model never notices.
    async fn forget_member(&self, family: &BucketFamily, model: &str) -> Result<BucketRemoved> {
        let member = self.member_named(family, model)?;
        if family.members().len() == 1 {
            return Err(Error::Invalid(format!(
                "`{model}` is the last model of bucket `{}`; remove the bucket itself with \
                 `findery bucket rm {}`",
                family.name, family.name
            )));
        }
        let by = format!("findery bucket rm {} --model {model}", family.name);
        let writers = self.writers_of(family, &member).await?;
        let _drained = self.drain_all(&writers, &by).await?;
        let path = shown(member.bucket.path());
        let tombstone = path.clone();
        update_registry(&self.cfg.store, move |r| {
            r.removing_members.insert(tombstone);
        })
        .await?;
        removal::drop_member_vectors(&member).await?;
        family.remove_member(&member.tag, &by).await?;
        update_registry(&self.cfg.store, move |r| r.removing_members.remove(&path)).await?;
        Ok(BucketRemoved {
            removed: true,
            projects_removed: 0,
            projects: Vec::new(),
        })
    }

    /// The member `model` names: a profile's alias, else a member's tag or
    /// recipe — what still names the file of a profile no longer configured.
    fn member_named(&self, family: &BucketFamily, model: &str) -> Result<Arc<BucketHandle>> {
        let profile = self.pool.known(model);
        let by_profile = profile
            .as_ref()
            .ok()
            .and_then(|alias| self.pool.recipe_of(alias).ok())
            .and_then(|recipe| family.member(&recipe.tag()));
        let by_name = || {
            family
                .members()
                .into_iter()
                .find(|m| m.tag.as_str() == model || m.recipe.as_str() == model)
        };
        if let Some(member) = by_profile.or_else(by_name) {
            return Ok(member);
        }
        // Neither a profile nor a file: most likely a typo, answered with the
        // names that exist.
        profile?;
        let files: Vec<String> = family
            .members()
            .iter()
            .map(|m| format!("{} ({})", m.tag, m.recipe))
            .collect();
        Err(Error::NotFound(format!(
            "bucket `{}` has no file for `{model}`; its files are {}",
            family.name,
            files.join(", ")
        )))
    }

    /// The attached projects of `family` whose model writes `member` — or
    /// whose model's vector space is not known without its runtime, which
    /// might be it.
    async fn writers_of(
        &self,
        family: &BucketFamily,
        member: &BucketHandle,
    ) -> Result<Vec<ProjectId>> {
        let mut writers = Vec::new();
        for id in self.attached_in(&family.name) {
            let Some(row) = family.freshest_row(&id).await? else {
                continue;
            };
            let recipe = self
                .pool
                .resolve(row.spec.model.as_ref())
                .and_then(|alias| self.pool.recipe_of(&alias));
            if recipe.is_err() || recipe.is_ok_and(|r| r.tag() == member.tag) {
                writers.push(id);
            }
        }
        Ok(writers)
    }
}

/// Where a bucket's files are when the daemon does not serve it: the registry
/// entry, else the base a detached project recorded, else where its name puts
/// it.
fn recorded_base(data_dir: &str, name: &str, registry: &Registry) -> String {
    let detached = || {
        registry
            .detached
            .values()
            .find(|entry| entry.family == name)
            .map(|entry| entry.base.clone())
    };
    registry
        .file_of(name)
        .map(str::to_string)
        .or_else(detached)
        .unwrap_or_else(|| bucket_path(data_dir, name))
}
