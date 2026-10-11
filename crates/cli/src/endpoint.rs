//! Where this invocation talks to, resolved once.
//!
//! INTENT: the address and the token had two reachable homes — the flag and the
//! environment — and the environment is the one that costs a `setx` and a new
//! session on Windows. The settings file the daemon already reads is the third,
//! and it is what `findery config set server …` writes, so naming a daemon is a
//! command rather than an export. Order: flag, environment (clap folds both into
//! [`crate::Cli`]), the file, then loopback.
//!
//! Kept apart from `run.rs`, whose one job is dispatch: naming where a command
//! goes is a different question from deciding what it does.

use crate::Cli;
use findery_client::DEFAULT_BASE_URL;
use findery_core::Result;

pub(crate) struct Endpoint {
    pub(crate) base_url: String,
    pub(crate) token: Option<String>,
}

impl Endpoint {
    pub(crate) fn resolve(cli: &Cli) -> Result<Self> {
        let (url, token) = crate::config::connection()?;
        Ok(Self {
            base_url: cli
                .server
                .clone()
                .or(url)
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            token: cli.token.clone().or(token),
        })
    }
}
