//! What the bridge advertises: the five tools and their schemas.
//!
//! Held apart from the code that answers them: a description is edited for a
//! reader and an implementation for the compiler, and mixing the two is how a
//! tool ends up documented as something it no longer does. The schemas are
//! also the source of truth for which argument names a call may use
//! ([`declared`]), so the two can never disagree.
//!
//! Every closed list — intents, modes, languages, kinds — is generated from the
//! vocabulary in `findery-core`, never typed here.

use serde_json::{Value, json};

use findery_core::intent::{IntentKind, IntentLibrary};
use findery_core::model::{ChunkKind, Lang, SearchMode};

/// How much of each hit `search` prints.
pub const DETAILS: &[&str] = &["compact", "full"];
/// What `projects` can do.
pub const PROJECT_ACTIONS: &[&str] = &["list", "status", "register", "intents"];
/// What `note` can do.
pub const NOTE_ACTIONS: &[&str] = &["add", "remove"];
/// How `sync` walks.
pub const SYNC_MODES: &[&str] = &["incremental", "full"];

/// The tool names, in the order `tools/list` gives them.
pub const TOOLS: &[&str] = &["search", "read", "projects", "sync", "note"];

/// `tools/list`.
pub fn catalogue() -> Value {
    json!({ "tools": TOOLS.iter().filter_map(|name| tool(name)).collect::<Vec<_>>() })
}

/// The argument names `name` declares, or `None` for a tool that does not exist.
pub fn declared(name: &str) -> Option<Vec<String>> {
    let tool = tool(name)?;
    let properties = tool["inputSchema"]["properties"].as_object()?;
    Some(properties.keys().cloned().collect())
}

fn tool(name: &str) -> Option<Value> {
    let (description, properties, required) = match name {
        "search" => search(),
        "read" => read(),
        "projects" => projects(),
        "sync" => sync(),
        "note" => note(),
        _ => return None,
    };
    Some(json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        },
    }))
}

type Spec = (String, Value, Vec<&'static str>);

fn project_property() -> Value {
    json!({ "type": "string", "description": "Project id. Optional: detected from \
        .findery/config.toml at or above the working directory, else from the registered root \
        that contains it." })
}

fn search() -> Spec {
    let description = "Find code, docs or config by meaning AND by exact identifier \
        (hybrid vector + BM25). Use it BEFORE grep or reading files whenever you need to locate \
        something: it answers ranked, line-numbered excerpts you cite as `path:L12-L40` and pass \
        to `read`. Pick the `intent` that matches your question; a workflow intent runs several \
        searches at once and groups the answer by leg.";
    let properties = json!({
        "project": project_property(),
        "query": { "type": "string", "description": "What you look for: a question, a \
            behaviour, or an identifier. Required unless `like` is given." },
        "intent": { "type": "string", "enum": intent_names(), "description": intent_guide() },
        "instruction": { "type": "string", "description": "Your own one-sentence task for the \
            embedding model, replacing the intent's: \"Given <input>, retrieve <what>.\" \
            Describe the TASK, not the query." },
        "raw_query": { "type": "boolean", "description": "Send `query` to the model verbatim: \
            no instruction, no template." },
        "like": { "type": "string", "description": "A hit's locator (`path:L12-L40`) or chunk \
            id: finds code that looks like it, by its stored vector — nothing is embedded." },
        "mode": { "type": "string", "enum": modes(),
            "description": "hybrid (default) fuses both lists; keyword for an exact identifier \
            or error string; vector for a fuzzy concept." },
        "paths": { "type": "array", "items": { "type": "string" },
            "description": "Path globs that narrow BEFORE scoring, e.g. crates/core/**." },
        "langs": { "type": "array", "items": { "type": "string",
            "enum": langs() } },
        "kinds": { "type": "array", "items": { "type": "string",
            "enum": kinds() },
            "description": "struct covers a class; doc_section is a Markdown section." },
        "symbol": { "type": "string", "description": "Only chunks whose symbol contains this \
            (case-insensitive)." },
        "scope": { "type": "string", "description": "A named restriction the project declares; \
            `projects {action: \"status\"}` lists them." },
        "k": { "type": "integer", "minimum": 1, "maximum": 50, "description": "Hits per search \
            (default 8; a workflow keeps each leg's own unless you set it)." },
        "max_snippet_chars": { "type": "integer", "minimum": 0, "description": "Per-excerpt cap \
            (default 1200). The most direct token lever." },
        "detail": { "type": "string", "enum": DETAILS, "description": "compact (default): a \
            workflow shows its top 3 hits per leg in full, the rest as one line each. full: every \
            excerpt, at the daemon's own cap unless max_snippet_chars says otherwise." }
    });
    (description.into(), properties, vec![])
}

fn read() -> Spec {
    let description = "Read what a search pointed at, from the index: a chunk by id, or the \
        lines a locator names with `context` lines around them (the daemon's disk copy when it \
        walked the file, else the indexed text). With `glob`, list the indexed files instead. \
        Cheaper than locating a file yourself; use your own Read when you are about to edit.";
    let properties = json!({
        "project": project_property(),
        "locator": { "type": "string", "description": "`path:L12-L40`, `path:L12` or a chunk \
            id — what a search hit prints." },
        "id": { "type": "string", "description": "A chunk id: that chunk's lines." },
        "context": { "type": "integer", "minimum": 0, "description": "Lines added before and \
            after (default 0; the daemon caps it)." },
        "glob": { "type": "string", "description": "List the indexed files this path glob \
            names (`crates/**/*.rs`, `**/*.md`) with language and chunk count." }
    });
    (description.into(), properties, vec![])
}

fn projects() -> Spec {
    let description = "The indexed projects. list (default): each one with its root, model, \
        languages, chunk count, freshness, scopes and default intent. status: one project's \
        counts, model health and what is wrong. register: index a directory, keeping whatever \
        its manifest and its earlier registration declared. intents: the intents the project's \
        searches accept, with the legs of each workflow.";
    let properties = json!({
        "action": { "type": "string", "enum": PROJECT_ACTIONS },
        "project": project_property(),
        "root": { "type": "string", "description": "register: absolute host path of the \
            project root." },
        "id": { "type": "string", "description": "register: the id to give it; derived from \
            the directory name when omitted." },
        "name": { "type": "string", "description": "register: a display name." },
        "extensions": { "type": "array", "items": { "type": "string" }, "description":
            "register: file extensions to index, without the dot; omitted keeps the project's \
            current list, or every language findery knows for a new one." }
    });
    (description.into(), properties, vec![])
}

fn sync() -> Spec {
    let description = "Bring the index up to date with the files. Incremental by default and \
        nearly free when nothing changed; pass `paths` right after editing files. Waits up to \
        `wait` seconds and answers the report, or the live progress and a job id when the sync \
        outlives the wait — call again with `job` to follow it.";
    let properties = json!({
        "project": project_property(),
        "mode": { "type": "string", "enum": SYNC_MODES, "description": "full re-reads \
            everything; the vector cache still makes unchanged chunks free." },
        "paths": { "type": "array", "items": { "type": "string" }, "description":
            "Project-relative files to refresh, after you edited them." },
        "wait": { "type": "integer", "minimum": 0, "maximum": 120, "description": "Seconds to \
            wait for the result (default 20; 0 answers at once with the job)." },
        "job": { "type": "string", "description": "Follow a sync an earlier call left running." }
    });
    (description.into(), properties, vec![])
}

fn note() -> Spec {
    let description = "Add a document to the index from text — a decision, a summary, a \
        generated doc; no file needs to exist — or remove one. Notes are never pruned by a sync.";
    let properties = json!({
        "action": { "type": "string", "enum": NOTE_ACTIONS, "description": "add (default) \
            or remove." },
        "project": project_property(),
        "path": { "type": "string", "description": "Project-relative path, also the note's \
            identity (e.g. notes/decisions.md); its extension picks the chunker." },
        "text": { "type": "string", "description": "add: the content." }
    });
    (description.into(), properties, vec!["path"])
}

/// The built-in intent names, locate first, as the library orders them.
pub fn intent_names() -> Vec<String> {
    IntentLibrary::builtin()
        .map(|library| library.names().map(str::to_string).collect())
        .unwrap_or_default()
}

/// One line per intent, grouped by kind: what an agent picks from.
fn intent_guide() -> String {
    let Ok(library) = IntentLibrary::builtin() else {
        return "A named retrieval task.".to_string();
    };
    let group = |kind: IntentKind| {
        library
            .iter()
            .filter(|intent| intent.kind == kind)
            .map(|intent| format!("{}: {}", intent.name, intent.summary))
            .collect::<Vec<_>>()
            .join(" | ")
    };
    format!(
        "The task the search serves; the right one measurably improves ranking. LOCATE (one \
         search) — {}. WORKFLOW (several searches, grouped by leg) — {}.",
        group(IntentKind::Locate),
        group(IntentKind::Workflow)
    )
}

/// Every language name a filter accepts.
pub fn langs() -> Vec<&'static str> {
    Lang::ALL.iter().map(Lang::as_str).collect()
}

/// Every chunk kind a filter accepts.
pub fn kinds() -> Vec<&'static str> {
    ChunkKind::ALL.iter().map(ChunkKind::as_str).collect()
}

/// Every search mode.
pub fn modes() -> Vec<&'static str> {
    SearchMode::ALL.iter().map(SearchMode::as_str).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> Vec<Value> {
        catalogue()["tools"].as_array().expect("array").clone()
    }

    #[test]
    fn the_catalogue_is_exactly_the_five_tools_each_with_a_strict_schema() {
        let listed: Vec<String> = tools()
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .collect();
        assert_eq!(listed, TOOLS);
        for tool in tools() {
            let schema = &tool["inputSchema"];
            assert_eq!(schema["type"], "object", "{tool}");
            assert_eq!(schema["additionalProperties"], false, "{tool}");
            let text = tool["description"].as_str().expect("description");
            assert!(text.len() > 80, "{tool}");
            assert!(!text.contains("  "), "no embedded run of spaces: {text}");
        }
    }

    #[test]
    fn every_closed_list_is_the_vocabulary_of_core() {
        let search = &tools()[0]["inputSchema"]["properties"];
        let listed = |v: &Value| -> Vec<String> {
            v.as_array()
                .expect("enum")
                .iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        };
        let langs: Vec<&str> = Lang::ALL.iter().map(Lang::as_str).collect();
        assert_eq!(listed(&search["langs"]["items"]["enum"]), langs);
        let kinds: Vec<&str> = ChunkKind::ALL.iter().map(ChunkKind::as_str).collect();
        assert_eq!(listed(&search["kinds"]["items"]["enum"]), kinds);
        assert_eq!(listed(&search["intent"]["enum"]), intent_names());
        assert!(intent_names().contains(&"doc-sync".to_string()));
        let guide = search["intent"]["description"].as_str().expect("guide");
        assert!(
            guide.contains("review: ") && guide.contains("usage: "),
            "{guide}"
        );
    }

    #[test]
    fn the_declared_arguments_are_the_schema_properties() {
        let read = declared("read").expect("read exists");
        assert!(read.contains(&"locator".to_string()) && read.contains(&"glob".to_string()));
        assert!(declared("search_code").is_none(), "the old names are gone");
    }
}
