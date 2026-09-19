//! Request and response shapes that exist only at the HTTP boundary.
//!
//! INTENT: everything the domain already names — `Project`, `SearchHit`,
//! `SyncReport`, `IndexStats` — is serialized straight from `findery-core`. Only the
//! genuinely transport-shaped things live here: how a sync is requested, how an
//! error is rendered, how a job is acknowledged.

// Every request and response shape is defined once, in L0. The HTTP layer adds
// nothing of its own: a DTO that existed only here would be a shape the Rust
// client and the MCP bridge could not see.
pub use findery_core::api::{
    AddDocument, Deleted, ErrorBody, ExportRequest, PruneRequest, PushDocuments, Removed, Stopping,
    SyncRequest, SyncResponse, WatchRequest, Watching,
};
use serde::Deserialize;

/// `DELETE /v1/projects/{id}/documents?path=...`
#[derive(Clone, Debug, Deserialize)]
pub struct DocumentQuery {
    pub path: String,
}
