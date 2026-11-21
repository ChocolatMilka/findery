//! Reading what a search pointed at: a chunk, the lines around a locator, and
//! the indexed files a glob names.
//!
//! INTENT: split from `lib.rs` by responsibility (CONV-029), like `search.rs`.

use findery_core::Result;
use findery_core::api::{ReadQuery, ReadResponse};
use findery_core::ids::ProjectId;
use findery_core::model::FileRecord;

use crate::{Client, urlencode};

impl Client {
    /// One chunk by id, or the lines a locator names, with context around.
    pub async fn read(&self, id: &ProjectId, query: &ReadQuery) -> Result<ReadResponse> {
        let mut params = Vec::new();
        if let Some(locator) = &query.locator {
            params.push(format!("locator={}", urlencode(locator)));
        }
        if let Some(chunk) = &query.id {
            params.push(format!("id={}", urlencode(chunk)));
        }
        if let Some(context) = query.context {
            params.push(format!("context={context}"));
        }
        let path = format!("/v1/projects/{id}/chunks?{}", params.join("&"));
        self.send::<(), _>(reqwest::Method::GET, &path, None).await
    }

    /// The indexed files a path glob names (`crates/**/*.rs`).
    pub async fn files(&self, id: &ProjectId, glob: &str) -> Result<Vec<FileRecord>> {
        let path = format!("/v1/projects/{id}/files?glob={}", urlencode(glob));
        self.send::<(), _>(reqwest::Method::GET, &path, None).await
    }
}
