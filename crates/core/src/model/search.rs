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
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
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
        }
    }

    /// Picks a named retrieval intent.
    pub fn with_task(mut self, task: crate::prompt::QueryTask) -> Self {
        self.task = Some(task);
        self
    }

    /// The instruction this request resolves to.
    ///
    /// INTENT: the precedence lives here, in L0, so the HTTP API, the MCP bridge
    /// and the CLI cannot disagree about which one wins. `None` means "use the
    /// daemon's configured default".
    pub fn resolved_instruction(&self) -> Option<&str> {
        match (&self.instruction, self.task) {
            (Some(free), _) if !free.trim().is_empty() => Some(free),
            (_, Some(task)) => Some(task.instruction()),
            _ => None,
        }
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
    use super::*;
    use crate::prompt::{DEFAULT_QUERY_INSTRUCTION, QueryTask};

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
}
