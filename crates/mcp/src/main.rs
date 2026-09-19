//! `findery-mcp` — the MCP stdio bridge for Claude Code.
//!
//! Configuration is entirely environmental, because that is all an MCP client
//! gives a server:
//!
//! * `FINDERY_SERVER_URL` — daemon address (default `http://127.0.0.1:8077`)
//! * `FINDERY_API_TOKEN`  — bearer token, when the daemon requires one
//! * `FINDERY_LOG`        — log filter; logs go to stderr, never stdout
//! * `FINDERY_NO_SPAWN`   — set to forbid starting the daemon automatically
//!
//! stdout carries the protocol and nothing else. A stray `println!` here
//! corrupts the JSON-RPC stream and the client drops the session, which is why
//! tracing is pinned to stderr below.

use findery_client::Client;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    init_tracing();

    let client = match Client::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("findery-mcp: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    // Claude Code starts this bridge, not the user, so there is nobody around to
    // start a daemon by hand. A failure here is logged and not fatal: exiting
    // would show up as "server failed to connect" with no explanation, whereas
    // staying up lets every tool call answer with the real reason.
    match findery_client::daemon::ensure_running(&client).await {
        Ok(true) => tracing::info!(daemon = client.base_url(), "started the daemon"),
        Ok(false) => {}
        Err(e) => tracing::warn!(error = %e, "no daemon; tool calls will fail until there is one"),
    }
    tracing::info!(daemon = client.base_url(), "mcp bridge ready");

    match findery_mcp::serve(client, tokio::io::stdin(), tokio::io::stdout()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "stdio loop ended");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Logs to stderr. stdout belongs to the protocol.
fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_env("FINDERY_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}
