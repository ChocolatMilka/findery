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
            warn_if_shadowed(settings::set(&key, &value, json)?);
            true
        }
        ConfigAction::Unset { key } => {
            warn_if_shadowed(settings::unset(&key, json)?);
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

/// Says when a written setting will not take effect.
///
/// INTENT: the environment overlay wins over the file on purpose — one image
/// has to serve every deployment — but that makes a successful `config set`
/// appear to do nothing at all. Naming the variable at the moment of the write
/// turns a trap back into a rule.
fn warn_if_shadowed(variable: Option<&'static str>) {
    let Some(variable) = variable else {
        return;
    };
    eprintln!(
        "warning: {variable} is set in this environment and overrides what was just \
         written.\n  Unset it, or the file will keep being ignored."
    );
}
