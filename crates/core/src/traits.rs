//! The three contracts every replaceable part of findery implements.
//!
//! INTENT: each trait is the seam where a user swaps an implementation without
//! touching the indexing engine. Keep them narrow. Every method added here has
//! to be honoured by every present and future backend.

use crate::Result;
use crate::config::ChunkConfig;
use crate::ids::{ChunkId, ProjectId};
use crate::model::{ChunkKind, Lang, RawChunk, ScoredChunk, SearchFilter};
use async_trait::async_trait;

/// Turns text into an embedding vector.
///
/// Implementations live in `findery-embed`. The asymmetry between documents and
/// queries is part of the contract, not an implementation detail: Qwen3
/// instructs queries and leaves documents bare.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Model identifier, as reported to clients and used in the cache key.
    fn model(&self) -> &str;

    /// Vector width. Never guessed at runtime: a mismatch with the configured
    /// value is a hard failure, because half a corpus at the wrong width is
    /// silently unsearchable.
    fn dim(&self) -> usize;

    /// Embeds document-side inputs, batched and possibly reordered internally
    /// but always returned in the caller's order, one vector per input.
    async fn embed_documents(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>>;

    /// Embeds a query, applying the instruction prefix.
    ///
    /// `instruction` overrides the configured default; `None` uses it.
    async fn embed_query(&self, query: &str, instruction: Option<&str>) -> Result<Vec<f32>>;

    /// Cheap liveness probe against the runtime.
    async fn health(&self) -> Result<()>;
}

/// The minimal metadata a vector store keeps next to each vector so it can
/// filter without calling back into the catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VectorPayload {
    pub path: String,
    pub lang: Lang,
    pub kind: ChunkKind,
    pub symbol: Option<String>,
}

/// One point to write into a vector store.
#[derive(Clone, Debug)]
pub struct VectorRecord {
    pub id: ChunkId,
    pub vector: Vec<f32>,
    pub payload: VectorPayload,
}

/// Where vectors live and how they are searched.
///
/// Implementations live in `findery-store`. This is the "choose where my vectors go"
/// seam: SQLite today, Qdrant today, anything else tomorrow.
#[async_trait]
pub trait VectorStore: Send + Sync {
    /// Backend name, surfaced in `IndexStats`.
    fn backend(&self) -> &'static str;

    /// Creates whatever the backend needs for this project, idempotently.
    async fn ensure_project(&self, project: &ProjectId, dim: usize) -> Result<()>;

    /// Removes the project and every vector it owns.
    async fn drop_project(&self, project: &ProjectId) -> Result<()>;

    /// Inserts or replaces points. Must be idempotent on `id`.
    async fn upsert(&self, project: &ProjectId, records: &[VectorRecord]) -> Result<()>;

    /// Deletes points by id, returning how many existed.
    async fn delete(&self, project: &ProjectId, ids: &[ChunkId]) -> Result<u64>;

    /// Top-`k` nearest neighbours by cosine similarity, filter applied first.
    ///
    /// Scores are similarities in `[-1, 1]`, sorted descending.
    async fn search(
        &self,
        project: &ProjectId,
        query: &[f32],
        k: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<ScoredChunk>>;

    /// Number of stored points for the project.
    async fn count(&self, project: &ProjectId) -> Result<u64>;
}

/// Splits one file into embeddable fragments.
///
/// Implementations live in `findery-chunk`. A chunker is pure: same input, same
/// output, no I/O, no clock, no randomness — the whole incremental sync relies
/// on that determinism.
pub trait Chunker: Send + Sync {
    /// The language this chunker is registered for.
    fn lang(&self) -> Lang;

    /// Splits `text`. `path` is passed for context only and must not be read
    /// from disk.
    fn chunk(&self, path: &str, text: &str, cfg: &ChunkConfig) -> Result<Vec<RawChunk>>;
}
