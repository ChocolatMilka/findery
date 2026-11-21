//! `GET /v1/intents`: the library a project's searches resolve against.

use serde::{Deserialize, Serialize};

use crate::intent::{Intent, IntentKind};

/// One intent, as a listing shows it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IntentInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    pub kind: IntentKind,
    pub summary: String,
    /// The leg labels, in the order they run.
    pub legs: Vec<String>,
}

impl From<&Intent> for IntentInfo {
    fn from(intent: &Intent) -> Self {
        Self {
            name: intent.name.clone(),
            aliases: intent.aliases.clone(),
            kind: intent.kind,
            summary: intent.summary.clone(),
            legs: intent.legs.iter().map(|leg| leg.label.clone()).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::IntentLibrary;

    #[test]
    fn a_listed_workflow_names_its_legs_and_round_trips() {
        let library = IntentLibrary::builtin().expect("the shipped library loads");
        let info = IntentInfo::from(library.get("review").expect("shipped"));
        assert_eq!(info.legs, ["callers", "tests", "docs", "similar"]);
        let wire = serde_json::to_string(&info).expect("serialize");
        assert!(wire.contains(r#""kind":"workflow""#), "{wire}");
        let back: IntentInfo = serde_json::from_str(&wire).expect("parse");
        assert_eq!(back.name, "review");
    }

    #[test]
    fn an_unknown_kind_is_refused() {
        let raw = r#"{"name":"x","kind":"magic","summary":"s","legs":[]}"#;
        assert!(serde_json::from_str::<IntentInfo>(raw).is_err());
    }
}
