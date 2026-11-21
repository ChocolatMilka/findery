//! Where this invocation talks to, resolved once.
//!
//! INTENT: the address and the token had two reachable homes — the flag and the
//! environment — and the environment is the one that costs a `setx` and a new
//! session on Windows. The settings file the daemon already reads is the third,
//! and it is what `findery config set server …` writes, so naming a daemon is a
//! command rather than an export. Order: flag, environment (clap folds both into
//! [`crate::args::Cli`]), the file, then loopback.
//!
//! Kept apart from `dispatch`, whose one job is routing: naming where a command
//! goes is a different question from deciding what it does.

use findery_core::Result;

use findery_client::DEFAULT_BASE_URL;

use crate::args::Cli;

pub(crate) struct Endpoint {
    pub(crate) base_url: String,
    pub(crate) token: Option<String>,
}

impl Endpoint {
    pub(crate) fn resolve(cli: &Cli) -> Result<Self> {
        Self::over(cli.server.as_deref(), cli.token.as_deref())
    }

    /// The same order, for a caller that has no parsed [`Cli`] — a TAB press,
    /// which reads the flag's two sources, the environment, itself.
    pub(crate) fn from_env() -> Result<Self> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        Self::over(
            var("FINDERY_SERVER_URL").as_deref(),
            var("FINDERY_API_TOKEN").as_deref(),
        )
    }

    /// `server` and `token` win over the settings file, which wins over
    /// loopback.
    fn over(server: Option<&str>, token: Option<&str>) -> Result<Self> {
        let (url, file_token) = crate::settings::connection()?;
        Ok(Self {
            base_url: server
                .map(str::to_string)
                .or(url)
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            token: token.map(str::to_string).or(file_token),
        })
    }
}
