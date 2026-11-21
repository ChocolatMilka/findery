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
//!
//! The crate is laid out by what each part is for:
//!
//! | Module | Holds |
//! |---|---|
//! | `args` | the argument surface clap parses, and nothing else |
//! | `dispatch` | one arm per command, each handing off to `commands` |
//! | `commands` | one module per command or family of commands |
//! | `settings` | the settings file the CLI shares with the server, and the address resolved from it |
//! | `workspace` | where the CLI stands: the project around the current directory, its manifest |
//! | `ui` | everything a terminal shows: palette, renderers, the live sync board |

mod args;
mod commands;
mod dispatch;
mod settings;
mod ui;
mod workspace;

use clap::Parser;

use args::Cli;

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("findery: cannot start the async runtime: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    runtime.block_on(run(cli))
}

async fn run(cli: Cli) -> std::process::ExitCode {
    let endpoint = match settings::endpoint::Endpoint::resolve(&cli) {
        Ok(endpoint) => endpoint,
        Err(e) => {
            eprintln!("findery: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match dispatch::dispatch(cli, &endpoint).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("findery: {}", ui::render::explain(&e, &endpoint.base_url));
            std::process::ExitCode::FAILURE
        }
    }
}
