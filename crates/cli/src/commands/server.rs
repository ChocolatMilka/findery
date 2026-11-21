//! `findery health` and the hidden `findery daemon` — whether the server
//! answers, and the lifecycle a service manager drives.

use findery_core::Result;

use findery_client::Client;

use crate::args::DaemonAction;
use crate::ui::render;

/// `findery health`: whether the index answers, and what it serves.
pub(crate) async fn health(client: &Client, json: bool) -> Result<()> {
    let health = client.health().await?;
    if json {
        return render::json(&health);
    }
    render::health(&health);
    Ok(())
}

/// `findery daemon start | stop | status`.
pub(crate) async fn daemon(client: &Client, action: DaemonAction) -> Result<()> {
    let url = client.base_url();
    match action {
        DaemonAction::Start => {
            if findery_client::daemon::ensure_running(client).await? {
                println!("started — {url}");
            } else {
                println!("already running — {url}");
            }
        }
        DaemonAction::Stop => {
            if findery_client::daemon::probe(url).await {
                client.stop_daemon().await?;
                println!("stopping — {url}");
            } else {
                println!("not running — {url}");
            }
        }
        DaemonAction::Status => {
            if findery_client::daemon::probe(url).await {
                render::health(&client.health().await?);
            } else {
                println!("not running — {url}");
            }
        }
    }
    Ok(())
}
