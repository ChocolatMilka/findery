//! MCP prompts: one playbook per task — review, refactor, doc-sync,
//! divergence, onboard, security, bug-trace — in Claude Code
//! `/mcp__findery__review <target> <scope>` and the like.
//!
//! INTENT: a workflow intent answers in one call; knowing what to do with that
//! answer is the other half. A prompt hands the model the order of the calls,
//! what to verify, and a checklist built from the languages the project
//! actually holds — the intent library's own hints, so the playbook names the
//! constructs the index was told to look for.

mod playbooks;

use serde_json::{Map, Value, json};

use findery_core::intent::IntentLibrary;
use findery_core::model::{Lang, LangShare};

use crate::Session;
use crate::render::{join_and, lang_name, projects::mix};
use playbooks::{PLAYBOOKS, Playbook};

/// Constructs a checklist line takes from each intent it draws on, so every
/// one of them is heard — the most telling come first in the library.
const HINTS_PER_INTENT: usize = 3;

/// A prompt request the bridge refuses, with the reason.
pub struct Refused(pub String);

/// `prompts/list`.
pub fn list() -> Value {
    let prompts: Vec<Value> = PLAYBOOKS.iter().map(describe).collect();
    json!({ "prompts": prompts })
}

fn describe(playbook: &Playbook) -> Value {
    let target = match playbook.target {
        Some(what) => json!({ "name": "target", "description": what, "required": true }),
        None => json!({ "name": "target", "required": false,
            "description": "the feature or area; the whole project when absent" }),
    };
    json!({
        "name": playbook.name,
        "title": playbook.title,
        "description": summary(playbook),
        "arguments": [target, {
            "name": "scope", "required": false,
            "description": "narrows every search: a scope the project declares, or a path glob"
        }],
    })
}

fn summary(playbook: &Playbook) -> String {
    let from_intent = playbook.intent.and_then(|name| {
        let library = IntentLibrary::builtin().ok()?;
        Some(library.find(name)?.summary.clone())
    });
    from_intent.unwrap_or_else(|| playbook.summary.to_string())
}

/// `prompts/get`: the playbook, for the project the bridge stands in.
pub async fn get(session: &Session, params: &Value) -> std::result::Result<Value, Refused> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(playbook) = playbooks::find(name) else {
        let known: Vec<&str> = PLAYBOOKS.iter().map(|p| p.name).collect();
        return Err(Refused(format!(
            "unknown prompt `{name}`; known: {}",
            known.join(", ")
        )));
    };
    let arguments = Arguments::read(playbook, params.get("arguments"))?;
    let context = Context::of(session).await;
    Ok(json!({
        "description": summary(playbook),
        "messages": [{
            "role": "user",
            "content": { "type": "text", "text": body(playbook, &arguments, &context) },
        }],
    }))
}

/// What the user typed after the prompt's name.
struct Arguments {
    target: Option<String>,
    scope: Option<String>,
}

impl Arguments {
    fn read(playbook: &Playbook, raw: Option<&Value>) -> std::result::Result<Self, Refused> {
        let empty = Map::new();
        let map = match raw {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(map)) => map,
            Some(_) => return Err(Refused("prompt arguments are an object".into())),
        };
        if let Some(key) = map
            .keys()
            .find(|k| !["target", "scope"].contains(&k.as_str()))
        {
            return Err(Refused(format!(
                "prompt `{}` has no argument `{key}`; it takes target, scope",
                playbook.name
            )));
        }
        let text = |key: &str| {
            map.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let target = text("target");
        if let (Some(what), None) = (playbook.target, &target) {
            return Err(Refused(format!(
                "prompt `{}` needs `target`: {what}",
                playbook.name
            )));
        }
        Ok(Self {
            target,
            scope: text("scope"),
        })
    }

    /// `, scope: "api"`, `, paths: ["src/**"]`, or nothing.
    fn narrow(&self) -> String {
        match self.scope.as_deref() {
            None => String::new(),
            Some(glob) if glob.contains(['/', '*', '?']) => format!(", paths: [\"{glob}\"]"),
            Some(name) => format!(", scope: \"{name}\""),
        }
    }
}

/// The project a prompt is written for, as far as the daemon could say.
struct Context {
    project: Option<String>,
    langs: Vec<LangShare>,
}

impl Context {
    /// A daemon that is down still gets a playbook, without the checklist.
    async fn of(session: &Session) -> Self {
        let Some(id) = session.detected().await else {
            return Self {
                project: None,
                langs: Vec::new(),
            };
        };
        let langs = match session.client().stats(&id).await {
            Ok(stats) => stats.langs,
            Err(_) => Vec::new(),
        };
        Self {
            project: Some(id.to_string()),
            langs,
        }
    }
}

fn body(playbook: &Playbook, args: &Arguments, context: &Context) -> String {
    let target = args.target.as_deref().unwrap_or("the whole project");
    let project = context
        .project
        .as_deref()
        .map_or(String::new(), |p| format!(" in `{p}`"));
    let mut out = format!(
        "# {} `{target}`{project}\nUse findery as your eyes: search and read from the index, \
         open a file with your own Read only to edit it. Cite every finding as \
         `path:Lstart-Lend`.\n\n## Steps\n",
        playbook.title
    );
    let narrow = args.narrow();
    for (i, step) in playbook.steps.iter().enumerate() {
        let step = step
            .replace("{target}", target)
            .replace("{narrow}", &narrow);
        out.push_str(&format!("{}. {step}\n", i + 1));
    }
    out.push_str("\n## Verify\n");
    for check in playbook.verify {
        out.push_str(&format!("- {check}\n"));
    }
    out.push_str(&checklist(playbook, &context.langs));
    out.push_str(
        "\n## Report\nFindings as `path:Lstart-Lend` — what is wrong — the fix, most severe \
         first. Say what you could not verify.\n",
    );
    out
}

/// One line per language of the project that the playbook's intents carry a
/// hint for: the constructs to look for, and what a change breaks there.
fn checklist(playbook: &Playbook, langs: &[LangShare]) -> String {
    if langs.is_empty() {
        return "\n## Language checklist\nUnknown until the project is synced: run sync, or \
                projects {action: \"status\"}.\n"
            .to_string();
    }
    let lines: Vec<String> = langs
        .iter()
        .filter_map(|share| language_line(playbook, share.lang))
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "\n## Language checklist — {}\n{}",
        mix(langs),
        lines.concat()
    )
}

fn language_line(playbook: &Playbook, lang: Lang) -> Option<String> {
    let constructs = hints(playbook, lang);
    let caveat = playbook.caveats.then(|| playbooks::caveat(lang)).flatten();
    if constructs.is_empty() && caveat.is_none() {
        return None;
    }
    let mut line = format!("- {}:", lang_name(lang));
    if !constructs.is_empty() {
        line.push_str(&format!(" look at {}.", join_and(&constructs)));
    }
    if let Some(caveat) = caveat {
        line.push_str(&format!(" Watch: {caveat}"));
    }
    line.push('\n');
    Some(line)
}

/// The constructs the playbook's intents name for `lang`, each once.
fn hints(playbook: &Playbook, lang: Lang) -> Vec<String> {
    let Ok(library) = IntentLibrary::builtin() else {
        return Vec::new();
    };
    let mut seen: Vec<String> = Vec::new();
    let named = playbook
        .hints_from
        .iter()
        .filter_map(|name| library.find(name));
    for intent in named {
        let fresh: Vec<String> = intent
            .hints_for(lang)
            .iter()
            .filter(|construct| !seen.contains(construct))
            .take(HINTS_PER_INTENT)
            .cloned()
            .collect();
        seen.extend(fresh);
    }
    seen
}

#[cfg(test)]
mod tests;
