//! `findery doctor` — say what is wrong, and the one line that fixes it.
//!
//! INTENT: the failure modes of this system are few and each has exactly one
//! remedy, but they surface far from their cause: a search returns nothing
//! because the model never loaded, a command hangs because a daemon on a remote
//! URL is down. A diagnostic that names the cause AND the command is the
//! difference between a product and a pile of parts.
//!
//! It never starts anything. Reporting is its whole job.

use findery_client::Client;
use findery_client::daemon;
use findery_core::Result;

/// One thing checked, and what to do when it is not right.
struct Check {
    label: &'static str,
    ok: bool,
    detail: String,
    fix: Option<String>,
}

impl Check {
    fn good(label: &'static str, detail: impl Into<String>) -> Self {
        Self {
            label,
            ok: true,
            detail: detail.into(),
            fix: None,
        }
    }

    fn bad(label: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            label,
            ok: false,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }
}

/// Runs every check and prints a report. Returns an error only if printing did.
pub async fn run(client: &Client, json: bool) -> Result<()> {
    let checks = collect(client).await;

    if json {
        let rows: Vec<_> = checks
            .iter()
            .map(|c| {
                serde_json::json!({
                    "check": c.label,
                    "ok": c.ok,
                    "detail": c.detail,
                    "fix": c.fix,
                })
            })
            .collect();
        return crate::render::json(&rows);
    }

    for check in &checks {
        println!(
            "{} {:<12} {}",
            if check.ok { "OK  " } else { "FAIL" },
            check.label,
            check.detail
        );
        if let Some(fix) = &check.fix {
            for line in fix.lines() {
                println!("       → {line}");
            }
        }
    }

    let failed = checks.iter().filter(|c| !c.ok).count();
    println!();
    if failed == 0 {
        println!("everything checks out.");
    } else {
        println!("{failed} problem(s) above, each with its fix.");
    }
    Ok(())
}

async fn collect(client: &Client) -> Vec<Check> {
    let mut checks = Vec::new();
    let url = client.base_url().to_string();

    // 1. The daemon.
    if !daemon::probe(&url).await {
        let fix = if daemon::is_loopback(&url) {
            "findery daemon start   (or just run any command — it starts itself)".to_string()
        } else {
            format!(
                "check the server at {url}; to use a local one instead, \
                 `findery config set server http://127.0.0.1:8077` (or unset \
                 FINDERY_SERVER_URL)"
            )
        };
        checks.push(Check::bad(
            "daemon",
            format!("nothing answering at {url}"),
            fix,
        ));
        // Every later check goes through it, so there is nothing more to say.
        return checks;
    }
    checks.push(Check::good("daemon", format!("answering at {url}")));

    // 2. The model, through the daemon.
    match client.health().await {
        Ok(health) => {
            let summary = format!(
                "{} ({} dims) · store {}",
                health.model, health.dim, health.store_backend
            );
            if health.embedder_ok {
                checks.push(Check::good("model", summary));
            } else {
                checks.push(Check::bad(
                    "model",
                    format!(
                        "{summary} — {}",
                        health.detail.unwrap_or_else(|| "not answering".into())
                    ),
                    "start your embedding runtime (LM Studio, Ollama, `make up-model`),\n\
                     then check FINDERY_EMBED_BASE_URL and FINDERY_EMBED_MODEL",
                ));
            }
        }
        Err(e) => checks.push(Check::bad(
            "model",
            e.to_string(),
            "the daemon answers but its readiness check failed; see its log",
        )),
    }

    // 3. Where the current directory stands.
    match client.list_projects().await {
        Ok(projects) if projects.is_empty() => checks.push(Check::bad(
            "here",
            "no project indexed yet",
            "cd into a project and run: findery init",
        )),
        Ok(projects) => {
            let cwd = crate::scope::cwd().unwrap_or_default();
            // The deepest match wins, exactly as command dispatch resolves it,
            // so the diagnostic never names a project the commands would not use.
            match projects
                .iter()
                .filter(|p| findery_core::path_map::is_within(&p.host_root, &cwd))
                .max_by_key(|p| findery_core::path_map::normalize(&p.host_root).len())
            {
                Some(project) => checks.push(Check::good(
                    "here",
                    format!(
                        "`{}` covers this directory ({} project(s) indexed)",
                        project.id,
                        projects.len()
                    ),
                )),
                None => checks.push(Check::bad(
                    "here",
                    format!(
                        "none of the {} indexed project(s) contains {cwd}",
                        projects.len()
                    ),
                    "findery init        (or use -p <project> from anywhere)",
                )),
            }
        }
        Err(e) => checks.push(Check::bad("here", e.to_string(), "see the daemon log")),
    }

    // 4. The buckets: how many, and whether any of them holds vectors this
    // daemon cannot compare. A conflict is reported, not raised, so it belongs
    // in a diagnostic rather than in a boot failure.
    match client.list_buckets().await {
        Ok(buckets) => {
            let summary = if buckets.is_empty() {
                "none open".to_string()
            } else {
                buckets
                    .iter()
                    .map(|c| format!("{} ({} project(s))", c.name, c.projects))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let conflicts: Vec<String> =
                buckets.iter().filter_map(|c| c.conflict.clone()).collect();
            if conflicts.is_empty() {
                checks.push(Check::good("buckets", summary));
            } else {
                checks.push(Check::bad("buckets", summary, conflicts.join("\n")));
            }
        }
        Err(e) => checks.push(Check::bad("buckets", e.to_string(), "see the daemon log")),
    }

    // 5. Where the data lives, so nobody has to guess. The bucket files
    // themselves are named by `findery bucket ls`.
    let data_dir = findery_core::config::data_dir_from_env();
    checks.push(Check::good("storage", data_dir));
    checks.push(Check::good("daemon log", daemon::log_path()));

    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_dead_daemon_stops_the_report_and_names_the_fix() {
        let client = Client::new("http://127.0.0.1:1").expect("client");
        let checks = collect(&client).await;
        assert_eq!(checks.len(), 1, "later checks all go through the daemon");
        assert!(!checks[0].ok);
        assert!(
            checks[0]
                .fix
                .as_deref()
                .is_some_and(|f| f.contains("daemon start")),
            "{:?}",
            checks[0].fix
        );
    }

    #[tokio::test]
    async fn a_dead_remote_daemon_does_not_suggest_starting_one() {
        let client = Client::new("https://findery.example.com").expect("client");
        let checks = collect(&client).await;
        let fix = checks[0].fix.as_deref().expect("a fix");
        assert!(
            !fix.contains("daemon start"),
            "starting a local one would answer wrongly: {fix}"
        );
        assert!(fix.contains("FINDERY_SERVER_URL"), "{fix}");
    }
}
