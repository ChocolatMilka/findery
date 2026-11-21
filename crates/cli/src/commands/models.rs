//! `findery model` — the embedding models the server knows, and which one a
//! project without a pin uses.
//!
//! INTENT: switching model is a choice, not a migration. Each model's vectors
//! live in their own file of every bucket, so `use` takes effect at the next
//! command and switching back re-embeds nothing. The switch itself is the
//! server's; this module asks, saves the choice, and renders.

use std::path::Path;

use findery_core::api::{ModelHealth, ModelInfo};
use findery_core::config::{bucket_member_path, user_config_path};
use findery_core::embedding::recipe::Recipe;
use findery_core::model::Project;
use findery_core::{Error, Result};

use findery_client::Client;

use crate::args::ModelAction;
use crate::ui::render;
use crate::ui::style::{Col, Columns, Paint, Style};

pub(crate) async fn run(client: &Client, action: ModelAction, json: bool) -> Result<()> {
    match action {
        ModelAction::Ls => ls(client, json).await,
        ModelAction::Show { alias } => show(client, alias.as_deref(), json).await,
        ModelAction::Use { alias } => switch(client, &alias, json).await,
    }
}

async fn ls(client: &Client, json: bool) -> Result<()> {
    let profiles = profiles(client).await?;
    if json {
        let health: Vec<&ModelHealth> = profiles.iter().map(|p| &p.health).collect();
        return render::json(&health);
    }
    print!("{}", render_ls(&profiles, Paint::for_stdout()));
    Ok(())
}

async fn show(client: &Client, alias: Option<&str>, json: bool) -> Result<()> {
    let profiles = profiles(client).await?;
    let profile = pick(&profiles, alias)?;
    if json {
        return render::json(&profile.health);
    }
    print!("{}", render_show(profile, Paint::for_stdout()));
    Ok(())
}

/// Switches the server's default, then saves it so the next start keeps it.
async fn switch(client: &Client, alias: &str, json: bool) -> Result<()> {
    let info = client.set_default_model(alias).await?;
    let saved = crate::settings::save("default_model", info.alias.as_str())
        .map_err(|e| unsaved(&info, &e))?;
    if json {
        render::json(&info)?;
    } else {
        print!("{}", render_switch(&info, &saved.path, Paint::for_stdout()));
    }
    crate::settings::warn_if_shadowed(saved.shadowed);
    Ok(())
}

/// The switch happened, the save did not: say both, or the next start
/// silently undoes what the user just chose.
fn unsaved(info: &ModelInfo, cause: &Error) -> Error {
    Error::Config(format!(
        "the server now uses `{}`, but saving it as `default_model` failed: {}; the next start \
         goes back to the previous default",
        info.alias,
        cause.detail()
    ))
}

/// A profile, its probe, and the projects using it right now.
struct Profile {
    health: ModelHealth,
    users: Vec<User>,
}

struct User {
    id: String,
    pinned: bool,
}

impl Profile {
    fn info(&self) -> &ModelInfo {
        &self.health.profile
    }

    fn status(&self) -> &'static str {
        status(&self.health)
    }

    fn tag(&self) -> Option<String> {
        self.recipe().map(|r| r.tag().to_string())
    }

    fn recipe(&self) -> Option<Recipe> {
        Recipe::parse(self.info().recipe.as_deref()?).ok()
    }

    fn users_text(&self) -> String {
        if self.users.is_empty() {
            return "-".to_string();
        }
        self.users
            .iter()
            .map(|u| match u.pinned {
                true => format!("{} (pinned)", u.id),
                false => u.id.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// `ready`, `down` or `idle`.
///
/// The pool reports a failed lazy build as "could not be built: …"; a profile
/// nobody used yet is idle, not broken.
pub(crate) fn status(h: &ModelHealth) -> &'static str {
    let failed = h
        .detail
        .as_deref()
        .is_some_and(|d| d.starts_with("could not be built"));
    match (h.profile.connected, h.ok) {
        (true, true) => "ready",
        (true, false) => "down",
        (false, _) if failed => "down",
        (false, _) => "idle",
    }
}

/// Every profile with its probe — one request, the one that probes — and the
/// projects that use each.
async fn profiles(client: &Client) -> Result<Vec<Profile>> {
    let health = client.health().await?;
    if health.models.is_empty() {
        return Err(Error::NotFound(
            "the server reports no model profiles: it predates them — `findery doctor` says \
             how to update it"
                .into(),
        ));
    }
    let projects = client.list_projects().await?;
    Ok(join(health.models, &projects))
}

fn join(models: Vec<ModelHealth>, projects: &[Project]) -> Vec<Profile> {
    models
        .into_iter()
        .map(|health| {
            let users = projects
                .iter()
                .filter(|p| p.model.as_ref() == Some(&health.profile.alias))
                .map(|p| User {
                    id: p.id.to_string(),
                    pinned: p.spec.model.is_some(),
                })
                .collect();
            Profile { health, users }
        })
        .collect()
}

/// The named profile, or the default one.
fn pick<'a>(profiles: &'a [Profile], alias: Option<&str>) -> Result<&'a Profile> {
    let found = match alias {
        Some(name) => profiles.iter().find(|p| p.info().alias.as_str() == name),
        None => profiles.iter().find(|p| p.info().is_default),
    };
    found.ok_or_else(|| {
        let known: Vec<&str> = profiles.iter().map(|p| p.info().alias.as_str()).collect();
        Error::model_unknown(alias.unwrap_or("?"), &known, &user_config_path())
    })
}

fn render_ls(profiles: &[Profile], paint: Paint) -> String {
    let header: Vec<String> = ["", "ALIAS", "MODEL", "DIM", "TAG", "STATUS", "USED BY"]
        .map(String::from)
        .to_vec();
    let rows: Vec<Vec<String>> = profiles.iter().map(ls_row).collect();
    let fit = Columns::fit(&std::iter::once(&header).chain(&rows).collect::<Vec<_>>());
    let cols = [
        Col::BOLD,
        Col::BOLD,
        Col::LEFT,
        Col::RIGHT,
        Col::DIM,
        Col::LEFT,
        Col::LEFT,
    ];
    // The header keeps each column's alignment, only dimmer.
    let header_cols = cols.map(|c| Col {
        style: Style::Dim,
        ..c
    });
    let mut out = format!("{}\n", fit.line(&header, &header_cols, paint));
    for row in &rows {
        out.push_str(&fit.line(row, &cols, paint));
        out.push('\n');
    }
    let hint = "* default — `findery model use <alias>` switches it now, re-embedding nothing";
    out.push_str(&format!("\n{}\n", paint.dim(hint)));
    out
}

fn ls_row(p: &Profile) -> Vec<String> {
    let info = p.info();
    vec![
        String::from(if info.is_default { "*" } else { "" }),
        info.alias.to_string(),
        info.model.clone(),
        info.dim.map_or_else(|| "?".to_string(), |d| d.to_string()),
        p.tag().unwrap_or_else(|| "-".to_string()),
        p.status().to_string(),
        p.users_text(),
    ]
}

fn render_show(p: &Profile, paint: Paint) -> String {
    let info = p.info();
    let mut title = vec![p.status()];
    if info.is_default {
        title.insert(0, "default");
    }
    let mut out = format!(
        "{}  {}\n",
        paint.bold(info.alias.as_str()),
        title.join(" · ")
    );
    let mut field = |label: &str, value: String| {
        out.push_str(&format!(
            "  {}  {value}\n",
            paint.dim(&format!("{label:<7}"))
        ));
    };
    field("model", info.model.clone());
    match p.recipe() {
        Some(recipe) => {
            field("recipe", recipe.to_string());
            field("file", format!("{} in each bucket", member_file(&recipe)));
        }
        None => field("recipe", "unknown until its runtime answers".to_string()),
    }
    field("used by", p.users_text());
    if p.status() == "down"
        && let Some(detail) = &p.health.detail
    {
        field("problem", detail.clone());
    }
    out
}

/// The file name a recipe's vectors live in, for a bucket at `bucket.db`.
fn member_file(recipe: &Recipe) -> String {
    let path = bucket_member_path(Path::new("bucket.db"), recipe);
    path.file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn render_switch(info: &ModelInfo, saved_in: &str, paint: Paint) -> String {
    let dims = info.dim.map(|d| format!(" · {d} dims")).unwrap_or_default();
    let alias = info.alias.as_str();
    let lines = [
        (
            "now",
            "every project without a pin uses it from its next command — no restart".to_string(),
        ),
        (
            "saved",
            format!(
                "default_model = \"{alias}\" in {}",
                saved_in.replace('\\', "/")
            ),
        ),
        (
            "next",
            format!(
                "`findery sync` in a project builds its {alias} index; switching back re-embeds \
                 nothing"
            ),
        ),
    ];
    let mut out = format!(
        "default model: {}  {}\n",
        paint.bold(alias),
        paint.dim(&format!("{}{dims}", info.model))
    );
    for (label, text) in lines {
        out.push_str(&format!(
            "  {}  {text}\n",
            paint.dim(&format!("{label:<5}"))
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use findery_core::ids::{ModelAlias, ProjectId};
    use findery_core::model::ProjectSpec;

    use super::*;

    fn health(alias: &str, model: &str, default: bool, connected: bool, ok: bool) -> ModelHealth {
        ModelHealth {
            profile: ModelInfo {
                alias: ModelAlias::new(alias).expect("alias"),
                model: model.into(),
                dim: Some(1024),
                recipe: Some(format!("{model}:1024")),
                is_default: default,
                connected,
            },
            ok,
            detail: None,
        }
    }

    fn project(id: &str, model: &str, pinned: bool) -> Project {
        let alias = ModelAlias::new(model).expect("alias");
        Project {
            id: ProjectId::new(id).expect("id"),
            name: id.into(),
            bucket: id.into(),
            model: Some(alias.clone()),
            host_root: format!("D:/work/{id}"),
            spec: ProjectSpec {
                model: pinned.then_some(alias),
                ..ProjectSpec::default()
            },
            created_at: 0,
            updated_at: 0,
        }
    }

    fn fixture() -> Vec<Profile> {
        let mut failed = health("nomic", "nomic-embed-text", false, false, false);
        failed.detail = Some("could not be built: upstream embedder: connection refused".into());
        join(
            vec![
                health(
                    "embedder",
                    "text-embedding-qwen3-embedding-0.6b",
                    false,
                    false,
                    false,
                ),
                health("harrier", "harrier-oss-v1-0.6b", true, true, true),
                failed,
                health(
                    "qwen3",
                    "text-embedding-qwen3-embedding-0.6b",
                    false,
                    false,
                    false,
                ),
            ],
            &[
                project("axle", "harrier", false),
                project("smalt", "harrier", false),
                project("docs", "qwen3", true),
            ],
        )
    }

    #[test]
    fn the_listing_aligns_every_profile_and_marks_the_default() {
        let text = render_ls(&fixture(), Paint::plain());
        let expected = [
            "   ALIAS     MODEL                                 DIM  TAG       STATUS  USED BY",
            "   embedder  text-embedding-qwen3-embedding-0.6b  1024  47m32eui  idle    -",
            "*  harrier   harrier-oss-v1-0.6b                  1024  zvpmtsdf  ready   axle, smalt",
            "   nomic     nomic-embed-text                     1024  6jollwgu  down    -",
            "   qwen3     text-embedding-qwen3-embedding-0.6b  1024  47m32eui  idle    docs (pinned)",
            "",
            "* default — `findery model use <alias>` switches it now, re-embedding nothing",
            "",
        ];
        assert_eq!(text, expected.join("\n"));
    }

    #[test]
    fn showing_a_profile_names_its_file_and_why_it_is_down() {
        let profiles = fixture();
        let default = render_show(pick(&profiles, None).expect("default"), Paint::plain());
        assert_eq!(
            default,
            "harrier  default · ready\n  \
             model    harrier-oss-v1-0.6b\n  \
             recipe   harrier-oss-v1-0.6b:1024\n  \
             file     bucket.harrier-oss-v1-0-6b-zvpmtsdf.db in each bucket\n  \
             used by  axle, smalt\n"
        );
        let down = render_show(
            pick(&profiles, Some("nomic")).expect("nomic"),
            Paint::plain(),
        );
        assert!(down.contains("problem  could not be built"), "{down}");
    }

    #[test]
    fn an_unknown_alias_lists_the_known_ones_and_where_to_add_one() {
        let profiles = fixture();
        let Err(err) = pick(&profiles, Some("harier")) else {
            panic!("an unknown alias is refused");
        };
        assert_eq!(err.code(), "model_unknown");
        let text = err.to_string();
        assert!(text.contains("embedder, harrier, nomic, qwen3"), "{text}");
        assert!(text.contains("[models.harier]"), "{text}");
    }

    #[test]
    fn a_switch_says_it_is_immediate_and_where_it_was_saved() {
        let info = fixture().remove(1).health.profile;
        let text = render_switch(&info, "C:\\data\\findery\\config.toml", Paint::plain());
        assert_eq!(
            text,
            "default model: harrier  harrier-oss-v1-0.6b · 1024 dims\n  \
             now    every project without a pin uses it from its next command — no restart\n  \
             saved  default_model = \"harrier\" in C:/data/findery/config.toml\n  \
             next   `findery sync` in a project builds its harrier index; switching back \
             re-embeds nothing\n"
        );
    }

    #[test]
    fn a_failed_save_says_the_switch_happened_anyway() {
        let info = fixture().remove(1).health.profile;
        let err = unsaved(&info, &Error::Config("disk full".into()));
        let text = err.to_string();
        assert!(
            text.contains("now uses `harrier`") && text.contains("disk full"),
            "{text}"
        );
    }
}
