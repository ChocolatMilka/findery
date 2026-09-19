//! Command dispatch, and the "which project am I in" resolution.
//!
//! Kept apart from `main.rs` so the argument surface reads as a table of
//! commands rather than a table of commands wrapped around their bodies.

use crate::render;
use crate::{Cli, Command, DaemonAction, DocAction, Scope};
use findery_client::Client;
use findery_core::api::{AddDocument, ExportRequest, RegisterProject, SyncRequest, WatchRequest};
use findery_core::ids::ProjectId;
use findery_core::model::{ChunkKind, Lang, SearchFilter, SearchMode, SearchRequest};
use findery_core::path_map;
use findery_core::prompt::QueryTask;
use findery_core::{Error, Result};
use std::io::Read;

pub(crate) async fn dispatch(cli: Cli) -> Result<()> {
    let mut client = Client::new(&cli.server)?;
    if let Some(token) = &cli.token {
        client = client.with_token(token.clone());
    }
    let json = cli.json;

    // Everything but the diagnostic and the lifecycle commands needs a daemon.
    // Starting it here is what removes the "keep a terminal open" step: the user
    // never learns that a daemon exists.
    if !matches!(cli.command, Command::Doctor | Command::Daemon { .. }) {
        if cli.no_spawn {
            // SAFETY: set before any request, on the single CLI thread.
            unsafe { std::env::set_var("FINDERY_NO_SPAWN", "1") };
        }
        if findery_client::daemon::ensure_running(&client).await? {
            eprintln!("(started the daemon in the background)");
        }
    }

    match cli.command {
        Command::Init {
            path,
            id,
            ext,
            no_sync,
            push,
        } => {
            let root = absolute(&path)?;
            let id = match id {
                Some(explicit) => ProjectId::new(explicit)?,
                None => derive_id(&client, &root).await?,
            };
            let project = client
                .register(&RegisterProject {
                    root: root.clone(),
                    id: Some(id.to_string()),
                    extensions: ext,
                    push: push.then_some(true),
                    ..Default::default()
                })
                .await?;
            if json {
                render::json(&project)?;
            } else {
                println!("indexing `{}` at {}", project.id, project.host_root);
            }
            if !no_sync {
                let report = if project.spec.push {
                    crate::push::sync(&client, &project).await?
                } else {
                    client.sync_now(&project.id).await?
                };
                if json {
                    render::json(&report)?;
                } else {
                    render::sync(&report);
                    println!("\nready — try: findery search \"<your question>\"");
                }
            }
        }

        Command::Search {
            query,
            scope,
            k,
            mode,
            task,
            instruct,
            paths,
            langs,
            kinds,
            max_chars,
        } => {
            let request = SearchRequest {
                project: resolve(&client, &scope).await?,
                query: query.join(" "),
                k: Some(k),
                mode: mode.as_deref().and_then(parse_mode),
                filter: SearchFilter {
                    paths,
                    langs: langs.iter().filter_map(|l| Lang::parse(l)).collect(),
                    kinds: kinds.iter().filter_map(|k| ChunkKind::parse(k)).collect(),
                    symbol_contains: None,
                },
                max_snippet_chars: max_chars,
                task: task.as_deref().and_then(QueryTask::parse),
                instruction: instruct,
            };
            let hits = client.search(&request).await?;
            if json {
                render::json(&hits)?;
            } else {
                render::hits(&hits);
            }
        }

        Command::Sync {
            scope,
            full,
            paths,
            background,
        } => {
            let id = resolve(&client, &scope).await?;
            // A project the daemon cannot see is fed by the client. The choice
            // was made at registration by testing the path, so nobody has to
            // remember which mode a project is in.
            let project = client.project(&id).await?;
            if project.spec.push {
                let report = crate::push::sync(&client, &project).await?;
                if json {
                    render::json(&report)?;
                } else {
                    render::sync(&report);
                }
                return Ok(());
            }
            let request = SyncRequest {
                mode: full.then(|| "full".to_string()),
                paths: (!paths.is_empty()).then_some(paths),
                wait: !background,
            };
            match client.sync(&id, &request).await? {
                findery_core::api::SyncResponse::Completed { report } => {
                    if json {
                        render::json(&report)?;
                    } else {
                        render::sync(&report);
                    }
                }
                findery_core::api::SyncResponse::Accepted { job } => {
                    if json {
                        render::json(&job)?;
                    } else {
                        println!(
                            "job {} queued; follow it with `findery jobs {}`",
                            job.id, job.id
                        );
                    }
                }
            }
        }

        Command::Status { scope } => {
            let stats = client.stats(&resolve(&client, &scope).await?).await?;
            if json {
                render::json(&stats)?;
            } else {
                render::stats(&stats);
            }
        }

        Command::Projects => {
            let list = client.list_projects().await?;
            if json {
                render::json(&list)?;
            } else {
                let here = current_project(&list).map(|id| id.to_string());
                render::projects(&list, here.as_deref());
            }
        }

        Command::Rm { scope } => {
            let id = resolve(&client, &scope).await?;
            let deleted = client.delete_project(&id).await?;
            let message = if deleted {
                format!("removed {id}")
            } else {
                "nothing to remove".to_string()
            };
            println!("{message}");
        }

        Command::Doc { action } => match action {
            DocAction::Add { path, scope, stdin } => {
                let id = resolve(&client, &scope).await?;
                let text = if stdin {
                    let mut buf = String::new();
                    std::io::stdin()
                        .read_to_string(&mut buf)
                        .map_err(|e| Error::Invalid(format!("cannot read stdin: {e}")))?;
                    Some(buf)
                } else {
                    None
                };
                let report = client
                    .add_document(&id, &AddDocument { path, text })
                    .await?;
                if json {
                    render::json(&report)?;
                } else {
                    render::sync(&report);
                }
            }
            DocAction::Rm { path, scope } => {
                let id = resolve(&client, &scope).await?;
                let removed = client.remove_document(&id, &path).await?;
                println!("removed {removed} chunk(s) for {path}");
            }
        },

        Command::Export { scope, format, dir } => {
            let id = resolve(&client, &scope).await?;
            let report = client
                .export(
                    &id,
                    &ExportRequest {
                        format: Some(format),
                        dir,
                    },
                )
                .await?;
            if json {
                render::json(&report)?;
            } else {
                render::export(&report);
            }
        }

        Command::Watch {
            scope,
            off,
            debounce_ms,
        } => {
            let id = resolve(&client, &scope).await?;
            let project = client.project(&id).await?;
            if project.spec.push {
                if off {
                    println!("nothing to stop: this project is watched by this command itself");
                    return Ok(());
                }
                // Blocking on purpose: the watcher is this process.
                return crate::push::watch(
                    &client,
                    &project,
                    std::time::Duration::from_millis(debounce_ms.clamp(100, 60_000)),
                )
                .await;
            }
            let state = client
                .set_watch(
                    &id,
                    &WatchRequest {
                        enabled: !off,
                        debounce_ms,
                    },
                )
                .await?;
            println!(
                "{}: watching {}",
                state.project,
                if state.watching { "on" } else { "off" }
            );
        }

        Command::Health => {
            let health = client.health().await?;
            if json {
                render::json(&health)?;
            } else {
                render::health(&health);
            }
        }

        Command::Doctor => crate::doctor::run(&client, json).await?,

        Command::Daemon { action } => match action {
            DaemonAction::Start => {
                if findery_client::daemon::ensure_running(&client).await? {
                    println!("started — {}", client.base_url());
                } else {
                    println!("already running — {}", client.base_url());
                }
            }
            DaemonAction::Stop => {
                if findery_client::daemon::probe(client.base_url()).await {
                    client.stop_daemon().await?;
                    println!("stopping — {}", client.base_url());
                } else {
                    println!("not running — {}", client.base_url());
                }
            }
            DaemonAction::Status => {
                if findery_client::daemon::probe(client.base_url()).await {
                    render::health(&client.health().await?);
                } else {
                    println!("not running — {}", client.base_url());
                }
            }
        },

        Command::Jobs { id } => match id {
            Some(id) => render::json(&client.job(&id).await?)?,
            None => {
                let list = client.jobs().await?;
                if json {
                    render::json(&list)?;
                } else {
                    render::jobs(&list);
                }
            }
        },
    }
    Ok(())
}

/// Resolves the project a command applies to.
async fn resolve(client: &Client, scope: &Scope) -> Result<ProjectId> {
    if let Some(explicit) = &scope.project {
        return ProjectId::new(explicit.clone());
    }
    let projects = client.list_projects().await?;
    current_project(&projects).ok_or_else(|| {
        let cwd = cwd().unwrap_or_else(|_| "the current directory".into());
        Error::Invalid(format!(
            "no indexed project contains `{cwd}`\n  \
             run `findery init` here, or name one with `-p <project>` (`findery projects` lists them)"
        ))
    })
}

/// The registered project whose root contains the current directory.
///
/// The deepest match wins, so a project nested inside another one is picked
/// over its parent.
fn current_project(projects: &[findery_core::model::Project]) -> Option<ProjectId> {
    let cwd = cwd().ok()?;
    let mut best: Option<(usize, ProjectId)> = None;
    for project in projects {
        if !path_map::is_within(&project.host_root, &cwd) {
            continue;
        }
        let depth = path_map::normalize(&project.host_root).len();
        if best.as_ref().is_none_or(|(d, _)| depth > *d) {
            best = Some((depth, project.id.clone()));
        }
    }
    best.map(|(_, id)| id)
}

/// Picks an id for a new project, refusing to hijack an existing one.
async fn derive_id(client: &Client, root: &str) -> Result<ProjectId> {
    let name = path_map::normalize(root)
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("project")
        .to_string();
    let id = ProjectId::slugify(&name)?;

    // An auto-derived id colliding with a different root is almost always an
    // accident: two directories with the same name on two disks.
    if let Ok(existing) = client.project(&id).await
        && !path_map::is_within(&existing.host_root, root)
        && !path_map::is_within(root, &existing.host_root)
    {
        return Err(Error::Invalid(format!(
            "id `{id}` is already used by {}\n  pick another with `findery init --id <name>`",
            existing.host_root
        )));
    }
    Ok(id)
}

/// The current working directory, normalized.
pub(crate) fn cwd() -> Result<String> {
    let dir = std::env::current_dir()
        .map_err(|e| Error::Invalid(format!("cannot read the current directory: {e}")))?;
    Ok(path_map::normalize(&dir.to_string_lossy()))
}

/// Absolute, normalized path with Windows verbatim prefixes stripped.
///
/// `canonicalize` returns `\\?\C:\…` on Windows, which is correct but useless in
/// a message and breaks prefix matching against a user-typed path.
fn absolute(path: &str) -> Result<String> {
    let resolved = std::fs::canonicalize(path)
        .map_err(|e| Error::Invalid(format!("cannot resolve `{path}`: {e}")))?;
    let text = path_map::normalize(&resolved.to_string_lossy());
    Ok(text.strip_prefix("//?/").unwrap_or(&text).to_string())
}

fn parse_mode(raw: &str) -> Option<SearchMode> {
    match raw {
        "hybrid" => Some(SearchMode::Hybrid),
        "vector" => Some(SearchMode::Vector),
        "keyword" => Some(SearchMode::Keyword),
        _ => None,
    }
}
