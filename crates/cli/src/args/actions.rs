//! The second word of the commands that take one: `config set`, `bucket rm`,
//! `model use`, `daemon stop`, `doc add`.

use clap::{Subcommand, ValueHint};
use clap_complete::ArgValueCandidates;

use super::Scope;
use super::complete::values;

#[derive(Subcommand)]
pub(crate) enum ConfigAction {
    /// Set one setting
    ///
    /// `model`, `server`, `url`, `key` and `dim` are the short names — `server`
    /// is the server address, `url` the embedding endpoint.
    Set {
        /// A short name such as `model`, or a full path such as `embedder.dim`
        #[arg(value_name = "KEY", add = ArgValueCandidates::new(values::settings))]
        key: String,
        /// The value to write; checked before the file is touched
        #[arg(value_name = "VALUE")]
        value: String,
    },
    /// Return one setting to its default
    Unset {
        /// A short name or a full path, as `set` takes it
        #[arg(value_name = "KEY", add = ArgValueCandidates::new(values::settings))]
        key: String,
    },
    /// List what is set, secrets masked
    Show,
    /// Print the file the settings live in
    Path,
}

#[derive(Subcommand)]
pub(crate) enum BucketAction {
    /// Every bucket the server has open, with what each holds
    Ls,
    /// Create one, or report the one that is already there
    Create {
        /// Letters, digits, `-`, `_` and `.`
        #[arg(value_name = "NAME")]
        name: String,
    },
    // Removal goes through the server, which holds the files open: a file
    // still open cannot be removed at all.
    /// Delete a bucket, its projects and their vectors — or one model's file
    Rm {
        /// The bucket to delete
        #[arg(value_name = "NAME", add = ArgValueCandidates::new(values::buckets))]
        name: String,
        /// Delete without asking; required while it holds projects
        #[arg(long)]
        force: bool,
        /// Delete only this model's file; the projects stay on the others
        #[arg(long, value_name = "ALIAS", add = ArgValueCandidates::new(values::models))]
        model: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum ModelAction {
    /// Every model, the default marked, and the projects using each
    Ls,
    /// One model in detail: its recipe, its file name, why it is down
    Show {
        /// The model to show [default: the default model]
        #[arg(value_name = "ALIAS", add = ArgValueCandidates::new(values::models))]
        alias: Option<String>,
    },
    /// Make a model the default, now and for the next start
    ///
    /// Takes effect at the next command, with no restart. Each model's vectors
    /// live in their own files, so switching back re-embeds nothing.
    Use {
        /// The model to make the default
        #[arg(value_name = "ALIAS", add = ArgValueCandidates::new(values::models))]
        alias: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum DaemonAction {
    /// Start it if it is not already running
    Start,
    /// Ask it to stop
    Stop,
    /// Say whether it answers, and what it is serving
    Status,
}

#[derive(Subcommand)]
pub(crate) enum DocAction {
    /// Index one document, from its file or from standard input
    Add {
        /// Project-relative path the document is indexed under
        #[arg(value_name = "PATH", value_hint = ValueHint::FilePath)]
        path: String,
        #[command(flatten)]
        scope: Scope,
        /// Read the content from standard input instead of the file
        #[arg(long)]
        stdin: bool,
    },
    /// Drop a document from the index
    Rm {
        /// Project-relative path of the document
        #[arg(value_name = "PATH", value_hint = ValueHint::FilePath)]
        path: String,
        #[command(flatten)]
        scope: Scope,
    },
}
