//! Command dispatch: one arm per command, each a hand-off to `commands`.
//!
//! INTENT: the table of commands reads as a table. A body that grows here is a
//! body that belongs in its command's module, where its own tests live.

use findery_core::Result;

use findery_client::Client;

use crate::args::{Cli, Command};
use crate::commands;
use crate::settings::endpoint::Endpoint;

pub(crate) async fn dispatch(cli: Cli, endpoint: &Endpoint) -> Result<()> {
    let mut client = Client::new(&endpoint.base_url)?;
    if let Some(token) = &endpoint.token {
        client = client.with_token(token.clone());
    }
    let json = cli.json;

    // Everything but the diagnostic and the lifecycle commands needs the server.
    // Starting it here is what removes the "keep a terminal open" step, and it is
    // deliberately silent: a message about a process the user never asked for is
    // a message about plumbing, in the middle of their answer. `findery doctor`
    // is where the plumbing is named, and only when there is something to fix.
    if !matches!(
        cli.command,
        Command::Doctor | Command::Daemon { .. } | Command::Config { .. }
    ) {
        if cli.no_spawn {
            // SAFETY: set before any request, on the single CLI thread.
            unsafe { std::env::set_var("FINDERY_NO_SPAWN", "1") };
        }
        findery_client::daemon::ensure_running(&client).await?;
    }

    match cli.command {
        Command::Init {
            path,
            id,
            ext,
            bucket,
            model,
            sync,
            push,
        } => {
            let options = commands::init::Options {
                path,
                id,
                ext,
                bucket,
                model,
                sync,
                push,
            };
            commands::init::run(&client, options, json).await
        }
        Command::Search(args) => commands::search::run(&client, *args, json).await,
        Command::Sync {
            scope,
            full,
            paths,
            background,
        } => {
            let options = commands::sync::Options {
                full,
                paths,
                background,
            };
            commands::sync::run(&client, &scope, options, json).await
        }
        Command::Status { scope } => commands::projects::status(&client, &scope, json).await,
        Command::Projects => commands::projects::list(&client, json).await,
        Command::Bucket { action } => commands::buckets::run(&client, action, json).await,
        Command::Model { action } => commands::models::run(&client, action, json).await,
        Command::Detach { scope } => commands::lifecycle::detach(&client, &scope, json).await,
        Command::Rm { scope } => commands::lifecycle::rm(&client, &scope, json).await,
        Command::Doc { action } => commands::projects::doc(&client, action, json).await,
        Command::Export { scope, format, dir } => {
            commands::projects::export(&client, &scope, format, dir, json).await
        }
        Command::Watch {
            scope,
            off,
            debounce_ms,
        } => commands::sync::watch(&client, &scope, off, debounce_ms).await,
        Command::Health => commands::server::health(&client, json).await,
        Command::Doctor => commands::doctor::run(&client, json).await,
        Command::Config { action } => commands::config::run(&client, action, json).await,
        Command::Daemon { action } => commands::server::daemon(&client, action).await,
        Command::Jobs { id } => commands::sync::jobs(&client, id.as_deref(), json).await,
    }
}
