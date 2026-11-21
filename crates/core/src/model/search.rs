//! Search: how a caller states a query, and what comes back.
//!
//! INTENT: split out of `model/mod.rs` so that "what an indexed thing is" and
//! "how you ask for one" stay separate concerns (CONV-029).

use serde::{Deserialize, Serialize};

use super::{Chunk, ChunkKind, Lang};
use crate::ids::{ChunkId, ProjectId};

/// Recall strategy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// Cosine similarity only.
    Vector,
    /// BM25 (FTS5) only — the right tool for an exact identifier.
    Keyword,
    /// Reciprocal-rank fusion of both lists. The default: on code it beats
    /// either one taken alone.
    #[default]
    Hybrid,
}

impl SearchMode {
    /// Every mode — what a refusal of an unknown one lists.
    pub const ALL: &'static [SearchMode] = &[Self::Hybrid, Self::Vector, Self::Keyword];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Vector => "vector",
            Self::Keyword => "keyword",
            Self::Hybrid => "hybrid",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "vector" => Some(Self::Vector),
            "keyword" => Some(Self::Keyword),
            "hybrid" => Some(Self::Hybrid),
            _ => None,
        }
    }
}

/// Restrictions applied BEFORE scoring.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchFilter {
    /// Path globs (`crates/core/**`, `**/*.md`).
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub langs: Vec<Lang>,
    #[serde(default)]
    pub kinds: Vec<ChunkKind>,
    /// Case-insensitive substring the symbol must contain.
    #[serde(default)]
    pub symbol_contains: Option<String>,
}

impl SearchFilter {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
            && self.langs.is_empty()
            && self.kinds.is_empty()
            && self.symbol_contains.is_none()
    }

    /// This filter with `on_top` applied over it, dimension by dimension: a
    /// dimension the request names replaces the scope's, one it leaves empty
    /// keeps it.
    ///
    /// Intersecting the two instead would turn `--scope api --lang markdown` into
    /// a search that matches nothing, and a filter nobody can widen is one
    /// nobody can escape.
    pub fn layer(&self, on_top: &SearchFilter) -> SearchFilter {
        SearchFilter {
            paths: if on_top.paths.is_empty() {
                self.paths.clone()
            } else {
                on_top.paths.clone()
            },
            langs: if on_top.langs.is_empty() {
                self.langs.clone()
            } else {
                on_top.langs.clone()
            },
            kinds: if on_top.kinds.is_empty() {
                self.kinds.clone()
            } else {
                on_top.kinds.clone()
            },
            symbol_contains: on_top
                .symbol_contains
                .clone()
                .or_else(|| self.symbol_contains.clone()),
        }
    }
}

/// A search as a caller states it. Shared verbatim by the HTTP API, the Rust
/// client and the MCP tools: one request type, no translation layer.
///
/// INTENT: `deny_unknown_fields`, because a misspelled knob (`instructions`,
/// `intnet`) is a search that silently ignores what the caller asked for.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchRequest {
    pub project: ProjectId,
    pub query: String,
    /// Number of hits; `None` falls back to `search.default_k`.
    #[serde(default)]
    pub k: Option<usize>,
    #[serde(default)]
    pub mode: Option<SearchMode>,
    #[serde(default)]
    pub filter: SearchFilter,
    /// Per-hit text budget; `None` falls back to `search.max_snippet_chars`.
    /// The most direct lever an MCP client has on its own token spend.
    #[serde(default)]
    pub max_snippet_chars: Option<usize>,
    /// A legacy intent name (`code`, `symbol`, `usage`, `docs`, `test`,
    /// `config`, `error` and their aliases). Means exactly what the same name
    /// means as `intent`.
    #[serde(default)]
    pub task: Option<crate::intent::QueryTask>,
    /// A named intent from the library — built-in, or defined by the daemon or
    /// the project — by name or alias. An unknown name is refused with the list
    /// of known ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    /// Free-form instruction, overriding the intent's sentence and every
    /// default.
    #[serde(default)]
    pub instruction: Option<String>,
    /// Sends the query to the model exactly as written: no instruction, no
    /// template. Beats every instruction.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub raw_query: bool,
    /// A chunk whose STORED vector the `similar` legs search with, by locator
    /// (`path:L12-L48`) or chunk id: zero embedding, the chunk itself left out.
    /// With no intent named, the search runs `similar`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub like: Option<String>,
    /// A named restriction the project declared, narrowing the SAME index.
    ///
    /// Sent as a name, not an expanded filter: the daemon holds the project's
    /// definition, so every client — CLI, MCP bridge, script — means the same
    /// filter by the same name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl SearchRequest {
    /// Minimal request: project, text, everything else from configuration.
    pub fn new(project: ProjectId, query: impl Into<String>) -> Self {
        Self {
            project,
            query: query.into(),
            k: None,
            mode: None,
            filter: SearchFilter::default(),
            max_snippet_chars: None,
            task: None,
            intent: None,
            instruction: None,
            raw_query: false,
            like: None,
            scope: None,
        }
    }

    /// Restricts the search to a scope the project declared.
    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }

    /// Picks a legacy intent name.
    pub fn with_task(mut self, task: crate::intent::QueryTask) -> Self {
        self.task = Some(task);
        self
    }

    /// Picks a named intent.
    pub fn with_intent(mut self, intent: impl Into<String>) -> Self {
        self.intent = Some(intent.into());
        self
    }
}

/// A (chunk, score) pair returned by a `VectorStore`.
#[derive(Clone, Debug, PartialEq)]
pub struct ScoredChunk {
    pub id: ChunkId,
    pub score: f32,
}

/// A complete search result, ready to be cited by an agent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchHit {
    pub chunk: Chunk,
    /// Fused score; monotonically decreasing across the returned list.
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector_score: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyword_score: Option<f32>,
    /// Absolute host-side path — what the user can actually open.
    pub host_path: String,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::embedding::prompt::DEFAULT_QUERY_INSTRUCTION;
    use crate::intent::{DEFAULT_MAX_INSTRUCTION_CHARS, IntentLibrary, PlanContext, QueryTask};
    use crate::model::ProjectSpec;

    fn request() -> SearchRequest {
        SearchRequest::new(
            ProjectId::new("demo").expect("id"),
            "where is retry handled",
        )
    }

    /// The instruction the plan gives `req`'s one leg, with no project default.
    fn alone(req: &SearchRequest) -> Option<String> {
        let ctx = PlanContext {
            library: IntentLibrary::builtin().expect("the shipped library loads"),
            scope: &SearchFilter::default(),
            project_instruction: None,
            project_intent: None,
            langs: &[],
            max_instruction_chars: DEFAULT_MAX_INSTRUCTION_CHARS,
        };
        let plan = req.resolve_plan(&ctx).expect("resolves");
        plan.legs.into_iter().next().and_then(|leg| leg.instruction)
    }

    #[test]
    fn every_mode_round_trips_through_its_name_and_its_serde_form() {
        for mode in SearchMode::ALL {
            assert_eq!(SearchMode::parse(mode.as_str()), Some(*mode));
            let wire = serde_json::to_string(mode).expect("serialize");
            assert_eq!(wire, format!("\"{}\"", mode.as_str()));
        }
        assert_eq!(SearchMode::parse("semantic"), None);
    }

    #[test]
    fn with_no_hint_the_daemon_default_is_used() {
        assert_eq!(alone(&request()), None);
    }

    #[test]
    fn a_preset_supplies_its_instruction() {
        let req = request().with_task(QueryTask::Symbol);
        assert_eq!(alone(&req).as_deref(), QueryTask::Symbol.instruction());
        assert_ne!(alone(&req).as_deref(), Some(DEFAULT_QUERY_INSTRUCTION));
    }

    #[test]
    fn free_text_beats_the_preset() {
        let mut req = request().with_task(QueryTask::Docs);
        req.instruction = Some("Given a migration question, retrieve the upgrade notes.".into());
        assert!(alone(&req).expect("instruction").contains("upgrade notes"));
    }

    #[test]
    fn a_blank_instruction_does_not_shadow_the_preset() {
        let mut req = request().with_task(QueryTask::Test);
        req.instruction = Some("   ".into());
        assert_eq!(alone(&req).as_deref(), QueryTask::Test.instruction());
    }

    #[test]
    fn a_misspelled_field_is_refused_rather_than_ignored() {
        let err = serde_json::from_str::<SearchRequest>(
            r#"{"project":"demo","query":"x","instructions":"typo"}"#,
        )
        .expect_err("unknown field");
        assert!(err.to_string().contains("instructions"), "{err}");
        let back: SearchRequest = serde_json::from_str(
            r#"{"project":"demo","query":"x","intent":"callers","raw_query":true,"like":"a.rs:L1-L2"}"#,
        )
        .expect("the new fields parse");
        assert_eq!(back.intent.as_deref(), Some("callers"));
        assert!(back.raw_query);
        assert_eq!(back.like.as_deref(), Some("a.rs:L1-L2"));
    }

    #[test]
    fn the_task_survives_a_json_round_trip() {
        let req = request().with_task(QueryTask::Usage);
        let wire = serde_json::to_string(&req).expect("serialize");
        assert!(wire.contains("\"task\":\"usage\""), "{wire}");
        let back: SearchRequest = serde_json::from_str(&wire).expect("deserialize");
        assert_eq!(back.task, Some(QueryTask::Usage));
    }

    #[test]
    fn a_request_without_task_still_parses() {
        let back: SearchRequest =
            serde_json::from_str(r#"{"project":"demo","query":"x"}"#).expect("deserialize");
        assert_eq!(back.task, None);
        assert_eq!(alone(&back), None);
        let wire = serde_json::to_string(&back).expect("serialize");
        assert!(
            !wire.contains("raw_query") && !wire.contains("intent") && !wire.contains("like"),
            "an unset knob stays off the wire: {wire}"
        );
    }

    /// A dimension the request names replaces the scope's; a dimension it leaves
    /// empty keeps the scope's. Intersecting them instead would make
    /// `--scope api --lang markdown` a search that matches nothing.
    #[test]
    fn a_scope_supplies_what_the_request_leaves_unsaid() {
        let scope = SearchFilter {
            paths: vec!["crates/server/**".into()],
            langs: vec![Lang::Rust],
            ..SearchFilter::default()
        };

        let untouched = scope.layer(&SearchFilter::default());
        assert_eq!(untouched, scope, "nothing said leaves the scope whole");

        let overridden = scope.layer(&SearchFilter {
            langs: vec![Lang::Markdown],
            ..SearchFilter::default()
        });
        assert_eq!(overridden.paths, scope.paths, "the scope's paths survive");
        assert_eq!(overridden.langs, vec![Lang::Markdown], "the request wins");
    }

    /// A scope restricts. It must never turn a narrow request into a wider one —
    /// which is what an intersection would do in reverse, and what a naive union
    /// would do outright.
    #[test]
    fn a_scope_never_widens_what_the_request_asked_for() {
        let scope = SearchFilter {
            paths: vec!["docs/**".into()],
            ..SearchFilter::default()
        };
        let request = SearchFilter {
            kinds: vec![ChunkKind::Function],
            ..SearchFilter::default()
        };
        let merged = scope.layer(&request);
        assert_eq!(merged.paths, vec!["docs/**"]);
        assert_eq!(merged.kinds, vec![ChunkKind::Function]);
        assert!(!merged.is_empty());
    }

    /// A scope name the project does not declare names the ones it does: the
    /// caller's next move is to pick one, and it cannot guess.
    #[test]
    fn an_unknown_scope_lists_the_ones_that_exist() {
        let spec = ProjectSpec {
            scopes: BTreeMap::from([("api".to_string(), SearchFilter::default())]),
            ..ProjectSpec::default()
        };
        let err = spec.scope("api2").expect_err("must be refused");
        assert!(err.to_string().contains("api"), "{err}");

        assert!(spec.scope("api").is_ok());

        let none = ProjectSpec::default();
        let err = none.scope("api").expect_err("must be refused");
        assert!(err.to_string().contains("manifest"), "{err}");
    }
}
