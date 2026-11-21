//! Where the vectors of a search come from: the embedder, once per distinct
//! prompt, or the bucket's content-addressed cache — the example a `like` leg
//! searches with, and the hits that pairing compares.

use std::collections::HashMap;
use std::sync::Arc;

use findery_core::embedding::prompt::QueryPrompt;
use findery_core::ids::ContentHash;
use findery_core::model::{Chunk, Locator};
use findery_core::traits::Embedder;
use findery_core::{Error, Result};

use crate::gates::Lease;
use crate::storage::families::BucketHandle;

/// The member a vector leg reads, once it is known to hold this model's
/// vectors of the project.
pub(super) async fn vector_member(lease: &Lease) -> Result<Arc<BucketHandle>> {
    let id = &lease.project.id;
    let unavailable = || Error::vectors_unavailable(id, lease.alias.as_str());
    let member = lease.member.clone().ok_or_else(unavailable)?;
    // Two embedding spaces must never be compared, whatever their widths.
    member.ensure_serves(&lease.recipe)?;
    if member.store.count(id).await? == 0 {
        // Never synced under this model, or its store was emptied. A project
        // that was synced and holds no chunk at all is simply empty.
        let (_, chunks, last_sync) = member.bucket.counts(id).await?;
        if last_sync.is_none() || chunks > 0 {
            return Err(unavailable());
        }
    }
    Ok(member)
}

/// Embeds `query` once per prompt, all at once; results follow `prompts`.
pub(super) async fn embed_all(
    embedder: &dyn Embedder,
    query: &str,
    prompts: &[QueryPrompt<'_>],
) -> Vec<Result<Vec<f32>>> {
    let calls = prompts.iter().map(|how| embedder.embed_query(query, *how));
    futures::future::join_all(calls).await
}

/// The chunk a `like` locator names, and its stored vector.
pub(super) struct Example {
    pub(super) chunk: Chunk,
    pub(super) vector: Vec<f32>,
}

/// Resolves `raw` in `member` and reads the chunk's vector from the cache:
/// zero embedding.
pub(super) async fn example(lease: &Lease, member: &BucketHandle, raw: &str) -> Result<Example> {
    let chunk = crate::search::read::locate(lease, member, &Locator::parse(raw)?).await?;
    let hash = chunk.content_hash.clone();
    let mut found = stored(member, lease, std::slice::from_ref(&hash)).await?;
    let Some(vector) = found.remove(hash.as_str()) else {
        return Err(Error::VectorsUnavailable(format!(
            "`{raw}` has no stored vector under model `{}` yet; run `findery sync -p {}`",
            lease.alias, lease.project.id
        )));
    };
    Ok(Example { chunk, vector })
}

/// Stored vectors under the lease's recipe, by content hash. A hash the cache
/// does not hold is simply absent.
pub(super) async fn stored(
    member: &BucketHandle,
    lease: &Lease,
    hashes: &[ContentHash],
) -> Result<HashMap<String, Vec<f32>>> {
    let recipe = lease.recipe.as_str();
    member.bucket.cached_vectors(recipe, hashes.to_vec()).await
}
