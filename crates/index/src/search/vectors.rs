//! Where the vectors of a search come from: the member file that holds the
//! project's vectors under its model.

use std::sync::Arc;

use findery_core::{Error, Result};

use crate::gates::Lease;
use crate::storage::families::BucketHandle;

/// The member the vector list reads, once it is known to hold this model's
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
