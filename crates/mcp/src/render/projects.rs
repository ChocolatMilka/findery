//! Projects as the model reads them: the listing, one project's status, a
//! registration's outcome, and the intents a project's searches accept.

use findery_core::api::{IntentInfo, Job, ModelHealth};
use findery_core::ids::ProjectId;
use findery_core::intent::IntentKind;
use findery_core::model::{IndexStats, LangShare, Project};

use super::{ago, clip, lang_name};

/// Languages a summary line names before it stops.
const SHOWN_LANGS: usize = 4;
/// Longest instruction a status line quotes.
const STATUS_INSTRUCTION_CHARS: usize = 120;

/// One project of the listing, with its counters when the daemon gave them.
pub struct Listed {
    pub project: Project,
    /// `None` when the daemon could not count it; it is listed all the same.
    pub stats: Option<IndexStats>,
}

/// Every project, two lines each; `here` is the one a call without `project`
/// would use.
pub fn list(projects: &[Listed], here: Option<&ProjectId>) -> String {
    if projects.is_empty() {
        return "No project is indexed yet. Register one: projects {action: \"register\", root: \
                \"<absolute path>\"}, then sync."
            .to_string();
    }
    let mut out = format!(
        "{} project(s); `*` is the one detected here\n",
        projects.len()
    );
    for listed in projects {
        let project = &listed.project;
        let mark = if Some(&project.id) == here { "*" } else { "-" };
        out.push_str(&format!(
            "{mark} `{}` — {}\n",
            project.id, project.host_root
        ));
        out.push_str(&format!("    {}\n", facts(project, listed.stats.as_ref())));
    }
    out
}

/// `model harrier (pinned) · rust 71%, axle 20% · 812 chunks · synced 3 h ago ·
/// scopes api · intent usage`.
fn facts(project: &Project, stats: Option<&IndexStats>) -> String {
    let mut parts = vec![model(project)];
    if let Some(stats) = stats {
        if !stats.langs.is_empty() {
            parts.push(mix(&stats.langs));
        }
        parts.push(format!("{} chunks", stats.chunks));
        parts.push(ago(stats.last_sync_at));
    }
    if !project.spec.scopes.is_empty() {
        let names: Vec<&str> = project.spec.scopes.keys().map(String::as_str).collect();
        parts.push(format!("scopes {}", names.join(", ")));
    }
    parts.extend(default_intent(project));
    parts.join(" · ")
}

fn model(project: &Project) -> String {
    match (&project.spec.model, &project.model) {
        (Some(pin), _) => format!("model {pin} (pinned)"),
        (None, Some(current)) => format!("model {current}"),
        (None, None) => "the daemon's default model".to_string(),
    }
}

fn default_intent(project: &Project) -> Option<String> {
    match (&project.spec.task, &project.spec.instruction) {
        (_, Some(_)) => Some("own default instruction".to_string()),
        (Some(task), None) => Some(format!("default intent {}", task.as_str())),
        (None, None) => None,
    }
}

/// `rust 71%, axle 20%, markdown 9%`.
pub fn mix(langs: &[LangShare]) -> String {
    let total: u64 = langs.iter().map(|share| share.chunks).sum::<u64>().max(1);
    let shown: Vec<String> = langs
        .iter()
        .take(SHOWN_LANGS)
        .map(|share| format!("{} {}%", share.lang.as_str(), share.chunks * 100 / total))
        .collect();
    let rest = langs.len().saturating_sub(SHOWN_LANGS);
    match rest {
        0 => shown.join(", "),
        n => format!("{} +{n} more", shown.join(", ")),
    }
}

/// What `status` gathered about one project.
pub struct Status<'a> {
    pub project: &'a Project,
    pub stats: &'a IndexStats,
    /// The health of the project's model profile, when the daemon reported it.
    pub model: Option<&'a ModelHealth>,
    pub running: Option<&'a Job>,
    /// Daemon notices that name the project.
    pub notices: Vec<&'a str>,
}

/// One project, every fact an agent needs before trusting its searches.
pub fn status(s: &Status<'_>) -> String {
    let (project, stats) = (s.project, s.stats);
    let mut out = format!(
        "`{}` — {} · bucket {}\n  {}{}\n  {} files · {} chunks · {} vectors · {} · {:.1} MB\n",
        project.id,
        project.host_root,
        project.bucket,
        model(project),
        runtime(stats, s.model),
        stats.files,
        stats.chunks,
        stats.vectors,
        ago(stats.last_sync_at),
        stats.disk_bytes as f64 / 1e6,
    );
    if !stats.langs.is_empty() {
        out.push_str(&format!("  languages {}\n", mix(&stats.langs)));
    }
    out.push_str(&scopes(project));
    if let Some(text) = &project.spec.instruction {
        out.push_str(&format!(
            "  default instruction \"{}\"\n",
            clip(text, STATUS_INSTRUCTION_CHARS)
        ));
    } else if let Some(task) = project.spec.task {
        out.push_str(&format!("  default intent {}\n", task.as_str()));
    }
    if let Some(job) = s.running {
        out.push_str(&format!("  sync running: job `{}`\n", job.id));
    }
    out.push_str(&problems(s));
    out
}

/// ` · harrier-oss-v1-0.6b, 1024 dims · runtime ok`.
fn runtime(stats: &IndexStats, health: Option<&ModelHealth>) -> String {
    let served = format!(" · {}, {} dims", stats.model, stats.dim);
    match health {
        Some(h) if h.ok => format!("{served} · runtime ok"),
        Some(h) if !h.profile.connected => format!("{served} · runtime not used yet"),
        Some(_) => format!("{served} · runtime DOWN"),
        None => served,
    }
}

fn scopes(project: &Project) -> String {
    if project.spec.scopes.is_empty() {
        return String::new();
    }
    let named: Vec<String> = project
        .spec
        .scopes
        .iter()
        .map(|(name, filter)| match filter.paths.as_slice() {
            [] => name.clone(),
            paths => format!("{name} ({})", paths.join(", ")),
        })
        .collect();
    format!("  scopes {}\n", named.join(", "))
}

/// The `note:` lines: what makes this project's answers narrower than asked.
fn problems(s: &Status<'_>) -> String {
    let mut notes = Vec::new();
    let stats = s.stats;
    if stats.last_sync_at.is_none() {
        notes.push(format!(
            "never synced under this model: sync {{project: \"{}\"}} builds the index",
            s.project.id
        ));
    } else if stats.vectors < stats.chunks {
        notes.push(format!(
            "{} chunk(s) have no vector under {} yet: searches fall back to keywords for them \
             until a sync embeds them",
            stats.chunks - stats.vectors,
            stats.model
        ));
    }
    if let Some(health) = s.model.filter(|h| !h.ok && h.profile.connected) {
        let detail = health.detail.as_deref().unwrap_or("no detail");
        notes.push(format!(
            "the model runtime does not answer ({detail}): searches are keyword-only"
        ));
    }
    notes.extend(s.notices.iter().map(|n| n.to_string()));
    super::notes(&notes)
}

/// What a registration kept and what to do next.
pub fn registered(project: &Project) -> String {
    let spec = &project.spec;
    let mut kept = Vec::new();
    if !spec.scopes.is_empty() {
        let names: Vec<&str> = spec.scopes.keys().map(String::as_str).collect();
        kept.push(format!("scopes {}", names.join(", ")));
    }
    if spec.instruction.is_some() {
        kept.push("default instruction".to_string());
    }
    if !spec.intents.is_empty() {
        let names: Vec<&str> = spec.intents.keys().map(String::as_str).collect();
        kept.push(format!("intents {}", names.join(", ")));
    }
    if let Some(pin) = &spec.model {
        kept.push(format!("model pin {pin}"));
    }
    let kept = match kept.is_empty() {
        true => String::new(),
        false => format!("\n  kept: {}", kept.join(" · ")),
    };
    format!(
        "registered `{}` at {} · bucket {} · {}{kept}\n  extensions: {}\nNext: sync {{project: \
         \"{}\"}} builds the index.\n",
        project.id,
        project.host_root,
        project.bucket,
        model(project),
        spec.extensions.join(", "),
        project.id,
    )
}

/// The intents a project's searches accept, locate first.
pub fn intents(project: &ProjectId, list: &[IntentInfo]) -> String {
    let mut out = format!("intents of `{project}` — pass one as search {{intent}}\n");
    for kind in [IntentKind::Locate, IntentKind::Workflow] {
        out.push_str(&format!("{}:\n", kind.as_str()));
        for info in list.iter().filter(|i| i.kind == kind) {
            out.push_str(&format!("  {} — {}", info.name, info.summary));
            if kind == IntentKind::Workflow {
                out.push_str(&format!(" [legs: {}]", info.legs.join(" → ")));
            }
            out.push('\n');
        }
    }
    out
}

/// The display name of the languages a project mixes, for a prompt.
pub fn language_names(langs: &[LangShare]) -> String {
    let names: Vec<String> = langs
        .iter()
        .map(|s| lang_name(s.lang).to_string())
        .collect();
    super::join_and(&names)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use findery_core::model::{Lang, ProjectSpec, SearchFilter};

    use super::*;

    fn project() -> Project {
        Project {
            id: ProjectId::new("axle").expect("id"),
            name: "axle".into(),
            bucket: "axle".into(),
            model: Some(findery_core::ids::ModelAlias::new("harrier").expect("alias")),
            spec: ProjectSpec {
                scopes: BTreeMap::from([(
                    "api".to_string(),
                    SearchFilter {
                        paths: vec!["crates/server/**".into()],
                        ..SearchFilter::default()
                    },
                )]),
                ..ProjectSpec::default()
            },
            host_root: "C:/work/axle".into(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn stats(chunks: u64, vectors: u64) -> IndexStats {
        IndexStats {
            project: "axle".into(),
            chunks,
            vectors,
            last_sync_at: Some(findery_core::support::clock::now_unix()),
            langs: vec![
                LangShare {
                    lang: Lang::Rust,
                    chunks: 70,
                },
                LangShare {
                    lang: Lang::Axle,
                    chunks: 30,
                },
            ],
            ..IndexStats::default()
        }
    }

    #[test]
    fn the_listing_marks_the_detected_project_and_carries_its_facts() {
        let listed = [Listed {
            project: project(),
            stats: Some(stats(100, 100)),
        }];
        let text = list(&listed, Some(&project().id));
        assert!(text.contains("* `axle` — C:/work/axle"), "{text}");
        assert!(
            text.contains("model harrier · rust 70%, axle 30% · 100 chunks"),
            "{text}"
        );
        assert!(text.contains("scopes api"), "{text}");
    }

    #[test]
    fn a_status_says_why_searches_are_narrower_than_asked() {
        let stats = stats(100, 40);
        let text = status(&Status {
            project: &project(),
            stats: &stats,
            model: None,
            running: None,
            notices: Vec::new(),
        });
        assert!(text.contains("scopes api (crates/server/**)"), "{text}");
        assert!(text.contains("note: 60 chunk(s) have no vector"), "{text}");
    }

    #[test]
    fn an_empty_listing_says_how_to_register() {
        assert!(list(&[], None).contains("projects {action: \"register\""));
    }
}
