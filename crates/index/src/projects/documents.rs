//! Documents fed one at a time or pushed in batches, and their removal.
//!
//! INTENT: split from `lib.rs`, which keeps the engine's wiring. These are the
//! entry points a client uses when it, not the daemon, holds the bytes — and
//! the rule they share is worth seeing in one place: an addition writes the
//! active model's member (creating it), a removal reaches EVERY member, because
//! a document the user deleted must not come back the day they switch model.

use std::collections::HashSet;

use findery_core::api::AddDocument;
use findery_core::ids::ProjectId;
use findery_core::model::{FileRecord, SyncMode, SyncReport};
use findery_core::{Error, Result};

use crate::gates::Access;
use crate::{Indexer, ProgressHandle, sanitize_rel, sync};

impl Indexer {
    /// Adds or refreshes one document. With `text`, the content is taken from
    /// the request and the file never has to exist on disk; without, the file
    /// is read from disk only when the project's walk would accept it.
    pub async fn add_document(
        &self,
        id: &ProjectId,
        rel_path: &str,
        text: Option<String>,
    ) -> Result<SyncReport> {
        let rel = sanitize_rel(rel_path)?;
        let Some(body) = text else {
            let targeted = SyncMode::Paths(vec![rel]);
            return self.sync(id, targeted, &ProgressHandle::detached()).await;
        };
        let lease = self.lease(id, Access::Write).await?;
        sync::index_inline(self, &lease, &rel, &body).await
    }

    /// Indexes a batch of documents pushed by a client.
    ///
    /// This is the entry point for a daemon that cannot see the project: the
    /// client walks, reads and sends, so no bind mount and no path translation
    /// are involved at all.
    pub async fn push_documents(
        &self,
        id: &ProjectId,
        documents: Vec<AddDocument>,
    ) -> Result<SyncReport> {
        // Validated before the lease: a bad path must not create a member.
        let mut cleaned = Vec::with_capacity(documents.len());
        for doc in documents {
            let text = doc.text.ok_or_else(|| {
                Error::Invalid(format!("document `{}` was pushed without text", doc.path))
            })?;
            cleaned.push((sanitize_rel(&doc.path)?, text));
        }
        let lease = self.lease(id, Access::Write).await?;
        sync::push_documents(self, &lease, cleaned).await
    }

    /// Removes pushed files the client no longer has, given everything it saw,
    /// from every member of the project's bucket. Returns how many went from
    /// the member serving the project's model.
    pub async fn prune(&self, id: &ProjectId, keep: Vec<String>) -> Result<u64> {
        let mut wanted = HashSet::with_capacity(keep.len());
        for path in keep {
            wanted.insert(sanitize_rel(&path)?);
        }
        let lease = self.lease(id, Access::Read).await?;
        let mut active = 0;
        for member in lease.family.members() {
            let removed = sync::prune(&member, &lease.project, &wanted).await?;
            if member.tag == lease.recipe.tag() {
                active = removed;
            }
        }
        Ok(active)
    }

    /// Everything the member of the project's model knows about its files.
    ///
    /// A pushing client diffs against this to decide what is worth sending.
    /// Under a model the project was never indexed with, that is nothing — so
    /// the client sends everything, and the push embeds it.
    pub async fn file_manifest(&self, id: &ProjectId) -> Result<Vec<FileRecord>> {
        let lease = self.lease(id, Access::Read).await?;
        let Some(member) = &lease.member else {
            return Ok(Vec::new());
        };
        let mut files: Vec<_> = member
            .bucket
            .file_map(&lease.project.id)
            .await?
            .into_values()
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    /// Drops one document from every member of the project's bucket. Returns
    /// the chunks removed from the member serving the project's model.
    pub async fn remove_document(&self, id: &ProjectId, rel_path: &str) -> Result<u64> {
        let rel = sanitize_rel(rel_path)?;
        let lease = self.lease(id, Access::Read).await?;
        let mut active = 0;
        for member in lease.family.members() {
            let removed = sync::forget_file(&member, &lease.project, &rel).await?;
            if member.tag == lease.recipe.tag() {
                active = removed;
            }
        }
        Ok(active)
    }
}
