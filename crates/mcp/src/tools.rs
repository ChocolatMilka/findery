//! The tools exposed to Claude Code, and how their answers are rendered.
//!
//! INTENT: the rendering IS the product here. A tool that returns raw JSON
//! spends a third of its tokens on punctuation and field names the model does
//! not need. Every result below is laid out to be read once and cited: a
//! `path:line-line` locator the agent can quote, a host path it can open, and
//! the excerpt itself — nothing else.

use findery_client::Client;
use findery_core::api::{AddDocument, ExportRequest, RegisterProject, SyncRequest};
use findery_core::ids::ProjectId;
use findery_core::model::{ChunkKind, Lang, SearchFilter, SearchHit, SearchMode, SearchRequest};
use findery_core::prompt::QueryTask;
use findery_core::{Error, Result};
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
                    "instruction": { "type": "string", "description": "Free-form instruction for the embedding model, overriding `task`. Describe the TASK, not the query." },
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
             when nothing changed. Pass paths to refresh specific files after editing them.",
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

/// Dispatches one `tools/call`, returning the text to show the model.
pub async fn call(client: &Client, name: &str, args: &Value) -> Result<String> {
    match name {
        "search_code" => search_code(client, args).await,
        "list_projects" => list_projects(client).await,
        "index_status" => index_status(client, args).await,
        "sync_project" => sync_project(client, args).await,
        "register_project" => register_project(client, args).await,
        "add_document" => add_document(client, args).await,
        "remove_document" => remove_document(client, args).await,
        "export_vectors" => export_vectors(client, args).await,
        other => Err(Error::Invalid(format!("unknown tool `{other}`"))),
    }
}

// ---------------------------------------------------------------------------

async fn search_code(client: &Client, args: &Value) -> Result<String> {
    let project = project_arg(args)?;
    let query = string_arg(args, "query")?;
    let filter = SearchFilter {
        paths: string_list(args, "paths"),
        langs: string_list(args, "langs")
            .iter()
            .filter_map(|s| Lang::parse(s))
            .collect(),
        kinds: string_list(args, "kinds")
            .iter()
            .filter_map(|s| ChunkKind::parse(s))
            .collect(),
        symbol_contains: args
            .get("symbol_contains")
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    let request = SearchRequest {
        project: project.clone(),
        query: query.clone(),
        k: args
            .get("k")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .or(Some(8)),
        mode: args
            .get("mode")
            .and_then(Value::as_str)
            .and_then(parse_mode),
        filter,
        max_snippet_chars: args
            .get("max_snippet_chars")
            .and_then(Value::as_u64)
            .map(|v| v as usize),
        task: args
            .get("task")
            .and_then(Value::as_str)
            .and_then(QueryTask::parse),
        instruction: args
            .get("instruction")
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    let hits = client.search(&request).await?;
    Ok(render_hits(&project, &query, &hits))
}

/// The layout the agent actually reads.
fn render_hits(project: &ProjectId, query: &str, hits: &[SearchHit]) -> String {
    if hits.is_empty() {
        return format!(
            "No match in `{project}` for {query:?}.\n\
             Try a broader query, mode=\"keyword\" for an exact identifier, or sync_project if \
             the code is newer than the index."
        );
    }
    let mut out = format!("{} excerpt(s) in `{project}` for {query:?}\n", hits.len());
    for (i, hit) in hits.iter().enumerate() {
        let kind = hit.chunk.kind.as_str();
        let symbol = hit
            .chunk
            .breadcrumb
            .as_deref()
            .or(hit.chunk.symbol.as_deref())
            .unwrap_or("-");
        out.push_str(&format!(
            "\n[{}] {} · {kind} `{symbol}` · {:.3}\n{}\n```{}\n{}\n```\n",
            i + 1,
            hit.chunk.locator(),
            hit.score,
            hit.host_path,
            hit.chunk.lang.as_str(),
            hit.chunk.text.trim_end(),
        ));
    }
    out
}

async fn list_projects(client: &Client) -> Result<String> {
    let projects = client.list_projects().await?;
    if projects.is_empty() {
        return Ok(
            "No project indexed yet. Use register_project with an absolute host path.".to_string(),
        );
    }
    let mut out = format!("{} project(s):\n", projects.len());
    for p in projects {
        out.push_str(&format!(
            "- `{}` — {} — {} [{}]\n",
            p.id,
            p.name,
            p.host_root,
            p.spec.extensions.join(", ")
        ));
    }
    Ok(out)
}

async fn index_status(client: &Client, args: &Value) -> Result<String> {
    let stats = client.stats(&project_arg(args)?).await?;
    Ok(format!(
        "project `{}`\n  files: {}\n  chunks: {}\n  vectors: {}\n  model: {} ({} dims)\n  \
         store: {}\n  last sync: {}\n  bucket on disk: {:.1} MB",
        stats.project,
        stats.files,
        stats.chunks,
        stats.vectors,
        stats.model,
        stats.dim,
        stats.store_backend,
        stats
            .last_sync_at
            .map(|t| t.to_string())
            .unwrap_or_else(|| "never".into()),
        stats.disk_bytes as f64 / 1e6,
    ))
}

async fn sync_project(client: &Client, args: &Value) -> Result<String> {
    let project = project_arg(args)?;
    let paths = string_list(args, "paths");
    let request = SyncRequest {
        mode: args.get("mode").and_then(Value::as_str).map(str::to_string),
        paths: if paths.is_empty() { None } else { Some(paths) },
        wait: args.get("wait").and_then(Value::as_bool).unwrap_or(true),
    };
    match client.sync(&project, &request).await? {
        findery_core::api::SyncResponse::Completed { report } => Ok(format!(
            "sync `{}` ({}) in {} ms\n  scanned {} · unchanged {} · added {} · updated {} · \
             removed {} · skipped {}\n  chunks +{} -{}\n  embeddings computed {} · cached {}{}",
            report.project,
            report.mode,
            report.duration_ms,
            report.files_scanned,
            report.files_unchanged,
            report.files_added,
            report.files_updated,
            report.files_removed,
            report.files_skipped,
            report.chunks_written,
            report.chunks_deleted,
            report.embeddings_computed,
            report.embeddings_cached,
            render_warnings(&report.warnings),
        )),
        findery_core::api::SyncResponse::Accepted { job } => Ok(format!(
            "sync `{}` started in the background, job `{}` (state: {:?}). \
             Poll with index_status, or call again with wait=true.",
            job.project, job.id, job.state
        )),
    }
}

fn render_warnings(warnings: &[String]) -> String {
    if warnings.is_empty() {
        return String::new();
    }
    let shown = warnings
        .iter()
        .take(5)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n    ");
    let rest = warnings.len().saturating_sub(5);
    let more = if rest > 0 {
        format!("\n    … and {rest} more")
    } else {
        String::new()
    };
    format!("\n  warnings:\n    {shown}{more}")
}

async fn register_project(client: &Client, args: &Value) -> Result<String> {
    let request = RegisterProject {
        root: string_arg(args, "root")?,
        id: args.get("id").and_then(Value::as_str).map(str::to_string),
        name: args.get("name").and_then(Value::as_str).map(str::to_string),
        extensions: {
            let e = string_list(args, "extensions");
            if e.is_empty() { None } else { Some(e) }
        },
        ..RegisterProject::default()
    };
    let project = client.register(&request).await?;
    Ok(format!(
        "registered `{}` at {} (extensions: {}). Run sync_project to build the index.",
        project.id,
        project.host_root,
        project.spec.extensions.join(", ")
    ))
}

async fn add_document(client: &Client, args: &Value) -> Result<String> {
    let project = project_arg(args)?;
    let request = AddDocument {
        path: string_arg(args, "path")?,
        text: args.get("text").and_then(Value::as_str).map(str::to_string),
    };
    let path = request.path.clone();
    let report = client.add_document(&project, &request).await?;
    Ok(format!(
        "indexed `{path}` in `{project}`: {} chunk(s), {} embedded, {} from cache",
        report.chunks_written, report.embeddings_computed, report.embeddings_cached
    ))
}

async fn remove_document(client: &Client, args: &Value) -> Result<String> {
    let project = project_arg(args)?;
    let path = string_arg(args, "path")?;
    let removed = client.remove_document(&project, &path).await?;
    Ok(format!(
        "removed `{path}` from `{project}`: {removed} chunk(s) dropped"
    ))
}

async fn export_vectors(client: &Client, args: &Value) -> Result<String> {
    let project = project_arg(args)?;
    let request = ExportRequest {
        format: args
            .get("format")
            .and_then(Value::as_str)
            .map(str::to_string),
        dir: args.get("dir").and_then(Value::as_str).map(str::to_string),
    };
    let report = client.export(&project, &request).await?;
    Ok(format!(
        "exported {} vector(s) of {} chunk(s), {} dims, format {} → {}\n  files: {}",
        report.vectors,
        report.chunks,
        report.dim,
        report.format,
        report.directory,
        report.files.join(", ")
    ))
}

// ---------------------------------------------------------------------------

fn project_arg(args: &Value) -> Result<ProjectId> {
    ProjectId::new(string_arg(args, "project")?)
}

fn string_arg(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| Error::Invalid(format!("`{key}` is required")))
}

fn string_list(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_mode(raw: &str) -> Option<SearchMode> {
    match raw {
        "hybrid" => Some(SearchMode::Hybrid),
        "vector" => Some(SearchMode::Vector),
        "keyword" => Some(SearchMode::Keyword),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use findery_core::ids::{ChunkId, ContentHash};
    use findery_core::model::Chunk;

    fn hit(path: &str, score: f32) -> SearchHit {
        let project = ProjectId::new("demo").expect("id");
        SearchHit {
            chunk: Chunk {
                id: ChunkId::derive(&project, path, 0),
                project,
                path: path.into(),
                lang: Lang::Rust,
                ordinal: 0,
                kind: ChunkKind::Method,
                symbol: Some("retry".into()),
                breadcrumb: Some("Client > retry".into()),
                start_line: 12,
                end_line: 48,
                content_hash: ContentHash::of(b"x"),
                text: "pub fn retry() {}".into(),
            },
            score,
            vector_score: Some(score),
            keyword_score: None,
            host_path: format!("C:/demo/{path}"),
        }
    }

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

    #[test]
    fn search_results_are_citable_and_carry_an_openable_path() {
        let project = ProjectId::new("demo").expect("id");
        let text = render_hits(&project, "retry", &[hit("src/net.rs", 0.842)]);
        assert!(text.contains("src/net.rs:12-48"), "{text}");
        assert!(text.contains("C:/demo/src/net.rs"), "{text}");
        assert!(text.contains("0.842"), "{text}");
        assert!(text.contains("```rust"), "{text}");
        assert!(text.contains("Client > retry"), "{text}");
    }

    #[test]
    fn an_empty_result_tells_the_agent_what_to_try_next() {
        let project = ProjectId::new("demo").expect("id");
        let text = render_hits(&project, "nothing", &[]);
        assert!(text.contains("No match"));
        assert!(
            text.contains("keyword"),
            "must suggest the exact-match mode: {text}"
        );
        assert!(
            text.contains("sync_project"),
            "must suggest re-syncing: {text}"
        );
    }

    #[test]
    fn missing_arguments_are_refused_with_the_field_name() {
        let err = string_arg(&json!({}), "query").expect_err("must fail");
        assert!(err.to_string().contains("query"));
        assert!(string_arg(&json!({ "query": "   " }), "query").is_err());
    }

    #[test]
    fn optional_list_arguments_degrade_to_empty() {
        assert!(string_list(&json!({}), "paths").is_empty());
        assert_eq!(
            string_list(&json!({ "paths": ["a", 3, "b"] }), "paths"),
            vec!["a", "b"]
        );
    }

    #[test]
    fn warnings_are_summarized_not_dumped() {
        let many: Vec<String> = (0..9).map(|i| format!("file{i}: too big")).collect();
        let text = render_warnings(&many);
        assert!(text.contains("and 4 more"), "{text}");
        assert!(render_warnings(&[]).is_empty());
    }

    #[tokio::test]
    async fn an_unknown_tool_is_refused() {
        let client = Client::new("http://127.0.0.1:1").expect("client");
        let err = call(&client, "rm_rf", &json!({}))
            .await
            .expect_err("must fail");
        assert_eq!(err.code(), "invalid");
    }
}
