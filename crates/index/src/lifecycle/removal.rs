//! The steps every removal takes — online (`rm`, `bucket rm`) and at boot,
//! finishing a tombstone a previous run left.
//!
//! INTENT: one module, so the two paths cannot disagree about which bucket is
//! shared, what a vector store's failure costs, or which files go. The rules:
//!
//! - a bucket is **shared** when another project has a row in its files or is
//!   detached into it, or when it is `default` — read from the files, never
//!   from the daemon's map of homes, which misses a row it could not read;
//! - vectors go **before** rows, and rows before files: a row with no vector
//!   is recoverable, an orphaned collection is not;
//! - a vector store that fails stops the removal, and the tombstone stays: a
//!   collection left behind would be inherited by the next project of the same
//!   id and model.

use std::collections::BTreeSet;

use findery_core::Result;
use findery_core::config::StoreConfig;
use findery_core::ids::ProjectId;
use findery_core::model::DEFAULT_BUCKET;

use findery_store::FamilyPath;
use findery_store::files::registry::Registry;

use crate::models::EmbedderPool;
use crate::storage::blocking::blocking;
use crate::storage::boot::{self, Opener};
use crate::storage::families::{BucketFamily, BucketHandle, shown};

/// Who else lives in a family a project was taken out of.
pub(crate) struct Sharing {
    /// True when the files stay: another project lives in them, or they are
    /// `default`'s.
    pub rows_only: bool,
    pub shared_with: Vec<ProjectId>,
}

/// Opens a family the daemon does not serve — its projects all detached, or
/// a tombstone's — for a removal only. A member that cannot be opened stops
/// it: deleting files whose vectors could not be reached orphans them.
pub(crate) async fn open_for_removal(
    cfg: &StoreConfig,
    pool: Option<&EmbedderPool>,
    name: &str,
    base: &FamilyPath,
) -> Result<BucketFamily> {
    let scan = boot::scan_async(cfg, name, base).await?;
    let removing = BTreeSet::new();
    let opener = Opener {
        cfg,
        pool,
        removing: &removing,
    };
    let (family, broken) = opener.open(scan).await;
    if let Some(reason) = broken.into_iter().next() {
        family.close().await?;
        return Err(findery_core::Error::Store(reason));
    }
    Ok(family)
}

/// Takes `id`'s vectors and rows out of every file of an open family, and
/// says whether the files themselves may go — they may when nothing else
/// lives in them. Who else lives there is read before anything is dropped.
pub(crate) async fn remove_rows(
    family: &BucketFamily,
    registry: &Registry,
    id: &ProjectId,
) -> Result<Sharing> {
    let shared_with = sharers(family, registry, id).await?;
    drop_rows_everywhere(family, std::slice::from_ref(id)).await?;
    Ok(Sharing {
        rows_only: !shared_with.is_empty() || family.name == DEFAULT_BUCKET,
        shared_with,
    })
}

/// The projects other than `id` with a row in the family's files, or
/// detached into it.
async fn sharers(
    family: &BucketFamily,
    registry: &Registry,
    id: &ProjectId,
) -> Result<Vec<ProjectId>> {
    let mut others = family.project_ids().await?;
    let detached_here = registry
        .detached
        .iter()
        .filter(|(_, entry)| entry.family == family.name)
        .filter_map(|(other, _)| ProjectId::new(other.clone()).ok());
    others.extend(detached_here);
    others.remove(id);
    Ok(others.into_iter().collect())
}

/// Drops `ids`' vectors from every member, then their rows from every file.
pub(crate) async fn drop_rows_everywhere(family: &BucketFamily, ids: &[ProjectId]) -> Result<()> {
    for member in family.members() {
        drop_vectors(&member, ids).await?;
    }
    for source in family.row_sources()? {
        for id in ids {
            source.delete_project(id).await?;
        }
    }
    Ok(())
}

/// Drops the vectors of every project in the family, before its files go:
/// a Qdrant collection outlives the file that names it. Returns the projects.
pub(crate) async fn drop_every_vector(family: &BucketFamily) -> Result<Vec<ProjectId>> {
    let ids: Vec<ProjectId> = family.project_ids().await?.into_iter().collect();
    for member in family.members() {
        drop_vectors(&member, &ids).await?;
    }
    Ok(ids)
}

/// Drops the vectors of every project with a row in one member.
pub(crate) async fn drop_member_vectors(member: &BucketHandle) -> Result<()> {
    let ids: Vec<ProjectId> = member
        .bucket
        .list_projects()
        .await?
        .into_iter()
        .map(|p| p.id)
        .collect();
    drop_vectors(member, &ids).await
}

async fn drop_vectors(member: &BucketHandle, ids: &[ProjectId]) -> Result<()> {
    for id in ids {
        member.store.drop_project(id).await?;
    }
    Ok(())
}

/// Deletes the files of a closed family and returns the databases that were
/// there: its whole directory when the family is where its name puts it,
/// otherwise each member's file set — the directory around it is the user's.
pub(crate) async fn delete_family_files(
    cfg: &StoreConfig,
    family: &BucketFamily,
    retry: &str,
) -> Result<Vec<String>> {
    let (cfg, base, name, retry) = (
        cfg.clone(),
        family.base.clone(),
        family.name.clone(),
        retry.to_string(),
    );
    let derived = family.at_derived_path && name != DEFAULT_BUCKET;
    blocking(move || {
        let members = findery_store::scan(&base)?;
        let deleted: Vec<String> = members.iter().map(|m| shown(&m.path)).collect();
        if derived {
            findery_store::delete_bucket(&cfg, &name)?;
            return Ok(deleted);
        }
        for member in &members {
            findery_store::remove_file_set(&member.path, &retry)?;
        }
        Ok(deleted)
    })
    .await
}
