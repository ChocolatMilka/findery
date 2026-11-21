//! `task`: the intent names the wire accepted before intents existed.
//!
//! INTENT: a closed type, so `{"task":"telepathy"}` stays the 422 it always was;
//! each value is just the name of a built-in intent.

use super::IntentLibrary;

/// A legacy retrieval intent name. Every value names a built-in locate intent
/// ([`QueryTask::as_str`] is that intent's name).
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
    /// The built-in intent's sentence with no language named.
    ///
    /// `None` only if the built-in library failed to load, which its own test
    /// rules out and `Config::validate` refuses at boot.
    pub fn instruction(&self) -> Option<&'static str> {
        IntentLibrary::builtin()
            .ok()?
            .find(self.as_str())
            .map(super::Intent::neutral_instruction)
    }

    /// The name of the intent this task stands for.
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

    /// Reads a task name or one of the aliases the wire has always accepted.
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

    /// Every task, for a CLI `--help` or an MCP enum.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::prompt::DEFAULT_QUERY_INSTRUCTION;

    /// Every spelling the wire accepted before intents existed, and the intent it
    /// must still mean.
    const LEGACY: &[(&str, &str)] = &[
        ("code", "code"),
        ("symbol", "symbol"),
        ("definition", "symbol"),
        ("usage", "usage"),
        ("callers", "usage"),
        ("docs", "docs"),
        ("doc", "docs"),
        ("test", "test"),
        ("tests", "test"),
        ("config", "config"),
        ("configuration", "config"),
        ("error", "error"),
        ("diagnostic", "error"),
    ];

    #[test]
    fn every_legacy_task_name_resolves_to_the_same_intent_through_the_library() {
        let library = IntentLibrary::builtin().expect("the shipped library loads");
        for (spelling, intent) in LEGACY {
            let task = QueryTask::parse(spelling).expect("a legacy spelling parses");
            assert_eq!(task.as_str(), *intent, "{spelling}");
            assert_eq!(
                library.get(spelling).expect("the library knows it").name,
                *intent,
                "`{spelling}` must mean the same intent as a task and as an intent"
            );
        }
    }

    #[test]
    fn every_task_round_trips_and_has_its_sentence() {
        for task in QueryTask::ALL {
            assert_eq!(QueryTask::parse(task.as_str()), Some(task), "{task:?}");
            let text = task.instruction().expect("a built-in intent");
            assert!(text.starts_with("Given "), "{task:?}: {text}");
        }
    }

    #[test]
    fn task_aliases_work_over_the_wire_and_the_unknown_is_refused() {
        let task: QueryTask = serde_json::from_str("\"definition\"").expect("alias accepted");
        assert_eq!(task, QueryTask::Symbol);
        assert_eq!(
            serde_json::to_string(&task).expect("serialize"),
            "\"symbol\""
        );
        assert!(serde_json::from_str::<QueryTask>("\"telepathy\"").is_err());
        assert_eq!(QueryTask::parse("magic"), None);
    }

    #[test]
    fn the_default_task_is_the_default_instruction() {
        assert_eq!(
            QueryTask::default().instruction(),
            Some(DEFAULT_QUERY_INSTRUCTION)
        );
    }
}
