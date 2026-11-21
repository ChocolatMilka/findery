//! `search`: the arguments become one [`SearchRequest`], the answer one text.

use findery_core::Result;
use findery_core::ids::ProjectId;
use findery_core::intent::{IntentKind, IntentLibrary};
use findery_core::model::{ChunkKind, Lang, SearchFilter, SearchMode, SearchRequest};

use super::as_usize;
use crate::Session;
use crate::args::Args;
use crate::catalogue::{self, DETAILS};
use crate::render::search::{Detail, render};

/// Hits per search when the agent says nothing: what the docs' budget assumes.
const DEFAULT_K: usize = 8;
/// Per-excerpt cap of a compact answer: 8 of them stay near 3k tokens.
const COMPACT_SNIPPET_CHARS: usize = 1_200;

pub(super) async fn run(session: &Session, args: &Args<'_>) -> Result<String> {
    let asked = Asked::read(args)?;
    let project = session.project(args).await?;
    let detail = asked.detail;
    let request = asked.into_request(project.clone());
    let response = session.client().search(&request).await?;
    Ok(render(&project, &request.query, &response, detail))
}

/// Everything a search call said, read and checked before any I/O.
struct Asked {
    query: String,
    intent: Option<String>,
    instruction: Option<String>,
    raw_query: bool,
    like: Option<String>,
    mode: Option<SearchMode>,
    filter: SearchFilter,
    scope: Option<String>,
    k: Option<usize>,
    max_snippet_chars: Option<usize>,
    detail: Detail,
}

impl Asked {
    fn read(args: &Args<'_>) -> Result<Self> {
        let like = args.text("like")?;
        // A search by example needs no words; any other search does.
        let query = match like {
            Some(_) => args.text("query")?.unwrap_or_default(),
            None => args.required("query")?,
        };
        let intent = args.text("intent")?;
        let detail = args
            .choice("detail", Detail::parse, DETAILS)?
            .unwrap_or_default();
        let snippet = match detail {
            Detail::Compact => Some(COMPACT_SNIPPET_CHARS),
            Detail::Full => None,
        };
        Ok(Self {
            k: args
                .count("k")?
                .map(as_usize)
                .or_else(|| default_k(intent.as_deref())),
            max_snippet_chars: args.count("max_snippet_chars")?.map(as_usize).or(snippet),
            mode: args.choice("mode", SearchMode::parse, &catalogue::modes())?,
            filter: SearchFilter {
                paths: args.texts("paths")?,
                langs: args.choices("langs", Lang::parse, &catalogue::langs())?,
                kinds: args.choices("kinds", ChunkKind::parse, &catalogue::kinds())?,
                symbol_contains: args.text("symbol")?,
            },
            instruction: args.text("instruction")?,
            raw_query: args.flag("raw_query")?.unwrap_or(false),
            scope: args.text("scope")?,
            query,
            intent,
            like,
            detail,
        })
    }

    fn into_request(self, project: ProjectId) -> SearchRequest {
        SearchRequest {
            k: self.k,
            mode: self.mode,
            filter: self.filter,
            max_snippet_chars: self.max_snippet_chars,
            intent: self.intent,
            instruction: self.instruction,
            raw_query: self.raw_query,
            like: self.like,
            scope: self.scope,
            ..SearchRequest::new(project, self.query)
        }
    }
}

/// A workflow keeps each leg's own `k`; anything else gets the default.
///
/// An intent the built-in library does not know may be the project's own; the
/// daemon resolves it — and refuses it, listing the known ones, if it is not.
fn default_k(intent: Option<&str>) -> Option<usize> {
    let library = IntentLibrary::builtin().ok()?;
    let workflow = intent
        .and_then(|name| library.find(name))
        .is_some_and(|i| i.kind == IntentKind::Workflow);
    (!workflow).then_some(DEFAULT_K)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn asked(raw: Value) -> Result<Asked> {
        let declared = catalogue::declared("search").expect("search");
        let args = Args::new("search", &raw, &declared)?;
        Asked::read(&args)
    }

    #[test]
    fn a_plain_search_gets_the_budgeted_defaults() {
        let a = asked(json!({ "query": "retry" })).expect("valid");
        assert_eq!(a.k, Some(DEFAULT_K));
        assert_eq!(a.max_snippet_chars, Some(COMPACT_SNIPPET_CHARS));
        let full = asked(json!({ "query": "retry", "detail": "full" })).expect("valid");
        assert_eq!(full.max_snippet_chars, None, "the daemon's own cap");
    }

    #[test]
    fn a_workflow_keeps_the_k_of_each_of_its_legs() {
        let a = asked(json!({ "query": "Indexer::sync", "intent": "review" })).expect("valid");
        assert_eq!(a.k, None);
        let alias = asked(json!({ "query": "x", "intent": "impact" })).expect("alias");
        assert_eq!(alias.k, None, "an alias of a workflow is a workflow");
    }

    #[test]
    fn an_unknown_language_kind_mode_or_detail_is_refused_listing_the_valid_ones() {
        let err = asked(json!({ "query": "x", "langs": ["rust", "cobol"] }))
            .err()
            .expect("lang");
        assert!(
            err.to_string().contains("`cobol`; valid: rust, markdown"),
            "{err}"
        );
        let err = asked(json!({ "query": "x", "kinds": ["class"] }))
            .err()
            .expect("kind");
        assert!(err.to_string().contains("doc_section"), "{err}");
        let err = asked(json!({ "query": "x", "mode": "semantic" }))
            .err()
            .expect("mode");
        assert!(err.to_string().contains("hybrid, vector, keyword"), "{err}");
        let err = asked(json!({ "query": "x", "detail": "brief" }))
            .err()
            .expect("detail");
        assert!(err.to_string().contains("compact, full"), "{err}");
    }

    #[test]
    fn a_search_needs_words_unless_it_searches_by_example() {
        assert!(asked(json!({})).is_err());
        let by_example = asked(json!({ "like": "src/a.rs:L1-L9" })).expect("like");
        assert!(by_example.query.is_empty());
    }

    #[test]
    fn the_filters_reach_the_request_as_typed_values() {
        let a = asked(json!({
            "query": "x", "langs": ["ts"], "kinds": ["method"], "symbol": "retry",
            "paths": ["src/**"], "scope": "api", "k": 3, "raw_query": true
        }))
        .expect("valid");
        let request = a.into_request(ProjectId::new("axle").expect("id"));
        assert_eq!(request.filter.langs, [Lang::JavaScript]);
        assert_eq!(request.filter.kinds, [ChunkKind::Method]);
        assert_eq!(request.filter.symbol_contains.as_deref(), Some("retry"));
        assert_eq!(request.k, Some(3));
        assert!(request.raw_query);
        assert_eq!(request.scope.as_deref(), Some("api"));
    }
}
