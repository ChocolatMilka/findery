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
pub mod config;
pub mod embedding;
pub mod error;
pub mod ids;
pub mod manifest;
pub mod model;
pub mod support;
pub mod traits;

pub use api::{
    AddDocument, Deleted, ErrorBody, ExportReport, ExportRequest, HealthReport, Job, JobState,
    RegisterProject, Removed, Stopping, SyncRequest, SyncResponse, WatchRequest, Watching,
};
pub use error::{Error, Result};
pub use ids::{ChunkId, ContentHash, ProjectId};
pub use model::filter::CompiledFilter;
pub use model::{
    Chunk, ChunkKind, FileRecord, IndexStats, Lang, Origin, Project, ProjectSpec, RawChunk,
    ScoredChunk, SearchFilter, SearchHit, SearchMode, SearchRequest, SyncMode, SyncReport,
};
pub use traits::{Chunker, Embedder, VectorPayload, VectorRecord, VectorStore};
