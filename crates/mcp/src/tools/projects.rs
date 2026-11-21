//! `projects`: list them, look at one, register one, list its intents.

use std::path::{Path, PathBuf};

use findery_core::Result;
use findery_core::api::RegisterProject;
use findery_core::manifest::{MANIFEST_DIR, MANIFEST_FILE};

use findery_client::resolve::read_manifest;

use crate::Session;
use crate::args::Args;
use crate::catalogue::PROJECT_ACTIONS;
use crate::render::projects::{self as view, Listed, Status};

#[derive(Clone, Copy)]
enum Action {
    List,
    Status,
    Register,
    Intents,
}

impl Action {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "list" => Some(Self::List),
            "status" => Some(Self::Status),
            "register" => Some(Self::Register),
            "intents" => Some(Self::Intents),
            _ => None,
        }
    }
}

pub(super) async fn run(session: &Session, args: &Args<'_>) -> Result<String> {
    let action = args
        .choice("action", Action::parse, PROJECT_ACTIONS)?
        .unwrap_or(Action::List);
    match action {
        Action::List => list(session).await,
        Action::Status => status(session, args).await,
        Action::Register => register(session, args).await,
        Action::Intents => intents(session, args).await,
    }
}

async fn list(session: &Session) -> Result<String> {
    let client = session.client();
    let mut listed = Vec::new();
    for project in client.list_projects().await? {
        // A project whose counters cannot be read is still listed: the
        // listing is how an agent finds out which ids exist at all.
        let stats = client.stats(&project.id).await.ok();
        listed.push(Listed { project, stats });
    }
    let here = session.detected().await;
    Ok(view::list(&listed, here.as_ref()))
}

async fn status(session: &Session, args: &Args<'_>) -> Result<String> {
    let client = session.client();
    let id = session.project(args).await?;
    let project = client.project(&id).await?;
    let stats = client.stats(&id).await?;
    // Health probes the runtimes; without it the status is still worth having.
    let health = client.health().await.ok();
    let jobs = client.jobs().await.unwrap_or_default();
    let model = health.as_ref().and_then(|h| {
        let alias = project.model.as_ref()?;
        h.models.iter().find(|m| &m.profile.alias == alias)
    });
    let notices = health
        .as_ref()
        .map(|h| {
            let tag = format!("`{id}`");
            h.notices
                .iter()
                .filter(|n| n.contains(&tag))
                .map(String::as_str)
                .collect()
        })
        .unwrap_or_default();
    let running = jobs
        .iter()
        .find(|job| job.project == id.as_str() && !job.state.is_terminal());
    Ok(view::status(&Status {
        project: &project,
        stats: &stats,
        model,
        running,
        notices,
    }))
}

/// Registers `root`, carrying its manifest when it has one: a cloned
/// repository arrives with its scopes, intents and pin, and the daemon — which
/// merges — keeps whatever this request leaves out.
async fn register(session: &Session, args: &Args<'_>) -> Result<String> {
    let root = absolute(session.cwd(), &args.required("root")?);
    let extensions = args.texts("extensions")?;
    let mut request = RegisterProject {
        id: args.text("id")?,
        name: args.text("name")?,
        extensions: Some(extensions).filter(|e| !e.is_empty()),
        ..RegisterProject::new(root.display().to_string().replace('\\', "/"))
    };
    let manifest = root.join(MANIFEST_DIR).join(MANIFEST_FILE);
    if manifest.is_file() {
        let config = read_manifest(&manifest)?;
        request.id = request
            .id
            .or_else(|| config.project_id().map(|id| id.to_string()));
        request.bucket = Some(config.bucket).filter(|b| !b.is_unspecified());
        request.scopes = Some(config.scopes);
        request.instruction = Some(config.instruction.unwrap_or_default());
        request.task = config.task;
        request.intents = Some(config.intents);
        request.model = Some(config.model.unwrap_or_default());
    }
    let project = session.client().register(&request).await?;
    Ok(view::registered(&project))
}

async fn intents(session: &Session, args: &Args<'_>) -> Result<String> {
    let id = session.project(args).await?;
    let list = session.client().intents(Some(&id)).await?;
    Ok(view::intents(&id, &list))
}

/// `raw` as an absolute path, relative ones read from the bridge's directory.
fn absolute(cwd: &Path, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    match path.is_absolute() {
        true => path.to_path_buf(),
        false => cwd.join(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_root_is_read_from_the_bridge_directory() {
        let cwd = std::env::temp_dir();
        assert_eq!(absolute(&cwd, "repo"), cwd.join("repo"));
        assert_eq!(absolute(&cwd, cwd.to_str().expect("utf8")), cwd);
        assert!(Action::parse("purge").is_none());
    }
}
