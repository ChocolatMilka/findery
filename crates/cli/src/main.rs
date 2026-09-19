//! `findery` — the command line client.
//!
//! Designed around one habit: `cd` into a project and work. `findery init`
//! registers the current directory; every other command finds the project that
//! contains the current directory, so `-p` is only needed when driving a
//! project from somewhere else.
//!
//! A thin shell over [`findery_client::Client`]. It never touches the catalog, a
//! project's files or the model: whatever it can do, the HTTP API can do, which
//! keeps the CLI and the MCP bridge honest about being peers.

mod doctor;
mod push;
mod render;
mod run;

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
    /// Daemon address.
    #[arg(long, env = "FINDERY_SERVER_URL", default_value = findery_client::DEFAULT_BASE_URL, global = true)]
    pub(crate) server: String,

    /// Bearer token, when the daemon requires one.
    #[arg(long, env = "FINDERY_API_TOKEN", global = true)]
    pub(crate) token: Option<String>,

    /// Print the raw JSON answer instead of the formatted one.
    #[arg(long, global = true)]
    pub(crate) json: bool,

    /// Never start the daemon; fail instead if it is not already running.
    #[arg(long, global = true, env = "FINDERY_NO_SPAWN")]
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
    /// Index the current directory (or PATH) and sync it.
    Init {
        /// Directory to index. Defaults to the current one.
        #[arg(default_value = ".")]
        path: String,
        /// Project id. Derived from the directory name when omitted.
        #[arg(long)]
        id: Option<String>,
        /// Extensions to index, comma-separated. Defaults to `rs,md`.
        #[arg(long, value_delimiter = ',')]
        ext: Option<Vec<String>>,
        /// Register without indexing.
        #[arg(long)]
        no_sync: bool,
        /// Send file contents to the daemon instead of letting it read them.
        /// Chosen automatically when the daemon cannot see the directory.
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
        /// Free-form instruction, overriding --task.
        #[arg(long)]
        instruct: Option<String>,
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
    /// Daemon and model status.
    Health,
    /// Check the whole setup and say what to fix.
    Doctor,
    /// Start, stop or inspect the background daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// List background jobs, or show one.
    Jobs { id: Option<String> },
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

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let server = cli.server.clone();
    match run::dispatch(cli).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("findery: {}", render::explain(&e, &server));
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use findery_core::path_map;

    #[test]
    fn the_deepest_matching_root_wins() {
        // `current_project` reads the real cwd, so the selection predicate is
        // exercised directly with a fixed one.
        let roots = [
            ("outer", "C:/dev"),
            ("inner", "C:/dev/app"),
            ("other", "D:/work"),
        ];
        let cwd = "C:/dev/app/crates/core";
        let mut best: Option<(usize, &str)> = None;
        for (id, root) in roots {
            if path_map::is_within(root, cwd) {
                let depth = path_map::normalize(root).len();
                if best.as_ref().is_none_or(|(b, _)| depth > *b) {
                    best = Some((depth, id));
                }
            }
        }
        assert_eq!(best.map(|(_, id)| id), Some("inner"));
    }

    #[test]
    fn a_sibling_directory_is_not_a_match() {
        assert!(!path_map::is_within("C:/dev/app", "C:/dev/app-old/src"));
        assert!(path_map::is_within("C:/dev/app", "c:/dev/app/src"));
        assert!(path_map::is_within("C:/dev/app", "C:/dev/app"));
    }

    #[test]
    fn a_project_on_another_disk_is_reachable() {
        assert!(path_map::is_within(
            "D:/work/tool",
            "D:/work/tool/src/lib.rs"
        ));
        assert!(!path_map::is_within("C:/dev", "D:/work/tool"));
    }

    #[test]
    fn the_cli_parses_a_cwd_scoped_search() {
        let cli =
            Cli::try_parse_from(["findery", "search", "where", "is", "retry"]).expect("parse");
        match cli.command {
            Command::Search { query, scope, .. } => {
                assert_eq!(query.join(" "), "where is retry");
                assert!(
                    scope.project.is_none(),
                    "no -p means: use the current directory"
                );
            }
            _ => panic!("expected a search"),
        }
    }

    #[test]
    fn an_explicit_project_still_works_from_anywhere() {
        let cli = Cli::try_parse_from(["findery", "search", "-p", "axle", "retry"]).expect("parse");
        match cli.command {
            Command::Search { scope, .. } => assert_eq!(scope.project.as_deref(), Some("axle")),
            _ => panic!("expected a search"),
        }
    }

    #[test]
    fn init_defaults_to_the_current_directory_and_indexes() {
        let cli = Cli::try_parse_from(["findery", "init"]).expect("parse");
        match cli.command {
            Command::Init {
                path, id, no_sync, ..
            } => {
                assert_eq!(path, ".");
                assert!(id.is_none());
                assert!(!no_sync);
            }
            _ => panic!("expected init"),
        }
    }

    #[test]
    fn the_command_surface_stays_parseable() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
