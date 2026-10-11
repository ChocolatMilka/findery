//! Search: how a caller states a query, and what comes back.
//!
//! INTENT: split out of `model/mod.rs` so that "what an indexed thing is" and
//! "how you ask for one" stay separate concerns (CONV-029).

use super::{Chunk, ChunkKind, Lang};
use crate::ids::{ChunkId, ProjectId};
use serde::{Deserialize, Serialize};

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
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Vector => "vector",
            Self::Keyword => "keyword",
            Self::Hybrid => "hybrid",
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
#[derive(Clone, Debug, Serialize, Deserialize)]
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
    /// Named retrieval intent. The embedding model is instruction-aware, and a
    /// task-tailored instruction buys 1-5% retrieval accuracy for free.
    #[serde(default)]
    pub task: Option<crate::prompt::QueryTask>,
    /// Free-form instruction, overriding both `task` and the configured default.
    #[serde(default)]
    pub instruction: Option<String>,
    /// A named restriction the project declared, narrowing the SAME index.
    ///
    /// Sent as a name, not an expanded filter: the daemon already holds the
    /// project's definition, and the MCP bridge is a different process that never
    /// reads a manifest.
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
            instruction: None,
            scope: None,
        }
    }

    /// Restricts the search to a scope the project declared.
    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }

    /// Picks a named retrieval intent.
    pub fn with_task(mut self, task: crate::prompt::QueryTask) -> Self {
        self.task = Some(task);
        self
    }

    /// The instruction this request resolves to on its own. `None` means nothing
    /// was said here — the project's default applies next, then the daemon's.
    /// See [`SearchRequest::resolve`] for the whole ladder.
    pub fn resolved_instruction(&self) -> Option<&str> {
        match (&self.instruction, self.task) {
            (Some(free), _) if !free.trim().is_empty() => Some(free),
            (_, Some(task)) => Some(task.instruction()),
            _ => None,
        }
    }

    /// What this request means once the project has had its say. `scope` is the
    /// filter the project declared under the name the request carried — empty
    /// when it named none — and `instruction` is the project's own default.
    ///
    /// The ladder is resolved here, in L0, so a transport cannot resolve it
    /// differently. See [`SearchFilter::layer`] for how the filters combine.
    pub fn resolve(&self, scope: &SearchFilter, instruction: Option<&str>) -> Resolved {
        Resolved {
            filter: scope.layer(&self.filter),
            instruction: self
                .resolved_instruction()
                .or(instruction)
                .map(str::to_string),
        }
    }
}

/// What a request means once the project's own defaults have been applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub filter: SearchFilter,
    pub instruction: Option<String>,
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
    use super::*;
    use crate::model::ProjectSpec;
    use crate::prompt::{DEFAULT_QUERY_INSTRUCTION, QueryTask};
    use std::collections::BTreeMap;

    fn request() -> SearchRequest {
        SearchRequest::new(
            ProjectId::new("demo").expect("id"),
            "where is retry handled",
        )
    }

    #[test]
    fn with_no_hint_the_daemon_default_is_used() {
        assert_eq!(request().resolved_instruction(), None);
    }

    #[test]
    fn a_preset_supplies_its_instruction() {
        let req = request().with_task(QueryTask::Symbol);
        assert_eq!(
            req.resolved_instruction(),
            Some(QueryTask::Symbol.instruction())
        );
        assert_ne!(req.resolved_instruction(), Some(DEFAULT_QUERY_INSTRUCTION));
    }

    #[test]
    fn free_text_beats_the_preset() {
        let mut req = request().with_task(QueryTask::Docs);
        req.instruction = Some("Given a migration question, retrieve the upgrade notes.".into());
        assert!(
            req.resolved_instruction()
                .expect("instruction")
                .contains("upgrade notes")
        );
    }

    #[test]
    fn a_blank_instruction_does_not_shadow_the_preset() {
        let mut req = request().with_task(QueryTask::Test);
        req.instruction = Some("   ".into());
        assert_eq!(
            req.resolved_instruction(),
            Some(QueryTask::Test.instruction())
        );
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
        assert_eq!(back.resolved_instruction(), None);
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

    /// The whole ladder in one place: request, then project, then nothing — the
    /// daemon's configured default being what "nothing" means.
    #[test]
    fn the_request_instruction_beats_the_projects_which_beats_nothing() {
        let project = Some("Given the project, retrieve what it needs.");
        let filter = SearchFilter::default();

        let from_request = SearchRequest {
            instruction: Some("Given the request, retrieve this.".into()),
            ..SearchRequest::new(ProjectId::new("demo").expect("id"), "q")
        };
        let resolved = from_request.resolve(&filter, project);
        assert_eq!(
            resolved.instruction.as_deref(),
            Some("Given the request, retrieve this.")
        );

        let silent = SearchRequest::new(ProjectId::new("demo").expect("id"), "q");
        assert_eq!(
            silent.resolve(&filter, project).instruction.as_deref(),
            project,
            "a request that says nothing gets the project's default"
        );
        assert_eq!(silent.resolve(&filter, None).instruction, None);
    }

    /// A request whose only instruction is whitespace is a request that said
    /// nothing, and must not shadow the project's.
    #[test]
    fn a_blank_request_instruction_does_not_shadow_the_projects() {
        let request = SearchRequest {
            instruction: Some("   ".into()),
            task: None,
            ..SearchRequest::new(ProjectId::new("demo").expect("id"), "q")
        };
        let resolved = request.resolve(&SearchFilter::default(), Some("the project's"));
        assert_eq!(resolved.instruction.as_deref(), Some("the project's"));
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
