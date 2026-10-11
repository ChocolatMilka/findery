//! What the bridge advertises, as `tools/list` returns it.
//!
//! Held apart from the code that answers it: a description is edited for a
//! reader and an implementation for the compiler, and mixing the two is how a
//! tool ends up documented as something it no longer does.

use serde_json::{Value, json};

/// The tool catalogue, as `tools/list` returns it.
pub fn catalogue() -> Value {
    json!({ "tools": [
        tool(
            "search_code",
            "Semantic + keyword search over an indexed codebase. USE THIS FIRST when locating \
             code, docs or a concept — it is cheaper and more accurate than grepping or reading \
             files. Returns ranked excerpts with exact line ranges and openable host paths.",
            json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Project id, from list_projects." },
                    "query": { "type": "string", "description": "Natural-language question or an exact symbol." },
                    "k": { "type": "integer", "minimum": 1, "maximum": 50, "description": "Number of excerpts (default 8)." },
                    "mode": { "type": "string", "enum": ["hybrid", "vector", "keyword"], "description": "Default hybrid. Use keyword for an exact identifier, vector for a fuzzy concept." },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Path globs, e.g. crates/core/**." },
                    "langs": { "type": "array", "items": { "type": "string", "enum": ["rust", "markdown", "toml", "other"] } },
                    "kinds": { "type": "array", "items": { "type": "string" }, "description": "function, method, struct, enum, trait, impl, module, test, doc_section…" },
                    "symbol_contains": { "type": "string", "description": "Restrict to symbols containing this substring." },
                    "task": {
                        "type": "string",
                        "enum": ["code", "symbol", "usage", "docs", "test", "config", "error"],
                        "description": "Retrieval intent, passed to the instruction-aware embedding model. \
                                        `code` (default) for a general question, `symbol` when you have a name and want its \
                                        definition, `usage` for call sites, `docs` for rationale, `test` for the tests \
                                        covering a behaviour, `config` for a setting or env var, `error` for a message or \
                                        panic. Picking the right one measurably improves the ranking."
                    },
                    "instruction": { "type": "string", "description": "Free-form instruction for the embedding model, overriding `task` and the project's default. Describe the TASK, not the query. It is the first half of the pair Qwen reads as \"Instruct: …\\nQuery: …\" — the second half is `query`, and both are sent." },
                    "scope": { "type": "string", "description": "A named restriction the project declares (e.g. a subtree, a language). Applied before scoring on the same index. An unknown name is refused and lists the ones that exist." },
                    "max_snippet_chars": { "type": "integer", "description": "Per-excerpt budget. Lower it to spend fewer tokens." }
                },
                "required": ["project", "query"]
            }),
        ),
        tool(
            "list_projects",
            "List the indexed projects with their roots. Call this when unsure which project id to use.",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "index_status",
            "File, chunk and vector counts for a project, plus the model in use and the last sync time.",
            json!({
                "type": "object",
                "properties": { "project": { "type": "string" } },
                "required": ["project"]
            }),
        ),
        tool(
            "sync_project",
            "Re-synchronize a project with the filesystem. Incremental by default and nearly free \
             when nothing changed. Pass paths to refresh specific files after editing them. On a \
             cold index this takes minutes: pass wait=false and follow the job with job_status.",
            json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string" },
                    "mode": { "type": "string", "enum": ["incremental", "full"] },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Project-relative paths to refresh." },
                    "wait": { "type": "boolean", "description": "Block until done (default true). Set false on a cold index." }
                },
                "required": ["project"]
            }),
        ),
        tool(
            "job_status",
            "Where a running indexing job is, and what it has done so far. Call it after a \
             sync_project that returned a job id instead of waiting for it.",
            json!({
                "type": "object",
                "properties": {
                    "job": { "type": "string", "description": "Job id, as sync_project returned it." }
                },
                "required": ["job"]
            }),
        ),
        tool(
            "register_project",
            "Index a new directory. The root is a host path; the daemon translates it through its mounts.",
            json!({
                "type": "object",
                "properties": {
                    "root": { "type": "string", "description": "Absolute host path of the project root." },
                    "id": { "type": "string", "description": "Slug; derived from the directory name when omitted." },
                    "name": { "type": "string" },
                    "extensions": { "type": "array", "items": { "type": "string" }, "description": "Defaults to rs and md." }
                },
                "required": ["root"]
            }),
        ),
        tool(
            "add_document",
            "Add or refresh one document. With `text`, the content is indexed directly and no file \
             needs to exist — useful for notes, decisions or generated docs.",
            json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string" },
                    "path": { "type": "string", "description": "Project-relative path, also the document's identity." },
                    "text": { "type": "string", "description": "Inline content; omit to read from disk." }
                },
                "required": ["project", "path"]
            }),
        ),
        tool(
            "remove_document",
            "Remove one document from the index, with its chunks and vectors.",
            json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string" },
                    "path": { "type": "string" }
                },
                "required": ["project", "path"]
            }),
        ),
        tool(
            "export_vectors",
            "Export a project's vectors and metadata to a directory (jsonl, json or binary float32).",
            json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string" },
                    "format": { "type": "string", "enum": ["jsonl", "json", "binary"] },
                    "dir": { "type": "string", "description": "Destination directory, host-side." }
                },
                "required": ["project"]
            }),
        ),
    ]})
}

fn tool(name: &str, description: &str, schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": schema })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogue_is_valid_and_every_tool_declares_a_schema() {
        let cat = catalogue();
        let tools = cat["tools"].as_array().expect("array");
        assert!(tools.len() >= 8);
        for t in tools {
            assert!(t["name"].as_str().is_some_and(|n| !n.is_empty()), "{t}");
            assert!(
                t["description"].as_str().is_some_and(|d| d.len() > 20),
                "{t}"
            );
            assert_eq!(t["inputSchema"]["type"], "object", "{t}");
        }
    }
}
