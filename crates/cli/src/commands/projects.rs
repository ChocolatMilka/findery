//! `findery status`, `findery projects`, `findery rm`, `findery doc` and
//! `findery export` — what the server knows about a project, its removal, and
//! the single documents and exports that go in and out of its index.

use std::io::Read;

use findery_core::api::{AddDocument, ExportRequest};
use findery_core::{Error, Result};

use findery_client::Client;

use crate::args::{DocAction, Scope};
use crate::ui::render;
use crate::workspace::scope::{current_project, resolve};

/// `findery status`: the counters of one project.
pub(crate) async fn status(client: &Client, scope: &Scope, json: bool) -> Result<()> {
    let stats = client.stats(&resolve(client, scope).await?).await?;
    if json {
        return render::json(&stats);
    }
    render::stats(&stats);
    Ok(())
}

/// `findery projects`: every registered project, the current one marked.
pub(crate) async fn list(client: &Client, json: bool) -> Result<()> {
    let list = client.list_projects().await?;
    if json {
        return render::json(&list);
    }
    let here = current_project(&list).map(|id| id.to_string());
    render::projects(&list, here.as_deref());
    Ok(())
}

/// `findery rm`: the project, off the server.
pub(crate) async fn rm(client: &Client, scope: &Scope) -> Result<()> {
    let id = resolve(client, scope).await?;
    if client.delete_project(&id).await? {
        println!("removed {id}");
    } else {
        println!("nothing to remove");
    }
    Ok(())
}

/// `findery doc add` / `findery doc rm`.
pub(crate) async fn doc(client: &Client, action: DocAction, json: bool) -> Result<()> {
    match action {
        DocAction::Add { path, scope, stdin } => {
            let id = resolve(client, &scope).await?;
            let text = if stdin { Some(read_stdin()?) } else { None };
            let report = client
                .add_document(&id, &AddDocument { path, text })
                .await?;
            if json {
                return render::json(&report);
            }
            render::sync(&report);
        }
        DocAction::Rm { path, scope } => {
            let id = resolve(client, &scope).await?;
            let removed = client.remove_document(&id, &path).await?;
            println!("removed {removed} chunk(s) for {path}");
        }
    }
    Ok(())
}

fn read_stdin() -> Result<String> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| Error::Invalid(format!("cannot read stdin: {e}")))?;
    Ok(buf)
}

/// `findery export`: a project's vectors and metadata, written by the server.
pub(crate) async fn export(
    client: &Client,
    scope: &Scope,
    format: String,
    dir: Option<String>,
    json: bool,
) -> Result<()> {
    let id = resolve(client, scope).await?;
    let request = ExportRequest {
        format: Some(format),
        dir,
    };
    let report = client.export(&id, &request).await?;
    if json {
        return render::json(&report);
    }
    render::export(&report);
    Ok(())
}
