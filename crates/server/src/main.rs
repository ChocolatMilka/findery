//! `findery` — the daemon.
//!
//! Loads the configuration (file, then `FINDERY_*` environment overlay), connects to
//! the embedding runtime, opens the catalog, and serves the HTTP API until it is
//! asked to stop.

use clap::Parser;
use findery_core::config::Config;
use findery_core::{Error, Result};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "finderyd",
    version,
    about = "findery daemon — indexes codebases and serves the search API"
)]
struct Args {
    /// Configuration file. Missing is fine: the defaults are a working setup.
    #[arg(
        short,
        long,
        env = "FINDERY_CONFIG",
        default_value = "/etc/findery/config.toml"
    )]
    config: PathBuf,

    /// Overrides `server.bind`.
    #[arg(long, env = "FINDERY_BIND")]
    bind: Option<String>,

    /// Parse the configuration, report it, and exit without binding a socket.
    #[arg(long)]
    check: bool,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // Before tracing is up this is the only channel there is, and after
            // it is up a second line costs nothing next to a failed boot.
            eprintln!("findery: {e}");
            tracing::error!(error = %e, "fatal");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let args = Args::parse();
    init_tracing();

    let mut cfg = load_config(&args.config)?;
    cfg.apply_env()?;
    if let Some(bind) = args.bind {
        cfg.server.bind = bind;
    }
    cfg.validate()?;

    if args.check {
        println!(
            "{}",
            toml::to_string_pretty(&cfg).unwrap_or_else(|e| format!("# {e}"))
        );
        return Ok(());
    }

    let bind = cfg.server.bind.clone();
    let authenticated = cfg.server.api_token.is_some();
    tracing::info!(
        model = %cfg.embedder.model,
        endpoint = %cfg.embedder.endpoint(),
        store = cfg.store.backend.as_str(),
        "starting"
    );

    let state = findery_server::state_from_config(cfg).await?;
    let asked_to_stop = state.shutdown.clone();
    let app = findery_server::app(state);

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|e| Error::Config(format!("cannot bind `{bind}`: {e}")))?;
    tracing::info!(%bind, authenticated, "listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(asked_to_stop))
        .await
        .map_err(|e| Error::Internal(format!("server: {e}")))
}

/// Reads the configuration file, tolerating its absence.
///
/// INTENT: `docker run` with nothing but environment variables must work. A
/// missing file is the documented zero-config path; an unreadable or malformed
/// one is an error, because silently falling back would hide a typo forever.
fn load_config(path: &std::path::Path) -> Result<Config> {
    match std::fs::read_to_string(path) {
        Ok(src) => {
            tracing::info!(config = %path.display(), "configuration loaded");
            Config::from_toml_str(&src)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(config = %path.display(), "no configuration file, using defaults");
            Ok(Config::default())
        }
        Err(e) => Err(Error::Config(format!(
            "cannot read `{}`: {e}",
            path.display()
        ))),
    }
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_env("FINDERY_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info,tower_http=warn"));
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
}

/// Resolves on Ctrl-C, on SIGTERM, or on `POST /v1/shutdown`.
///
/// The first two make `docker stop` a clean shutdown instead of a 10-second wait
/// followed by SIGKILL. The third is for the daemon nobody started by hand:
/// there is no terminal to Ctrl-C and no pid anyone wrote down, so
/// `findery daemon stop` asks over the API.
async fn shutdown_signal(asked_to_stop: std::sync::Arc<tokio::sync::Notify>) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let over_the_api = async { asked_to_stop.notified().await };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => tracing::warn!(error = %e, "cannot listen for SIGTERM"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
        _ = over_the_api => {}
    }
    tracing::info!("shutting down");
}
