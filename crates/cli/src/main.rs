//! `findery` — the command line client.
//!
//! Designed around one habit: `cd` into a project and work. `findery init`
//! registers the current directory; every other command finds the project that
//! contains the current directory, so `-p` is only needed when driving a
//! project from somewhere else.
//!
//! A thin shell over [`findery_client::Client`]. It never touches the bucket, a
//! project's files or the model: whatever it can do, the HTTP API can do, which
//! keeps the CLI and the MCP bridge honest about being peers.

mod config;
mod doctor;
mod endpoint;
mod init;
mod manifest;
mod progress;
mod push;
mod render;
mod run;
mod scope;

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
        /// language the chunkers know: `rs,md,toml,js,mjs,cjs,ts,py,php,json`.
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

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let endpoint = match endpoint::Endpoint::resolve(&cli) {
        Ok(endpoint) => endpoint,
        Err(e) => {
            eprintln!("findery: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match run::dispatch(cli, &endpoint).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("findery: {}", render::explain(&e, &endpoint.base_url));
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
    fn init_registers_without_indexing_unless_asked() {
        let cli = Cli::try_parse_from(["findery", "init"]).expect("parse");
        match cli.command {
            Command::Init {
                path,
                id,
                bucket,
                sync,
                ..
            } => {
                assert_eq!(path, ".");
                assert!(id.is_none());
                assert!(bucket.is_none(), "the configured default applies");
                assert!(!sync, "indexing is a separate, deliberate step");
            }
            _ => panic!("expected init"),
        }
    }

    #[test]
    fn init_can_choose_a_bucket_and_index_in_one_go() {
        let cli =
            Cli::try_parse_from(["findery", "init", "--bucket", "work", "--sync"]).expect("parse");
        match cli.command {
            Command::Init { bucket, sync, .. } => {
                assert_eq!(bucket.as_deref(), Some("work"));
                assert!(sync);
            }
            _ => panic!("expected init"),
        }
    }

    #[test]
    fn the_bucket_command_names_what_it_acts_on() {
        let cli = Cli::try_parse_from(["findery", "bucket", "create", "work"]).expect("parse");
        match cli.command {
            Command::Bucket {
                action: BucketAction::Create { name },
            } => assert_eq!(name, "work"),
            _ => panic!("expected bucket create"),
        }
        let cli = Cli::try_parse_from(["findery", "bucket", "rm", "work"]).expect("parse");
        match cli.command {
            Command::Bucket {
                action: BucketAction::Rm { name, force },
            } => {
                assert_eq!(name, "work");
                assert!(!force);
            }
            _ => panic!("expected bucket rm"),
        }
    }

    #[test]
    fn no_daemon_address_is_baked_into_the_argument_surface() {
        // The file has to be able to speak below the flag and the environment,
        // so clap must not fill the field with a default of its own.
        let cli = Cli::try_parse_from(["findery", "projects"]).expect("parse");
        assert!(cli.server.is_none());
        assert!(cli.token.is_none());
    }

    #[test]
    fn the_command_surface_stays_parseable() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    /// The spelling every document uses has to be the spelling that works.
    /// `FINDERY_NO_SPAWN=1` is written in the compose file and in three pages;
    /// clap's default parser accepts only `true` and `false`, which made a
    /// documented setting fail at startup.
    #[test]
    fn the_no_spawn_variable_accepts_the_spelling_we_document() {
        // SAFETY: the variable is removed before the assertion, and no other
        // test in this binary reads it.
        unsafe { std::env::set_var("FINDERY_NO_SPAWN", "1") };
        let parsed = Cli::try_parse_from(["findery", "status"]);
        unsafe { std::env::remove_var("FINDERY_NO_SPAWN") };

        assert!(
            parsed
                .expect("`FINDERY_NO_SPAWN=1` is the documented spelling")
                .no_spawn,
            "the documented value must set the flag"
        );
    }

    /// The server is plumbing: it starts itself, so offering to manage it is
    /// offering to manage something the user never asked for.
    #[test]
    fn the_server_lifecycle_is_reachable_but_not_advertised() {
        use clap::CommandFactory;
        let surface = Cli::command();
        let daemon = surface
            .get_subcommands()
            .find(|c| c.get_name() == "daemon")
            .expect("the escape hatch still exists for a service manager");
        assert!(daemon.is_hide_set(), "`daemon` must stay out of --help");
    }

    /// Nothing visible may mention the server by its process name: a command
    /// that narrates its own plumbing teaches the concept it was meant to hide.
    ///
    /// Walks the whole surface rather than a list, because the failure mode is a
    /// command added later with a careless `///` line — and a list would let it
    /// through.
    #[test]
    fn nothing_visible_ever_names_the_daemon() {
        use clap::CommandFactory;
        let surface = Cli::command();
        for command in surface.get_subcommands() {
            if command.is_hide_set() {
                continue;
            }
            // `render_long_help` covers the summary, the flag descriptions and
            // the subcommands in one page — the whole surface a user can read.
            let name = command.get_name().to_string();
            let text = command.clone().render_long_help().to_string();
            assert!(
                !text.to_lowercase().contains("daemon"),
                "`{name}` mentions the daemon:\n{text}"
            );
        }
    }
}
