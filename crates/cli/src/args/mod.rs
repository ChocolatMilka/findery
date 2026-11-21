//! The argument surface: what clap parses, and nothing else.
//!
//! A `///` line here is user-facing text — clap prints it — so its first
//! paragraph is the one-line summary `-h` shows, the rest is what `--help`
//! adds, and every reason a field is shaped the way it is goes in a `//`
//! comment. The bodies live in `commands`, reached through `dispatch`; how
//! the surface is grouped, styled and illustrated lives in [`help`], what TAB
//! offers in [`complete`].

mod actions;
pub(crate) mod complete;
pub(crate) mod help;
#[cfg(test)]
mod tests;
pub(crate) mod vocab;

pub(crate) use actions::{BucketAction, ConfigAction, DaemonAction, DocAction, ModelAction};

use clap::{Parser, Subcommand, ValueHint};
use clap_complete::ArgValueCandidates;

use complete::values;

/// The heading every subcommand lists the global options under, below its own.
const GLOBAL: &str = "Global options";

#[derive(Parser)]
#[command(
    name = "findery",
    bin_name = "findery",
    version,
    about = "Index and search your codebases with a local semantic index"
)]
pub(crate) struct Cli {
    /// Server address [default: the settings, else local]
    ///
    /// `server.url` in the settings file, else a local server started on demand.
    #[arg(
        long,
        env = "FINDERY_SERVER_URL",
        global = true,
        value_name = "URL",
        value_hint = ValueHint::Url,
        help_heading = GLOBAL
    )]
    pub(crate) server: Option<String>,

    /// Bearer token [default: `server.api_token`]
    ///
    /// Only for a server that requires one.
    // `hide_env_values`: the help must never print a secret that happens to be
    // exported in the shell reading it.
    #[arg(
        long,
        env = "FINDERY_API_TOKEN",
        global = true,
        value_name = "TOKEN",
        hide_env_values = true,
        help_heading = GLOBAL
    )]
    pub(crate) token: Option<String>,

    /// Print the raw JSON answer, for a script
    #[arg(long, global = true, help_heading = GLOBAL)]
    pub(crate) json: bool,

    /// Never start a server; fail if none is running
    // `BoolishValueParser` is here for the environment variable, not for the
    // flag. Everything that documents this setting — the compose file, the
    // usage page — writes `FINDERY_NO_SPAWN=1`, and clap's default parser
    // accepts only `true` and `false`. A documented spelling that is rejected
    // at startup is worse than no spelling.
    #[arg(
        long,
        global = true,
        env = "FINDERY_NO_SPAWN",
        value_parser = clap::builder::BoolishValueParser::new(),
        help_heading = GLOBAL
    )]
    pub(crate) no_spawn: bool,

    #[command(subcommand)]
    pub(crate) command: Command,
}

/// Selects a project: explicit id, or the one containing the current directory.
#[derive(clap::Args, Clone, Default)]
pub(crate) struct Scope {
    /// Project id [default: the project containing the current directory]
    #[arg(
        short,
        long,
        global = true,
        value_name = "PROJECT",
        add = ArgValueCandidates::new(values::projects)
    )]
    pub(crate) project: Option<String>,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Register a directory as a project, the current one by default
    ///
    /// Indexing is a separate step — `findery sync`, or `--sync` here — so a
    /// repository can be registered, and its bucket chosen, before a model is
    /// reachable.
    Init {
        /// Directory to register
        #[arg(default_value = ".", value_name = "PATH", value_hint = ValueHint::DirPath)]
        path: String,
        /// Project id [default: derived from the directory name]
        #[arg(long, value_name = "ID")]
        id: Option<String>,
        /// Extensions to index, comma-separated [default: every known language]
        ///
        /// The default list: `rs,md,toml,js,mjs,cjs,ts,py,php,json,axle,java,kt,kts,go`.
        #[arg(long, value_name = "EXT,…", value_delimiter = ',')]
        ext: Option<Vec<String>>,
        /// Bucket to hold the project [default: one inside it, named after it]
        // Chosen once: every later command follows the project, so nobody has
        // to remember which index a directory belongs to.
        #[arg(long, value_name = "NAME", add = ArgValueCandidates::new(values::buckets))]
        bucket: Option<String>,
        /// Model to pin the project to [default: follow the default model]
        #[arg(long, value_name = "ALIAS", add = ArgValueCandidates::new(values::models))]
        model: Option<String>,
        /// Index it now instead of waiting for `findery sync`
        #[arg(long)]
        sync: bool,
        /// Send file contents instead of letting the server read them
        ///
        /// Chosen automatically when the server cannot see the directory.
        #[arg(long)]
        push: bool,
    },
    /// Search the current project: by meaning, by exact words, or both
    Search(Box<crate::commands::search::SearchArgs>),
    /// List the intents a search can name in this project
    Intents {
        #[command(flatten)]
        scope: Scope,
    },
    /// Bring the index up to date with the files
    Sync {
        #[command(flatten)]
        scope: Scope,
        /// Re-read every file; vectors already computed are still reused
        #[arg(long)]
        full: bool,
        /// Only these project-relative paths; repeatable
        #[arg(long = "path", value_name = "PATH", value_hint = ValueHint::AnyPath)]
        paths: Vec<String>,
        /// Return at once with a job id instead of following the run
        #[arg(long)]
        background: bool,
    },
    /// File, chunk and vector counts for a project
    Status {
        #[command(flatten)]
        scope: Scope,
    },
    /// List the registered projects, the current one marked
    Projects,
    /// Buckets: independent indexes under one server
    Bucket {
        #[command(subcommand)]
        action: BucketAction,
    },
    /// Embedding models: list them, inspect one, switch the default
    Model {
        #[command(subcommand)]
        action: ModelAction,
    },
    /// Take a project off the server, keeping its index on disk, unlocked
    ///
    /// `findery init` in the project attaches it again, instantly.
    Detach {
        #[command(flatten)]
        scope: Scope,
    },
    /// Delete a project and its index — its bucket files too, unless shared
    ///
    /// The manifest (`.findery/config.toml`) is kept.
    Rm {
        #[command(flatten)]
        scope: Scope,
    },
    /// Add or drop a single document
    Doc {
        #[command(subcommand)]
        action: DocAction,
    },
    /// Export a project's vectors and metadata
    Export {
        #[command(flatten)]
        scope: Scope,
        /// File format to write
        #[arg(long, default_value = "jsonl", value_parser = ["jsonl", "json", "binary"])]
        format: String,
        /// Directory to write into [default: chosen by the server]
        #[arg(long, value_name = "DIR", value_hint = ValueHint::DirPath)]
        dir: Option<String>,
    },
    /// Re-sync on every file change, or stop doing so
    Watch {
        #[command(flatten)]
        scope: Scope,
        /// Stop watching
        #[arg(long)]
        off: bool,
        /// Quiet time after a change before re-syncing, in milliseconds
        #[arg(long, value_name = "MS", default_value_t = 1500)]
        debounce_ms: u64,
    },
    /// Whether the index answers, and what it serves
    Health,
    /// Check the whole setup and say what to fix
    Doctor,
    /// Read and write the settings, without touching your shell
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Shell completion: print the script, or how to load it
    Completions(complete::CompletionsArgs),
    // Hidden from `--help`: it starts itself, it stops itself, and the only
    // reader of this command is a service manager or someone debugging one.
    // Showing it invites the user to manage a process they never asked for.
    /// Start, stop or inspect the background server
    #[command(hide = true)]
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// List background jobs, or show one
    Jobs {
        /// A job id, as `sync --background` prints it [default: every job]
        #[arg(value_name = "JOB")]
        id: Option<String>,
    },
}
