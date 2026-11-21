//! qwen3.7-text-embedding instruction templates.
//!
//! qwen3.7-text-embedding is instruction-aware and **asymmetric**: the QUERY carries an
//! instruction, the DOCUMENT never does. Qwen measures a 1-5% retrieval gain
//! from a task-tailored instruction, which is free — it costs one prefix on a
//! string that was going to be embedded anyway.
//!
//! Prefixing documents too would cost accuracy AND double the blast radius of a
//! prompt change, since every stored vector would have to be recomputed.
//!
//! Wire format expected by the model:
//!
//! ```text
//! Instruct: {task}
//! Query: {query}
//! ```
//!
//! INTENT: this module is the single source of truth for that format
//! (CONV-012). Nobody concatenates `"Instruct: "` anywhere else.

/// Default task description, used when a caller names nothing.
pub const DEFAULT_QUERY_INSTRUCTION: &str = "Given a developer question about a Rust codebase, retrieve the source code \
     or documentation passages that answer it.";

/// Named retrieval intents.
///
/// INTENT: a preset is what an agent can actually pick correctly. Asking a model
/// to invent a good instruction on every call gets you a paraphrase of the query
/// — which is worse than the default, because it duplicates the signal the query
/// already carries instead of describing the TASK.
///
/// Free-form instructions remain available and take precedence; these are the
/// paved road, not a fence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QueryTask {
    /// General "where is this handled" question. The default.
    #[default]
    Code,
    /// A symbol name, looking for its definition.
    Symbol,
    /// An API, looking for how it is called.
    Usage,
    /// Design rationale and prose rather than implementation.
    Docs,
    /// The tests that pin a behaviour down.
    Test,
    /// A configuration key, flag or environment variable.
    Config,
    /// An error message, a panic or a diagnostic code.
    Error,
}

impl QueryTask {
    /// The instruction sent to the model for this intent.
    ///
    /// Each one describes the TASK and the shape of the wanted passage; none of
    /// them restates the query.
    pub fn instruction(&self) -> &'static str {
        match self {
            Self::Code => DEFAULT_QUERY_INSTRUCTION,
            Self::Symbol => {
                "Given the name of a symbol in a codebase, retrieve the passage that \
                 defines it: its declaration, its type, and its documentation comment."
            }
            Self::Usage => {
                "Given an API, a function or a type, retrieve passages that CALL or \
                 INSTANTIATE it, showing how it is used in practice."
            }
            Self::Docs => {
                "Given a question about a project, retrieve documentation, design notes \
                 and rationale that explain how and why it works, rather than \
                 implementation code."
            }
            Self::Test => {
                "Given a described behaviour, retrieve the tests that exercise and \
                 assert it."
            }
            Self::Config => {
                "Given a setting, flag or environment variable, retrieve the passages \
                 that declare it, give its default, and read it."
            }
            Self::Error => {
                "Given an error message, a panic or a diagnostic, retrieve the code that \
                 produces it and the code that handles it."
            }
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Symbol => "symbol",
            Self::Usage => "usage",
            Self::Docs => "docs",
            Self::Test => "test",
            Self::Config => "config",
            Self::Error => "error",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "code" => Some(Self::Code),
            "symbol" | "definition" => Some(Self::Symbol),
            "usage" | "callers" => Some(Self::Usage),
            "docs" | "doc" => Some(Self::Docs),
            "test" | "tests" => Some(Self::Test),
            "config" | "configuration" => Some(Self::Config),
            "error" | "diagnostic" => Some(Self::Error),
            _ => None,
        }
    }

    /// Every preset, for a CLI `--help` or an MCP enum.
    pub const ALL: [Self; 7] = [
        Self::Code,
        Self::Symbol,
        Self::Usage,
        Self::Docs,
        Self::Test,
        Self::Config,
        Self::Error,
    ];
}

crate::serde_via_parse!(QueryTask, option);

/// Renders the query side of the asymmetric pair.
pub fn format_query(instruction: &str, query: &str) -> String {
    let task = instruction.trim();
    if task.is_empty() {
        return query.to_string();
    }
    format!("Instruct: {task}\nQuery: {query}")
}

/// Renders the document side. Identity by contract — kept as a function so the
/// asymmetry is explicit at every call site instead of being folklore.
pub fn format_document(text: &str) -> &str {
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_carries_the_instruction() {
        let q = format_query(DEFAULT_QUERY_INSTRUCTION, "where is retry handled");
        assert!(q.starts_with("Instruct: "));
        assert!(q.contains("\nQuery: where is retry handled"));
    }

    #[test]
    fn empty_instruction_degrades_to_the_raw_query() {
        assert_eq!(format_query("  ", "abc"), "abc");
    }

    #[test]
    fn documents_stay_untouched() {
        assert_eq!(format_document("fn main() {}"), "fn main() {}");
    }

    #[test]
    fn every_preset_round_trips_and_describes_a_task() {
        for task in QueryTask::ALL {
            assert_eq!(QueryTask::parse(task.as_str()), Some(task), "{task:?}");
            let text = task.instruction();
            assert!(
                text.len() > 40,
                "{task:?}: an instruction must describe the task"
            );
            assert!(
                text.starts_with("Given "),
                "{task:?}: Qwen's format states the input, then what to retrieve"
            );
        }
    }

    #[test]
    fn presets_are_distinct() {
        let mut seen = std::collections::HashSet::new();
        for task in QueryTask::ALL {
            assert!(
                seen.insert(task.instruction()),
                "{task:?} duplicates another preset"
            );
        }
    }

    #[test]
    fn aliases_are_accepted_and_the_unknown_is_refused() {
        assert_eq!(QueryTask::parse("DEFINITION"), Some(QueryTask::Symbol));
        assert_eq!(QueryTask::parse("callers"), Some(QueryTask::Usage));
        assert_eq!(QueryTask::parse("magic"), None);
    }

    #[test]
    fn preset_aliases_work_over_the_wire_too() {
        // `--task definition` on the CLI and `{"task":"definition"}` over HTTP
        // must mean the same thing.
        let task: QueryTask = serde_json::from_str("\"definition\"").expect("alias accepted");
        assert_eq!(task, QueryTask::Symbol);
        assert_eq!(
            serde_json::to_string(&task).expect("serialize"),
            "\"symbol\""
        );
        assert!(serde_json::from_str::<QueryTask>("\"telepathy\"").is_err());
    }

    #[test]
    fn the_default_preset_is_the_default_instruction() {
        assert_eq!(
            QueryTask::default().instruction(),
            DEFAULT_QUERY_INSTRUCTION
        );
    }
}
