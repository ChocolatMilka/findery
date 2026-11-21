//! The call an agent's search loop makes.
//!
//! INTENT: split from `lib.rs` by responsibility (CONV-029), like `admin.rs`.

use findery_core::Result;
use findery_core::api::{SearchResponse, SearchWire};
use findery_core::model::SearchRequest;

use crate::Client;

impl Client {
    /// Searches, reading both the `{hits, warnings}` answer and the bare array
    /// an older daemon sends.
    pub async fn search(&self, req: &SearchRequest) -> Result<SearchResponse> {
        let wire: SearchWire = self
            .send(reqwest::Method::POST, "/v1/search", Some(req))
            .await?;
        Ok(wire.into())
    }
}
