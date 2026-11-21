//! The calls that manage the index rather than feed or query it: buckets,
//! model profiles, and taking a project off the daemon.
//!
//! INTENT: split from `lib.rs` by responsibility (CONV-029). They share one
//! trait — each ends with a file or a setting the user acts on next — and none
//! of them sits in an agent's search loop.

use findery_core::Result;
use findery_core::api::{
    BucketInfo, BucketRemoved, CreateBucket, ModelInfo, ProjectDetached, ProjectRemoved,
    SetDefaultModel,
};
use findery_core::ids::ProjectId;

use crate::{Client, urlencode};

impl Client {
    /// Every bucket the daemon has open, one entry per model file in each.
    pub async fn list_buckets(&self) -> Result<Vec<BucketInfo>> {
        self.send::<(), _>(reqwest::Method::GET, "/v1/buckets", None)
            .await
    }

    /// Creates a bucket, or reports the one that is already there.
    pub async fn create_bucket(&self, name: &str) -> Result<BucketInfo> {
        let body = CreateBucket {
            name: name.to_string(),
        };
        self.send(reqwest::Method::POST, "/v1/buckets", Some(&body))
            .await
    }

    /// Deletes a bucket, its projects and their vectors — or, with `model`,
    /// only that model's file, the projects staying on the others.
    ///
    /// Through the daemon: it is the only process that knows every handle on
    /// the files, and a file still open cannot be removed at all.
    pub async fn delete_bucket(&self, name: &str, model: Option<&str>) -> Result<BucketRemoved> {
        let query = model.map_or_else(String::new, |m| format!("?model={}", urlencode(m)));
        let path = format!("/v1/buckets/{name}{query}");
        self.send::<(), _>(reqwest::Method::DELETE, &path, None)
            .await
    }

    /// Removes a project and its index, and says which files went.
    ///
    /// An older daemon answers `{"deleted": …}` alone; it decodes as a removal
    /// that names no file, with `project` filled from `id`.
    pub async fn delete_project(&self, id: &ProjectId) -> Result<ProjectRemoved> {
        let path = format!("/v1/projects/{id}");
        let removed = self
            .send::<(), _>(reqwest::Method::DELETE, &path, None)
            .await?;
        Ok(named_removal(removed, id))
    }

    /// Takes a project off the daemon, keeping its index on disk, released.
    pub async fn detach_project(&self, id: &ProjectId) -> Result<ProjectDetached> {
        let path = format!("/v1/projects/{id}/detach");
        self.send::<(), _>(reqwest::Method::POST, &path, None).await
    }

    /// The model profiles the daemon knows, and which one is the default.
    pub async fn models(&self) -> Result<Vec<ModelInfo>> {
        self.send::<(), _>(reqwest::Method::GET, "/v1/models", None)
            .await
    }

    /// Switches the model unpinned projects use, from the daemon's next
    /// operation on. An alias no profile defines is `ModelUnknown`.
    pub async fn set_default_model(&self, alias: &str) -> Result<ModelInfo> {
        let body = SetDefaultModel {
            alias: alias.to_string(),
        };
        self.send(reqwest::Method::POST, "/v1/models/default", Some(&body))
            .await
    }
}

/// A removal names its project even when an older daemon did not.
fn named_removal(mut removed: ProjectRemoved, id: &ProjectId) -> ProjectRemoved {
    if removed.project.is_empty() {
        removed.project = id.to_string();
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn axle() -> ProjectId {
        ProjectId::new("axle").expect("id")
    }

    #[test]
    fn a_removal_names_the_files_that_went() {
        let body = r#"{"project":"axle","deleted":true,"rows_only":false,
                       "files_deleted":["/p/.findery/bucket.mock-abcdefgh.db"]}"#;
        let removed = named_removal(serde_json::from_str(body).expect("decode"), &axle());
        assert_eq!(removed.files_deleted.len(), 1);
        assert!(!removed.rows_only);
    }

    /// Version skew: a daemon older than `ProjectRemoved` sent `{"deleted"}`.
    #[test]
    fn the_older_removal_envelope_still_decodes_and_names_its_project() {
        let removed = named_removal(
            serde_json::from_str(r#"{"deleted":true}"#).expect("decode"),
            &axle(),
        );
        assert_eq!(removed.project, "axle");
        assert!(removed.deleted && removed.files_deleted.is_empty());
        assert!(serde_json::from_str::<ProjectRemoved>(r#"{"removed":3}"#).is_err());
    }
}
