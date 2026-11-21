//! Finishing, at boot, the removals a previous run started and could not end.
//!
//! INTENT: `findery rm` and `findery bucket rm` write a tombstone into
//! `buckets.toml` **before** their first destructive step and clear it **after**
//! their last. A removal cut short — a crash, a file another process held — is
//! therefore never half-forgotten: the next boot finds the tombstone and
//! finishes the job before anything opens the files, so nothing is orphaned
//! and nothing comes back. A removal that still cannot finish is reported in
//! `health`, and what it removes is not opened: serving a bucket the user
//! asked to delete would be worse than serving nothing.
//!
//! The steps are `removal`'s, the same the online verbs take.

use std::collections::HashSet;
use std::path::Path;

use findery_core::config::{Config, bucket_path};
use findery_core::ids::ProjectId;
use findery_core::{Error, Result};

use findery_store::FamilyPath;
use findery_store::files::registry::{DetachedEntry, Registry};

use crate::lifecycle::removal;
use crate::storage::blocking::update_registry;
use crate::storage::families::shown;

/// What a pending removal still holds: never opened until it finishes.
#[derive(Default)]
pub(crate) struct Unfinished {
    pub families: HashSet<String>,
}

/// Finishes every pending removal. Returns what must not be opened and one
/// notice per removal that could not finish.
pub(crate) async fn complete(cfg: &Config, registry: &Registry) -> (Unfinished, Vec<String>) {
    let mut unfinished = Unfinished::default();
    let mut notices = Vec::new();
    for name in &registry.removing_families {
        if let Err(refused) = findery_store::ensure_removable(name) {
            notices.push(drop_refused_tombstone(cfg, name, &refused).await);
            continue;
        }
        if let Err(e) = finish_bucket_removal(cfg, registry, name).await {
            unfinished.families.insert(name.clone());
            notices.push(format!(
                "the removal of bucket `{name}` is unfinished: {}; it is not served until the \
                 next start finishes it",
                e.detail()
            ));
        }
    }
    for (id, entry) in &registry.detached {
        if !entry.removing || unfinished.families.contains(&entry.family) {
            continue;
        }
        let Ok(project) = ProjectId::new(id.clone()) else {
            continue;
        };
        if let Err(e) = finish_project_removal(cfg, registry, &project, entry).await {
            unfinished.families.insert(entry.family.clone());
            notices.push(format!(
                "the removal of project `{id}` is unfinished: {}; bucket `{}` is not served \
                 until the next start finishes it",
                e.detail(),
                entry.family
            ));
        }
    }
    // A member still listed after this is skipped by the opener, which reads
    // the same tombstones.
    for path in &registry.removing_members {
        if let Err(e) = finish_member_removal(cfg, path).await {
            notices.push(format!(
                "the removal of {path} is unfinished: {}; it is not served until the next start \
                 finishes it",
                e.detail()
            ));
        }
    }
    (unfinished, notices)
}

/// A tombstone for a bucket that may not go — `default`, by a hand edit — is
/// dropped and the bucket kept. Returns the notice that says so.
async fn drop_refused_tombstone(cfg: &Config, name: &str, refused: &Error) -> String {
    let owned = name.to_string();
    let dropped = update_registry(&cfg.store, move |r| r.removing_families.remove(&owned)).await;
    let kept = match dropped {
        Ok(_) => "the tombstone was dropped".to_string(),
        Err(e) => format!("the tombstone could not be dropped ({})", e.detail()),
    };
    format!(
        "a removal of bucket `{name}` was recorded and refused ({}); the bucket is kept and {kept}",
        refused.detail()
    )
}

/// Finishes a `findery bucket rm`: every project's vectors, then the files.
async fn finish_bucket_removal(cfg: &Config, registry: &Registry, name: &str) -> Result<()> {
    let file = registry
        .file_of(name)
        .map(str::to_string)
        .unwrap_or_else(|| bucket_path(&cfg.store.data_dir, name));
    let base = FamilyPath::from_base(Path::new(&file))?;
    let family = removal::open_for_removal(&cfg.store, None, name, &base).await?;
    let dropped = removal::drop_every_vector(&family).await;
    family.close().await?;
    dropped?;
    removal::delete_family_files(&cfg.store, &family, &format!("findery bucket rm {name}")).await?;
    let name = name.to_string();
    update_registry(&cfg.store, move |r| {
        r.buckets.remove(&name);
        r.removing_families.remove(&name);
        r.detached.retain(|_, entry| entry.family != name);
    })
    .await
}

/// Finishes a `findery rm`: the project's rows go from every file, and the
/// files themselves when nothing else lives in them.
async fn finish_project_removal(
    cfg: &Config,
    registry: &Registry,
    project: &ProjectId,
    entry: &DetachedEntry,
) -> Result<()> {
    let base = FamilyPath::from_base(Path::new(&entry.base))?;
    let family = removal::open_for_removal(&cfg.store, None, &entry.family, &base).await?;
    let sharing = removal::remove_rows(&family, registry, project).await;
    family.close().await?;
    let rows_only = sharing?.rows_only;
    if !rows_only {
        let retry = format!("findery rm -p {project}");
        removal::delete_family_files(&cfg.store, &family, &retry).await?;
    }
    let (id, name) = (project.to_string(), entry.family.clone());
    update_registry(&cfg.store, move |r| {
        r.detached.remove(&id);
        if !rows_only {
            r.buckets.remove(&name);
        }
    })
    .await
}

/// Finishes a `findery bucket rm --model`: that member's vectors, then its
/// file set.
async fn finish_member_removal(cfg: &Config, path: &str) -> Result<()> {
    let base = FamilyPath::from_base(Path::new(path))?;
    let family = removal::open_for_removal(&cfg.store, None, base.stem(), &base).await?;
    let member = family
        .members()
        .into_iter()
        .find(|m| shown(m.bucket.path()) == path);
    let removed = match member {
        Some(member) => remove_one_member(&family, &member, path).await,
        None => Ok(()),
    };
    family.close().await?;
    removed?;
    let path = path.to_string();
    update_registry(&cfg.store, move |r| r.removing_members.remove(&path)).await?;
    Ok(())
}

async fn remove_one_member(
    family: &crate::BucketFamily,
    member: &crate::BucketHandle,
    path: &str,
) -> Result<()> {
    removal::drop_member_vectors(member).await?;
    let retry = format!("findery bucket rm {} --model …", family.name);
    family
        .remove_member(&member.tag, &retry)
        .await?
        .ok_or_else(|| Error::Internal(format!("{path} vanished from its family")))?;
    Ok(())
}
