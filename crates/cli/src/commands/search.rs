//! `findery search`: the request the arguments describe, and the hits printed.

use crate::args::Scope;
use crate::ui::render;
use crate::workspace::scope::{parse_mode, resolve};
use findery_client::Client;
use findery_core::Result;
use findery_core::embedding::prompt::QueryTask;
use findery_core::model::{ChunkKind, Lang, SearchFilter, SearchRequest};

/// What `findery search` was asked for, past the project.
pub(crate) struct Options {
    pub(crate) query: Vec<String>,
    pub(crate) k: usize,
    pub(crate) mode: Option<String>,
    pub(crate) task: Option<String>,
    pub(crate) instruction: Option<String>,
    pub(crate) search_scope: Option<String>,
    pub(crate) paths: Vec<String>,
    pub(crate) langs: Vec<String>,
    pub(crate) kinds: Vec<String>,
    pub(crate) max_chars: Option<usize>,
}

pub(crate) async fn run(
    client: &Client,
    scope: &Scope,
    options: Options,
    json: bool,
) -> Result<()> {
    let request = SearchRequest {
        project: resolve(client, scope).await?,
        query: options.query.join(" "),
        k: Some(options.k),
        mode: options.mode.as_deref().and_then(parse_mode),
        filter: SearchFilter {
            paths: options.paths,
            langs: options
                .langs
                .iter()
                .filter_map(|l| Lang::parse(l))
                .collect(),
            kinds: options
                .kinds
                .iter()
                .filter_map(|k| ChunkKind::parse(k))
                .collect(),
            symbol_contains: None,
        },
        max_snippet_chars: options.max_chars,
        task: options.task.as_deref().and_then(QueryTask::parse),
        instruction: options.instruction,
        scope: options.search_scope,
    };
    let hits = client.search(&request).await?;
    if json {
        return render::json(&hits);
    }
    render::hits(&hits);
    Ok(())
}
