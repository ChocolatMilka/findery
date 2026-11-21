//! `findery config` — set, unset, show, and locate the settings file.
//!
//! The file itself — where it lives, how a key is written, which environment
//! variable shadows it — is `settings`; this module is the command, and the one
//! decision the command adds: a written setting replaces a running server.

use findery_core::Result;

use findery_client::Client;

use crate::args::ConfigAction;
use crate::settings;

pub(crate) async fn run(client: &Client, action: ConfigAction, json: bool) -> Result<()> {
    let written = match action {
        ConfigAction::Set { key, value } => {
            settings::warn_if_shadowed(settings::set(&key, &value, json)?);
            true
        }
        ConfigAction::Unset { key } => {
            settings::warn_if_shadowed(settings::unset(&key, json)?);
            true
        }
        ConfigAction::Show => {
            settings::show(json)?;
            false
        }
        ConfigAction::Path => {
            settings::path(json)?;
            false
        }
    };
    // Editing a local file needs no server, but a server already running read
    // its settings at boot and would keep serving the old ones. Replacing it is
    // plumbing: the next command starts it again, and saying so would put a
    // sentence about a process between the user and their own setting.
    if written && findery_client::daemon::probe(client.base_url()).await {
        client.stop_daemon().await?;
    }
    Ok(())
}
