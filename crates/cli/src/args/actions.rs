//! The second word of the commands that take one: `config set`, `bucket rm`,
//! `daemon stop`, `doc add`.

use clap::Subcommand;

use super::Scope;

#[derive(Subcommand)]
pub(crate) enum ConfigAction {
    /// Set one setting.
    ///
    /// `model`, `server`, `url`, `key` and `dim` are the short names — `server`
    /// is the server address, `url` the embedding endpoint.
    Set {
        /// `model`, or a full path such as `embedder.query_instruction`.
        key: String,
        value: String,
    },
    /// Return one setting to its default.
    Unset { key: String },
    /// List what is set, secrets masked.
    Show,
    /// Print the file the settings live in.
    Path,
}

#[derive(Subcommand)]
pub(crate) enum BucketAction {
    /// Every bucket the server has open, with what each holds.
    Ls,
    /// Create one, or report the one that is already there.
    Create {
        /// Letters, digits, `-`, `_` and `.`.
        name: String,
    },
    /// Delete a bucket, its projects and their vectors.
    ///
    /// Removal goes through the server, which holds the file open: a bucket
    /// bucket still open cannot be removed at all.
    Rm {
        name: String,
        /// Delete without asking. Required when it holds projects.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum DaemonAction {
    /// Start it if it is not already running.
    Start,
    /// Ask it to stop.
    Stop,
    /// Say whether it answers, and what it is serving.
    Status,
}

#[derive(Subcommand)]
pub(crate) enum DocAction {
    /// Index a document. With `--stdin`, the content is read from standard input.
    Add {
        path: String,
        #[command(flatten)]
        scope: Scope,
        #[arg(long)]
        stdin: bool,
    },
    /// Drop a document from the index.
    Rm {
        path: String,
        #[command(flatten)]
        scope: Scope,
    },
}
