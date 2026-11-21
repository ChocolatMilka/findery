//! # findery-core — layer L0
//!
//! The shared vocabulary of findery:
//!
//! | Module | Holds |
//! |---|---|
//! | [`error`], [`ids`], [`traits`] | the error, the newtyped identifiers, the three seams (`Embedder`, `VectorStore`, `Chunker`) |
//! | [`model`] | what is indexed and how it is asked for: chunks, projects, search requests and the filter that evaluates them |
//! | [`api`] | the wire: what clients send and what the daemon answers |
//! | [`config`] | the daemon's configuration, its model profiles, its environment overlay |
//! | [`manifest`] | `.findery/config.toml`, the manifest of one project |
//! | [`intent`] | the intent library: named retrieval tasks, their legs, and how a request resolves to a plan |
//! | [`embedding`] | the vector space (`Recipe`) and the prompt formats a model reads |
//! | [`support`] | the clock, host/container path translation, the serde-via-`parse` macro |
//!
//! INTENT: this layer depends on no other workspace crate and touches neither
//! the filesystem nor the network. It describes; it does not execute. Any I/O
//! here would be an upward dependency in disguise.

pub mod api;
pub mod config;
pub mod embedding;
pub mod error;
pub mod ids;
pub mod intent;
pub mod manifest;
pub mod model;
pub mod support;
pub mod traits;

pub use api::{
    AddDocument, ErrorBody, ExportReport, ExportRequest, HealthReport, Job, JobState, ModelHealth,
    ModelInfo, ProjectDetached, ProjectRemoved, RegisterProject, Removed, SearchResponse, Stopping,
    SyncRequest, SyncResponse, WatchRequest, Watching,
};
pub use embedding::recipe::{Recipe, RecipeTag};
pub use error::{Error, Result};
pub use ids::{ChunkId, ContentHash, ModelAlias, ProjectId};
pub use intent::{
    Intent, IntentKind, IntentLibrary, IntentOverride, PlanContext, ResolvedLeg, ResolvedPlan,
};
pub use model::filter::CompiledFilter;
pub use model::{
    Chunk, ChunkKind, FileRecord, IndexStats, Lang, Origin, Project, ProjectSpec, RawChunk,
    ScoredChunk, SearchFilter, SearchHit, SearchMode, SearchRequest, SyncMode, SyncReport,
};
pub use traits::{Chunker, Embedder, VectorPayload, VectorRecord, VectorStore};
