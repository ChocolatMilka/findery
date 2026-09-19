//! Which project a command applies to, and the path shapes that answer it.
//!
//! INTENT: split out of `run.rs`, whose own header admitted to two jobs —
//! dispatching a command, and working out which project the caller is standing
//! in. The second is the one with the rules worth reading on their own: the
//! deepest root wins, an auto-derived id may not hijack an existing project,
//! and a Windows verbatim prefix never reaches a message (CONV-029).

use crate::Scope;
use findery_client::Client;
use findery_core::ids::ProjectId;
use findery_core::model::SearchMode;
use findery_core::path_map;
use findery_core::{Error, Result};

/// Resolves the project a command applies to.
pub(crate) async fn resolve(client: &Client, scope: &Scope) -> Result<ProjectId> {
    if let Some(explicit) = &scope.project {
        return ProjectId::new(explicit.clone());
    }
    let projects = client.list_projects().await?;
    current_project(&projects).ok_or_else(|| {
        let cwd = cwd().unwrap_or_else(|_| "the current directory".into());
        Error::Invalid(format!(
            "no indexed project contains `{cwd}`\n  \
             run `findery init` here, or name one with `-p <project>` (`findery projects` lists them)"
        ))
    })
}

/// The registered project whose root contains the current directory.
///
/// The deepest match wins, so a project nested inside another one is picked
/// over its parent.
pub(crate) fn current_project(projects: &[findery_core::model::Project]) -> Option<ProjectId> {
    let cwd = cwd().ok()?;
    let mut best: Option<(usize, ProjectId)> = None;
    for project in projects {
        if !path_map::is_within(&project.host_root, &cwd) {
            continue;
        }
        let depth = path_map::normalize(&project.host_root).len();
        if best.as_ref().is_none_or(|(d, _)| depth > *d) {
            best = Some((depth, project.id.clone()));
        }
    }
    best.map(|(_, id)| id)
}

/// Picks an id for a new project, refusing to hijack an existing one.
pub(crate) async fn derive_id(client: &Client, root: &str) -> Result<ProjectId> {
    let name = path_map::normalize(root)
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("project")
        .to_string();
    let id = ProjectId::slugify(&name)?;

    // An auto-derived id colliding with a different root is almost always an
    // accident: two directories with the same name on two disks.
    if let Ok(existing) = client.project(&id).await
        && !path_map::is_within(&existing.host_root, root)
        && !path_map::is_within(root, &existing.host_root)
    {
        return Err(Error::Invalid(format!(
            "id `{id}` is already used by {}\n  pick another with `findery init --id <name>`",
            existing.host_root
        )));
    }
    Ok(id)
}

/// The current working directory, normalized.
pub(crate) fn cwd() -> Result<String> {
    let dir = std::env::current_dir()
        .map_err(|e| Error::Invalid(format!("cannot read the current directory: {e}")))?;
    Ok(path_map::normalize(&dir.to_string_lossy()))
}

/// Absolute, normalized path with Windows verbatim prefixes stripped.
///
/// `canonicalize` returns `\\?\C:\…` on Windows, which is correct but useless in
/// a message and breaks prefix matching against a user-typed path.
pub(crate) fn absolute(path: &str) -> Result<String> {
    let resolved = std::fs::canonicalize(path)
        .map_err(|e| Error::Invalid(format!("cannot resolve `{path}`: {e}")))?;
    let text = path_map::normalize(&resolved.to_string_lossy());
    Ok(text.strip_prefix("//?/").unwrap_or(&text).to_string())
}

pub(crate) fn parse_mode(raw: &str) -> Option<SearchMode> {
    match raw {
        "hybrid" => Some(SearchMode::Hybrid),
        "vector" => Some(SearchMode::Vector),
        "keyword" => Some(SearchMode::Keyword),
        _ => None,
    }
}
