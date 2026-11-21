//! The calls an agent's search loop makes: a search, and the intents it can
//! name.
//!
//! INTENT: split from `lib.rs` by responsibility (CONV-029), like `admin.rs`.

use findery_core::Result;
use findery_core::api::{IntentInfo, SearchResponse, SearchWire};
use findery_core::ids::ProjectId;
use findery_core::model::SearchRequest;

use crate::Client;

impl Client {
    /// Searches, reading the current answer — legs, pairs and duplicates
    /// included — the `{hits, warnings}` one, and the bare array an older
    /// daemon sends.
    pub async fn search(&self, req: &SearchRequest) -> Result<SearchResponse> {
        let wire: SearchWire = self
            .send(reqwest::Method::POST, "/v1/search", Some(req))
            .await?;
        Ok(wire.into())
    }

    /// The intents a search can name: the daemon's library, or a project's,
    /// its manifest layer included.
    pub async fn intents(&self, project: Option<&ProjectId>) -> Result<Vec<IntentInfo>> {
        let path = match project {
            Some(id) => format!("/v1/intents?project={id}"),
            None => "/v1/intents".to_string(),
        };
        self.send::<(), _>(reqwest::Method::GET, &path, None).await
    }
}
