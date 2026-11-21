//! The doctor's view of the index itself: each model profile, what the server
//! did on its own at boot, and which projects have no vectors under the model
//! they use now.
//!
//! INTENT: split from the report so each check is a pure function of what the
//! server answered, testable from a fixture. Only [`vector_checks`] asks
//! anything, one `stats` per project.

use findery_core::api::{HealthReport, ModelHealth};
use findery_core::model::{IndexStats, Project};

use findery_client::Client;

use super::Check;

/// One check per model profile; a server that predates profiles gets the one
/// line it always had.
pub(super) fn model_checks(health: &HealthReport) -> Vec<Check> {
    if health.models.is_empty() {
        return vec![legacy_model_check(health)];
    }
    health.models.iter().map(model_check).collect()
}

fn model_check(h: &ModelHealth) -> Check {
    let p = &h.profile;
    let dims = p.dim.map(|d| format!(" ({d} dims)")).unwrap_or_default();
    let role = if p.is_default { " · default" } else { "" };
    let summary = format!("{} · {}{dims}{role}", p.alias, p.model);
    match crate::commands::models::status(h) {
        "ready" => Check::good("model", format!("{summary} · ready")),
        "idle" => Check::good("model", format!("{summary} · idle until a project uses it")),
        _ => Check::bad(
            "model",
            format!(
                "{summary} · {}",
                h.detail.as_deref().unwrap_or("not answering")
            ),
            format!(
                "start the runtime serving `{}` at that address (`runtime/models.ps1 start`\n\
                 or `.sh`, LM Studio, Ollama, `make up-model`), or point at the one that\n\
                 runs: findery config set url <its address> — or switch: findery model use <alias>",
                p.model
            ),
        ),
    }
}

fn legacy_model_check(health: &HealthReport) -> Check {
    let summary = format!("{} ({} dims)", health.model, health.dim);
    if health.embedder_ok {
        return Check::good("model", summary);
    }
    Check::bad(
        "model",
        format!(
            "{summary} — {}",
            health.detail.as_deref().unwrap_or("not answering")
        ),
        "start your embedding runtime (`runtime/models.ps1 start` or `.sh`, LM Studio,\n\
         Ollama, `make up-model`),\n\
         then check FINDERY_EMBED_BASE_URL and FINDERY_EMBED_MODEL",
    )
}

/// What the server did, or could not do, on its own — each with what to do.
pub(super) fn notice_checks(notices: &[String]) -> Vec<Check> {
    notices.iter().map(|n| notice_check(n)).collect()
}

/// The notices are the server's sentences; the doctor recognizes each kind by
/// the phrase the server writes for it, and a kind it does not know still
/// shows, with the log as the fix.
fn notice_check(notice: &str) -> Check {
    let has = |phrase: &str| notice.contains(phrase);
    // Information, not a problem: a file moved, one not claimed yet, or an old
    // copy kept beside the file that serves — the sentence itself says what to
    // delete, and nothing is wrong until then.
    let informational =
        has(" is now ") || has("records no recipe yet") || has("was left as it is because");
    let fix = if informational {
        return Check::good("notice", notice);
    } else if has("stays where it is because") {
        "close whatever holds the file (another findery, an editor, a backup tool), then\n\
         restart the server — `findery daemon stop`; the next command retries the move"
    } else if has("has rows in buckets") {
        "keep one copy: `findery bucket ls` shows both buckets; remove the stale one with\n\
         `findery bucket rm <name> --force` if it holds nothing else"
    } else if has("is unfinished") {
        "close whatever holds the file, then restart the server — `findery daemon stop`;\n\
         the next start finishes the removal"
    } else {
        "see the server log, named at the end of this report"
    };
    Check::bad("notice", notice, fix)
}

/// One `stats` per project: a project with no vectors under its model is one
/// whose searches answer from keywords alone.
pub(super) async fn vector_checks(client: &Client, projects: &[Project]) -> Vec<Check> {
    let mut checks = Vec::new();
    for project in projects {
        match client.stats(&project.id).await {
            Ok(stats) => checks.extend(vector_check(project, &stats)),
            Err(e) => checks.push(Check::bad(
                "vectors",
                format!("`{}`: {e}", project.id),
                "see the server log",
            )),
        }
    }
    if checks.is_empty() && !projects.is_empty() {
        checks.push(Check::good(
            "vectors",
            "every project has vectors under its model",
        ));
    }
    checks
}

fn vector_check(project: &Project, stats: &IndexStats) -> Option<Check> {
    if stats.vectors > 0 {
        return None;
    }
    let model = project
        .model
        .as_ref()
        .map_or_else(|| stats.model.clone(), ToString::to_string);
    Some(Check::bad(
        "vectors",
        format!("`{}` has no vectors under `{model}` yet", project.id),
        format!(
            "findery sync -p {}   (keyword search works meanwhile)",
            project.id
        ),
    ))
}

#[cfg(test)]
mod tests {
    use findery_core::api::ModelInfo;
    use findery_core::ids::{ModelAlias, ProjectId};
    use findery_core::model::ProjectSpec;

    use super::*;

    fn profile(alias: &str, default: bool, connected: bool, ok: bool) -> ModelHealth {
        ModelHealth {
            profile: ModelInfo {
                alias: ModelAlias::new(alias).expect("alias"),
                model: format!("{alias}-model"),
                dim: Some(1024),
                recipe: None,
                is_default: default,
                connected,
            },
            ok,
            detail: (!ok).then(|| "upstream embedder: connection refused".into()),
        }
    }

    fn health(models: Vec<ModelHealth>) -> HealthReport {
        HealthReport {
            ok: true,
            version: String::new(),
            model: "m".into(),
            dim: 1024,
            store_backend: "sqlite".into(),
            projects: 1,
            embedder_ok: true,
            detail: None,
            notices: Vec::new(),
            models,
        }
    }

    #[test]
    fn each_profile_is_a_line_and_only_a_down_one_carries_a_fix() {
        let mut idle = profile("qwen3", false, false, false);
        idle.detail = Some("not used yet: built on the first operation that needs it".into());
        let checks = model_checks(&health(vec![
            profile("harrier", true, true, true),
            idle,
            profile("nomic", false, true, false),
        ]));
        assert_eq!(checks.len(), 3);
        assert!(checks[0].ok && checks[0].detail.contains("default · ready"));
        assert!(checks[1].ok && checks[1].detail.contains("idle"));
        assert!(!checks[2].ok);
        let fix = checks[2].fix.as_deref().expect("a fix");
        assert!(
            fix.contains("nomic-model") && fix.contains("findery model use"),
            "{fix}"
        );
    }

    #[test]
    fn a_server_without_profiles_still_gets_its_model_line() {
        let mut old = health(Vec::new());
        old.embedder_ok = false;
        let checks = model_checks(&old);
        assert_eq!(checks.len(), 1);
        assert!(!checks[0].ok);
    }

    /// The phrases are the ones the server writes (`AdoptNotice::describe`,
    /// the boot's home picker, the tombstones); a change there must change
    /// these fixtures with it.
    #[test]
    fn every_notice_kind_gets_its_own_fix_and_information_is_not_a_problem() {
        let cases = [
            (
                "bucket `a`: /a/bucket.db is now /a/bucket.m-x.db (`m:8`)",
                None,
            ),
            (
                "bucket `a`: /a/bucket.db records no recipe yet; the first sync claims it",
                None,
            ),
            (
                "bucket `a`: /a/bucket.db was left as it is because /a/bucket.m-x.db already \
                 holds `m:8`; delete /a/bucket.db once you no longer need it",
                None,
            ),
            (
                "bucket `a`: /a/bucket.db stays where it is because it is held by another \
                 process (os error 32); it keeps serving `m:8`, and the move is retried at the \
                 next start",
                Some("retries the move"),
            ),
            (
                "project `axle` has rows in buckets `default`, `axle`; serving `axle`, the most \
                 recently registered — `findery rm` the copy you no longer want",
                Some("keep one copy"),
            ),
            (
                "the removal of bucket `w` is unfinished: busy; it is not served until the next \
                 start finishes it",
                Some("finishes the removal"),
            ),
            ("something new", Some("server log")),
        ];
        for (notice, wanted) in cases {
            let check = notice_check(notice);
            match wanted {
                None => assert!(check.ok && check.fix.is_none(), "{notice}"),
                Some(phrase) => {
                    let fix = check.fix.as_deref().unwrap_or_default().replace('\n', " ");
                    assert!(!check.ok && fix.contains(phrase), "{notice} → {fix}");
                }
            }
        }
    }

    #[test]
    fn a_project_without_vectors_under_its_model_is_told_to_sync() {
        let project = Project {
            id: ProjectId::new("axle").expect("id"),
            name: "axle".into(),
            bucket: "axle".into(),
            model: Some(ModelAlias::new("harrier").expect("alias")),
            host_root: "D:/work/axle".into(),
            spec: ProjectSpec::default(),
            created_at: 0,
            updated_at: 0,
        };
        let mut stats = IndexStats {
            project: "axle".into(),
            bucket: "axle".into(),
            files: 12,
            chunks: 0,
            vectors: 0,
            dim: 1024,
            model: "harrier-oss-v1-0.6b".into(),
            store_backend: "sqlite".into(),
            last_sync_at: None,
            disk_bytes: 0,
            langs: Vec::new(),
        };
        let check = vector_check(&project, &stats).expect("missing vectors");
        assert_eq!(check.detail, "`axle` has no vectors under `harrier` yet");
        assert!(
            check
                .fix
                .as_deref()
                .is_some_and(|f| f.starts_with("findery sync -p axle"))
        );
        stats.vectors = 40;
        assert!(vector_check(&project, &stats).is_none());
    }
}
