//! Which project a command applies to, and the path shapes that answer it.
//!
//! INTENT: the resolution rule itself — `-p`, then the manifest walked
//! upward, then the deepest registered root — lives in
//! [`findery_client::resolve`], because the MCP bridge must answer the same way
//! from the same directory. What stays here is the CLI's side of it: its flag,
//! its repair sentence, and the id `init` derives without hijacking another
//! project (CONV-029).

use findery_core::ids::ProjectId;
use findery_core::model::SearchMode;
use findery_core::support::path_map;
use findery_core::{Error, Result};

use findery_client::Client;

use crate::args::Scope;

/// Resolves the project a command applies to.
///
/// Three answers, narrowest first: the `-p` flag, the manifest at or above the
/// current directory, and finally the daemon, asked which of its projects
/// contains this directory. The manifest comes before the daemon because it is
/// the only one of the two that answers the same way offline, from a
/// subdirectory, and in a repository somebody else cloned.
pub(crate) async fn resolve(client: &Client, scope: &Scope) -> Result<ProjectId> {
    let dir = std::env::current_dir()
        .map_err(|e| Error::Invalid(format!("cannot read the current directory: {e}")))?;
    client
        .resolve_project(scope.project.as_deref(), &dir, REPAIR)
        .await
}

/// What the CLI tells a caller standing in no project.
const REPAIR: &str =
    "run `findery init` here, or name one with `-p <project>` (`findery projects` lists them)";

/// The registered project whose root contains the current directory, the
/// deepest one first.
pub(crate) fn current_project(projects: &[findery_core::model::Project]) -> Option<ProjectId> {
    findery_client::resolve::deepest_root(projects, &cwd().ok()?)
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
    SearchMode::parse(raw)
}
