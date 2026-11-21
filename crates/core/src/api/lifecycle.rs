//! What taking a project off the daemon reports: `detach` keeps the files and
//! releases them, `rm` deletes them.
//!
//! INTENT: both answers name files, because the reason these verbs exist is a
//! file the user wants to do something with — move, delete, back up — and the
//! one thing they cannot see from outside is whether the daemon still holds it.

use serde::{Deserialize, Serialize};

/// `POST /v1/projects/{id}/detach` — the project is off the daemon, its index
/// stays on disk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectDetached {
    pub project: String,
    /// The bucket files the daemon closed: no findery process holds them any
    /// more, and they are not reopened at the next start.
    #[serde(default)]
    pub closed: Vec<String>,
    /// Why the bucket stays open, when it does — another attached project
    /// shares it, or it is the default bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub still_open: Option<String>,
}

/// `DELETE /v1/projects/{id}` — the project and its index are gone.
///
/// Every field but `deleted` has a default, so the older `{"deleted": bool}`
/// a daemon sent before this shape still decodes: a project removed, with no
/// file named.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRemoved {
    /// Empty from a daemon that predates this shape.
    #[serde(default)]
    pub project: String,
    /// Always true on success. Kept so a client that reads the older
    /// `{"deleted": true}` envelope still understands the answer.
    pub deleted: bool,
    /// Every file deleted: each member of the bucket. Empty when the bucket is
    /// shared and only the project's rows went.
    #[serde(default)]
    pub files_deleted: Vec<String>,
    /// True when the bucket is shared, so the project's rows and vectors were
    /// deleted and its files kept.
    #[serde(default)]
    pub rows_only: bool,
    /// The projects the bucket is shared with, when `rows_only`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared_with: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client that predates this shape decodes `{"deleted": …}` and ignores
    /// the rest; the field has to stay for it.
    #[test]
    fn a_removal_still_reads_as_the_older_envelope() {
        let removed = ProjectRemoved {
            project: "axle".into(),
            deleted: true,
            files_deleted: vec!["/p/.findery/bucket.mock-abcdefgh.db".into()],
            rows_only: false,
            shared_with: Vec::new(),
        };
        let value = serde_json::to_value(&removed).expect("serialize");
        assert_eq!(value["deleted"], true);
        assert!(value.get("shared_with").is_none());
        let back: ProjectRemoved = serde_json::from_value(value).expect("deserialize");
        assert_eq!(back, removed);
    }

    /// Version skew the other way: a newer client reading an older daemon.
    #[test]
    fn the_older_envelope_decodes_as_a_removal_that_names_no_file() {
        let old: ProjectRemoved =
            serde_json::from_str(r#"{"deleted": true}"#).expect("the older shape");
        assert!(old.deleted && !old.rows_only);
        assert!(old.project.is_empty() && old.files_deleted.is_empty());
        assert!(serde_json::from_str::<ProjectRemoved>(r#"{"project": "x"}"#).is_err());
    }
}
