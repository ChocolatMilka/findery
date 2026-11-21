//! `findery doctor` — say what is wrong, and the one line that fixes it.
//!
//! INTENT: the failure modes of this system are few and each has exactly one
//! remedy, but they surface far from their cause: a search returns nothing
//! because the model never loaded, a command hangs because a daemon on a remote
//! URL is down. A diagnostic that names the cause AND the command is the
//! difference between a product and a pile of parts.
//!
//! It never starts anything. Reporting is its whole job. The checks on the
//! index itself — models, notices, vectors — live in [`index`].

mod index;

use findery_core::model::Project;
use findery_core::{Error, Result};

use findery_client::Client;
use findery_client::daemon;

use crate::ui::style::Paint;

/// One thing checked, and what to do when it is not right.
pub(super) struct Check {
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
        return crate::ui::render::json(&rows);
    }
    print!("{}", render(&checks, Paint::for_stdout()));
    Ok(())
}

/// The report: one line per check, its fix underneath, a verdict at the end.
fn render(checks: &[Check], paint: Paint) -> String {
    let mut out = String::new();
    for check in checks {
        let mark = if check.ok {
            paint.green("OK  ")
        } else {
            paint.red("FAIL")
        };
        out.push_str(&format!("{mark} {:<10} {}\n", check.label, check.detail));
        for line in check.fix.iter().flat_map(|f| f.lines()) {
            out.push_str(&format!("{}\n", paint.dim(&format!("     → {line}"))));
        }
    }
    let failed = checks.iter().filter(|c| !c.ok).count();
    out.push_str(&match failed {
        0 => "\neverything checks out.\n".to_string(),
        n => format!("\n{n} problem(s) above, each with its fix.\n"),
    });
    out
}

/// The remedy for one failed call: a route the daemon does not serve is a stale
/// binary, anything else is worth the log.
fn fix_for(error: &Error, stale: &str) -> String {
    match error {
        Error::NotFound(_) => stale.to_string(),
        _ => "see the daemon log".to_string(),
    }
}

/// What to say when the daemon answered 404 on a route this build asks for.
///
/// INTENT: a daemon older than the CLI has no such route, and the symptom —
/// "not found" on a command that exists — points at nothing. The version it
/// reports names the cause, and a daemon that reports none is by definition
/// older than the field itself.
fn stale_route_fix(daemon_version: Option<&str>) -> String {
    let cli = env!("CARGO_PKG_VERSION");
    match daemon_version {
        Some(v) if v != cli => format!(
            "the daemon does not serve this route: it runs {v}, this CLI is {cli} — \
             rebuild and reinstall (`make install`), then restart it (`findery daemon stop`)"
        ),
        Some(_) => "the daemon does not serve this route; see its log".to_string(),
        None => format!(
            "the daemon does not serve this route and reports no version, so it predates \
             {cli} — rebuild and reinstall (`make install`), then restart it \
             (`findery daemon stop`)"
        ),
    }
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

    // 2. The models, and what the daemon did on its own at boot. The version
    // it reports — or the absence of one — tells a stale daemon from a broken
    // one later.
    let mut daemon_version: Option<String> = None;
    let mut store_backend = String::new();
    match client.health().await {
        Ok(health) => {
            daemon_version = (!health.version.is_empty()).then(|| health.version.clone());
            store_backend = health.store_backend.clone();
            checks.extend(index::model_checks(&health));
            checks.extend(index::notice_checks(&health.notices));
        }
        Err(e) => checks.push(Check::bad(
            "model",
            e.to_string(),
            "the daemon answers but its readiness check failed; see its log",
        )),
    }
    let stale = stale_route_fix(daemon_version.as_deref());

    // 3. Where the current directory stands, and whether each project has
    // vectors under the model it uses now.
    let projects = client.list_projects().await;
    match &projects {
        Ok(list) => {
            checks.push(here(list));
            checks.extend(index::vector_checks(client, list).await);
        }
        Err(e) => checks.push(Check::bad("here", e.to_string(), fix_for(e, &stale))),
    }

    // 4. The buckets: whether any holds vectors this daemon cannot compare. A
    // conflict is reported, not raised, so it belongs in a diagnostic.
    match client.list_buckets().await {
        Ok(buckets) => checks.push(buckets_check(&buckets)),
        Err(e) => checks.push(Check::bad("buckets", e.to_string(), fix_for(&e, &stale))),
    }

    // 5. The manifest, when there is one.
    if let Ok(list) = &projects {
        checks.extend(manifest_check(list));
    }

    // 6. Where the data lives, so nobody has to guess.
    let data_dir = findery_core::config::data_dir_from_env();
    let storage = match store_backend.as_str() {
        "" => data_dir,
        backend => format!("{data_dir}  (vectors in {backend})"),
    };
    checks.push(Check::good("storage", storage));
    checks.push(Check::good("daemon log", daemon::log_path()));
    checks
}

/// Which project covers the current directory. The deepest match wins, as
/// command dispatch resolves it, so the doctor never names another one.
fn here(projects: &[Project]) -> Check {
    if projects.is_empty() {
        return Check::bad(
            "here",
            "no project indexed yet",
            "cd into a project and run: findery init",
        );
    }
    let cwd = crate::workspace::scope::cwd().unwrap_or_default();
    let covering = projects
        .iter()
        .filter(|p| findery_core::support::path_map::is_within(&p.host_root, &cwd))
        .max_by_key(|p| findery_core::support::path_map::normalize(&p.host_root).len());
    match covering {
        Some(project) => Check::good(
            "here",
            format!(
                "`{}` covers this directory ({} project(s) indexed)",
                project.id,
                projects.len()
            ),
        ),
        None => Check::bad(
            "here",
            format!(
                "none of the {} indexed project(s) contains {cwd}",
                projects.len()
            ),
            "findery init        (or use -p <project> from anywhere)",
        ),
    }
}

fn buckets_check(buckets: &[findery_core::api::BucketInfo]) -> Check {
    let summary = if buckets.is_empty() {
        "none open".to_string()
    } else {
        buckets
            .iter()
            .map(|c| format!("{} ({} project(s))", c.name, c.projects))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let conflicts: Vec<String> = buckets
        .iter()
        .flat_map(|b| {
            b.members
                .iter()
                .filter_map(|m| m.conflict.clone())
                .chain(b.members.is_empty().then(|| b.conflict.clone()).flatten())
        })
        .collect();
    match conflicts.is_empty() {
        true => Check::good("buckets", summary),
        false => Check::bad("buckets", summary, conflicts.join("\n")),
    }
}

/// The file records the bucket the daemon *answered with*, so a disagreement
/// means it was edited by hand or the project was registered elsewhere since.
/// The daemon wins — its answer is which file holds the row.
fn manifest_check(projects: &[Project]) -> Option<Check> {
    let cwd = std::env::current_dir().ok()?;
    let found = findery_client::resolve::discover_manifest(&cwd).ok()??;
    let root = found.root.display().to_string().replace('\\', "/");
    let path = crate::workspace::manifest::path_of(&found.root)
        .display()
        .to_string();
    let project = projects.iter().find(|p| p.host_root == root);
    Some(match (project, found.config.bucket.name.as_deref()) {
        (Some(project), Some(declared)) if declared != project.bucket => Check::bad(
            "manifest",
            format!(
                "{path} says bucket `{declared}`, but the daemon holds `{}` in `{}`",
                project.id, project.bucket
            ),
            "findery init        (rewrites the manifest from the daemon's answer)",
        ),
        (Some(project), _) => Check::good("manifest", format!("{path} → `{}`", project.id)),
        (None, _) => Check::bad(
            "manifest",
            format!("{path} names a project the daemon does not have"),
            "findery init        (registers this directory again)",
        ),
    })
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

    #[test]
    fn a_route_the_daemon_does_not_serve_names_the_stale_binary() {
        let older = stale_route_fix(Some("0.0.9"));
        assert!(older.contains("0.0.9"), "{older}");
        assert!(older.contains(env!("CARGO_PKG_VERSION")), "{older}");
        assert!(older.contains("make install"), "{older}");

        // A daemon too old to report a version is the older one by definition.
        let voiceless = stale_route_fix(None);
        assert!(voiceless.contains("no version"), "{voiceless}");
        assert!(voiceless.contains("make install"), "{voiceless}");

        let same = stale_route_fix(Some(env!("CARGO_PKG_VERSION")));
        assert!(same.contains("see its log"), "{same}");
    }

    #[test]
    fn a_fix_never_carries_the_indentation_of_its_source() {
        for fix in [stale_route_fix(None), stale_route_fix(Some("0.0.9"))] {
            for line in fix.lines() {
                assert!(!line.starts_with(' '), "indented fix: {fix:?}");
            }
        }
    }

    #[test]
    fn only_a_missing_resource_blames_the_binary() {
        assert!(fix_for(&Error::NotFound("route".into()), "STALE").contains("STALE"));
        assert!(fix_for(&Error::Store("disk".into()), "STALE").contains("daemon log"));
    }

    #[test]
    fn the_report_puts_each_fix_under_its_check_and_counts_the_problems() {
        let checks = [
            Check::good(
                "model",
                "harrier · harrier-oss-v1-0.6b (1024 dims) · default · ready",
            ),
            Check::bad(
                "vectors",
                "`axle` has no vectors under `harrier` yet",
                "findery sync -p axle   (keyword search works meanwhile)",
            ),
        ];
        assert_eq!(
            render(&checks, Paint::plain()),
            "OK   model      harrier · harrier-oss-v1-0.6b (1024 dims) · default · ready\n\
             FAIL vectors    `axle` has no vectors under `harrier` yet\n     \
             → findery sync -p axle   (keyword search works meanwhile)\n\
             \n1 problem(s) above, each with its fix.\n"
        );
    }

    #[test]
    fn a_conflict_on_any_file_of_a_bucket_is_a_problem() {
        let mut info: findery_core::api::BucketInfo = serde_json::from_value(serde_json::json!({
            "name": "axle", "path": "/p/bucket.db", "projects": 1, "chunks": 0,
            "disk_bytes": 0, "is_default": false,
            "members": [{ "path": "/p/bucket.x-abcdefgh.db", "tag": "abcdefgh",
                          "recipe": "x:8", "projects": 1, "chunks": 0, "disk_bytes": 0,
                          "conflict": "holds `y:8`" }]
        }))
        .expect("fixture");
        let check = buckets_check(std::slice::from_ref(&info));
        assert!(!check.ok && check.fix.as_deref() == Some("holds `y:8`"));
        info.members[0].conflict = None;
        assert!(buckets_check(&[info]).ok);
    }
}
