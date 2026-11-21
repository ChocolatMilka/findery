//! The argument surface: what clap parses, and nothing else.
//!
//! A `///` line here is user-facing text — clap prints it. The bodies live in
//! `commands`, reached through `dispatch`.

mod actions;
#[cfg(test)]
mod tests;

pub(crate) use actions::{BucketAction, ConfigAction, DaemonAction, DocAction};

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "findery",
    version,
    about = "Index and search your codebases with a local semantic index",
    after_help = "Most commands take no project argument: the one containing the current \
                  directory is used. Start with `findery init` inside a repository."
)]
pub(crate) struct Cli {
    /// Server address. Defaults to `server.url` in the settings file, then to a
    /// local server started on demand.
    #[arg(long, env = "FINDERY_SERVER_URL", global = true)]
    pub(crate) server: Option<String>,

    /// Bearer token, when the server requires one. Defaults to `server.api_token`
    /// in the settings file.
    #[arg(long, env = "FINDERY_API_TOKEN", global = true)]
    pub(crate) token: Option<String>,

    /// Print the raw JSON answer instead of the formatted one.
    #[arg(long, global = true)]
    pub(crate) json: bool,

    /// Never start the server; fail instead if it is not already running.
    ///
    /// INTENT: `BoolishValueParser` is here for the environment variable, not
    /// for the flag. Everything that documents this setting — the compose file,
    /// the page you are reading — writes `FINDERY_NO_SPAWN=1`, and clap's
    /// default parser accepts only `true` and `false`. A documented spelling
    /// that is rejected at startup is worse than no spelling.
    #[arg(
        long,
        global = true,
        env = "FINDERY_NO_SPAWN",
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    pub(crate) no_spawn: bool,

    #[command(subcommand)]
    pub(crate) command: Command,
}

/// Selects a project: explicit id, or the one containing the current directory.
#[derive(clap::Args, Clone, Default)]
pub(crate) struct Scope {
    /// Project id. Defaults to the project containing the current directory.
    #[arg(short, long, global = true)]
    pub(crate) project: Option<String>,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Register the current directory (or PATH) as a project.
    ///
    /// Indexing is a separate step, so a repository can be registered — and its
    /// bucket chosen — before a model is reachable.
    Init {
        /// Directory to index. Defaults to the current one.
        #[arg(default_value = ".")]
        path: String,
        /// Project id. Derived from the directory name when omitted.
        #[arg(long)]
        id: Option<String>,
        /// Extensions to index, comma-separated. Defaults to every
        /// language the chunkers know:
        /// `rs,md,toml,js,mjs,cjs,ts,py,php,json,axle,java,kt,kts,go`.
        #[arg(long, value_delimiter = ',')]
        ext: Option<Vec<String>>,
        /// Bucket to hold this project, by name. Defaults to a bucket inside
        /// the project, named after it.
        ///
        /// Chosen once: every later command follows the project, so nobody has
        /// to remember which index a directory belongs to.
        #[arg(long)]
        bucket: Option<String>,
        /// Index it now instead of waiting for `findery sync`.
        #[arg(long)]
        sync: bool,
        /// Send file contents instead of letting the server read them.
        /// Chosen automatically when the server cannot see the directory.
        #[arg(long)]
        push: bool,
    },
    /// Search the index.
    Search {
        /// Query words; joined with spaces.
        #[arg(required = true)]
        query: Vec<String>,
        #[command(flatten)]
        scope: Scope,
        #[arg(short, long, default_value_t = 8)]
        k: usize,
        #[arg(long, value_parser = ["hybrid", "vector", "keyword"])]
        mode: Option<String>,
        /// Retrieval intent sent to the instruction-aware model.
        #[arg(long, value_parser = ["code", "symbol", "usage", "docs", "test", "config", "error"])]
        task: Option<String>,
        /// Free-form instruction for the embedding model, overriding --task.
        ///
        /// The first half of what the model reads as "Instruct: …\nQuery: …";
        /// the query is the other half, and both are always sent.
        #[arg(long, alias = "instruct")]
        instruction: Option<String>,
        /// A named restriction the project declares, narrowing this same index.
        ///
        /// Declared in `.findery/config.toml`, applied before scoring — never a
        /// second index, never a cut applied after the ranking.
        #[arg(long = "scope")]
        search_scope: Option<String>,
        #[arg(long = "path")]
        paths: Vec<String>,
        #[arg(long = "lang")]
        langs: Vec<String>,
        #[arg(long = "kind")]
        kinds: Vec<String>,
        /// Per-excerpt character budget.
        #[arg(long)]
        max_chars: Option<usize>,
    },
    /// Synchronize a project with the filesystem.
    Sync {
        #[command(flatten)]
        scope: Scope,
        /// Rebuild every file, ignoring the metadata and hash gates.
        #[arg(long)]
        full: bool,
        /// Limit the sync to these project-relative paths.
        #[arg(long = "path")]
        paths: Vec<String>,
        /// Return immediately with a job id.
        #[arg(long)]
        background: bool,
    },
    /// File, chunk and vector counts.
    Status {
        #[command(flatten)]
        scope: Scope,
    },
    /// List indexed projects.
    Projects,
    /// Manage buckets: independent indexes under one server.
    Bucket {
        #[command(subcommand)]
        action: BucketAction,
    },
    /// Remove a project, its chunks and its vectors.
    Rm {
        #[command(flatten)]
        scope: Scope,
    },
    /// Add or drop a single document.
    Doc {
        #[command(subcommand)]
        action: DocAction,
    },
    /// Export vectors and metadata.
    Export {
        #[command(flatten)]
        scope: Scope,
        #[arg(long, default_value = "jsonl", value_parser = ["jsonl", "json", "binary"])]
        format: String,
        #[arg(long)]
        dir: Option<String>,
    },
    /// Turn filesystem watching on or off.
    Watch {
        #[command(flatten)]
        scope: Scope,
        #[arg(long)]
        off: bool,
        #[arg(long, default_value_t = 1500)]
        debounce_ms: u64,
    },
    /// What the index answers, and what it serves.
    Health,
    /// Check the whole setup and say what to fix.
    Doctor,
    /// Read and write the settings, without touching your shell.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Start, stop or inspect the background server.
    ///
    /// Hidden from `--help`: it starts itself, it stops itself, and the only
    /// reader of this command is a service manager or someone debugging one.
    /// Showing it invites the user to manage a process they never asked for.
    #[command(hide = true)]
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// List background jobs, or show one.
    Jobs { id: Option<String> },
}
