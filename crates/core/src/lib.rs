//! # findery-core — layer L0
//!
//! The shared vocabulary of findery: domain types, identifiers, errors,
//! configuration, contracts (`Embedder`, `VectorStore`, `Chunker`) and host
//! path translation.
//!
//! INTENT: this layer depends on no other workspace crate and touches neither
//! the filesystem nor the network. It describes; it does not execute. Any I/O
//! here would be an upward dependency in disguise.

pub mod api;
pub mod clock;
pub mod config;
pub mod error;
pub mod filter;
pub mod ids;
pub mod model;
pub mod path_map;
pub mod prompt;
#[macro_use]
pub mod serde_str;
pub mod traits;

pub use api::{
    AddDocument, Deleted, ErrorBody, ExportReport, ExportRequest, HealthReport, Job, JobState,
    RegisterProject, Removed, Stopping, SyncRequest, SyncResponse, WatchRequest, Watching,
};
pub use error::{Error, Result};
pub use filter::CompiledFilter;
pub use ids::{ChunkId, ContentHash, ProjectId};
pub use model::{
    Chunk, ChunkKind, FileRecord, IndexStats, Lang, Origin, Project, ProjectSpec, RawChunk,
    ScoredChunk, SearchFilter, SearchHit, SearchMode, SearchRequest, SyncMode, SyncReport,
};
pub use traits::{Chunker, Embedder, VectorPayload, VectorRecord, VectorStore};
