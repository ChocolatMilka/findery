//! `findery sync`, `findery watch` and `findery jobs` — keeping an index up to
//! date, and following the work that does it.
//!
//! A project is fed one of two ways, chosen at registration by testing the
//! path: the server walks what it can see and runs a job the CLI follows
//! ([`follow`]), or the CLI walks and sends the contents itself ([`push`]).
//! Nobody has to remember which mode a project is in.

pub(crate) mod follow;
pub(crate) mod push;

use std::time::Duration;

use findery_core::Result;
use findery_core::api::{SyncRequest, SyncResponse, WatchRequest};

use findery_client::Client;

use crate::args::Scope;
use crate::ui::board::Board;
use crate::ui::render;
use crate::workspace::scope::resolve;

/// What `findery sync` was asked for, past the project.
pub(crate) struct Options {
    pub(crate) full: bool,
    pub(crate) paths: Vec<String>,
    pub(crate) background: bool,
}

/// `findery sync`.
pub(crate) async fn run(
    client: &Client,
    scope: &Scope,
    options: Options,
    json: bool,
) -> Result<()> {
    let id = resolve(client, scope).await?;
    let project = client.project(&id).await?;
    if project.spec.push {
        let board = if json { None } else { Board::new() };
        let report = push::sync(client, &project, board.as_ref()).await?;
        return if json {
            render::json(&report)
        } else {
            render::sync(&report);
            Ok(())
        };
    }
    // `--background` is a promise to return immediately, so it never gets a
    // bar: watching a job is the opposite of not waiting for it.
    let board = if json || options.background {
        None
    } else {
        Board::new()
    };
    let request = SyncRequest {
        mode: options.full.then(|| "full".to_string()),
        paths: (!options.paths.is_empty()).then_some(options.paths),
        // The server runs a job and the CLI follows it; see
        // `follow::watch_sync` on why it never asks the server to hold one
        // request open for the length of the run.
        wait: false,
        wait_ms: None,
    };
    let follow = !options.background;
    match follow::watch_sync(client, &id, &request, board, follow).await? {
        SyncResponse::Completed { report } if json => render::json(&report),
        SyncResponse::Completed { report } => {
            render::sync(&report);
            Ok(())
        }
        SyncResponse::Accepted { job } if json => render::json(&job),
        SyncResponse::Accepted { job } => {
            println!(
                "job {} queued; follow it with `findery jobs {}`",
                job.id, job.id
            );
            Ok(())
        }
    }
}

/// `findery watch`: the server's watcher, or — for a pushed project — this
/// process, blocking, as the watcher.
pub(crate) async fn watch(
    client: &Client,
    scope: &Scope,
    off: bool,
    debounce_ms: u64,
) -> Result<()> {
    let id = resolve(client, scope).await?;
    let project = client.project(&id).await?;
    if project.spec.push {
        if off {
            println!("nothing to stop: this project is watched by this command itself");
            return Ok(());
        }
        let debounce = Duration::from_millis(debounce_ms.clamp(100, 60_000));
        return push::watch(client, &project, debounce).await;
    }
    let request = WatchRequest {
        enabled: !off,
        debounce_ms,
    };
    let state = client.set_watch(&id, &request).await?;
    println!(
        "{}: watching {}",
        state.project,
        if state.watching { "on" } else { "off" }
    );
    Ok(())
}

/// `findery jobs [id]`: every job, or one in full.
pub(crate) async fn jobs(client: &Client, id: Option<&str>, json: bool) -> Result<()> {
    if let Some(id) = id {
        return render::json(&client.job(id).await?);
    }
    let list = client.jobs().await?;
    if json {
        return render::json(&list);
    }
    render::jobs(&list);
    Ok(())
}
