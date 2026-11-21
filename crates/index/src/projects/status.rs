//! What the engine reports about itself: a project's counters, its export, the
//! model profiles, and the daemon's health.
//!
//! INTENT: split from `lib.rs`. Each of these answers "what is true right
//! now" and none of them writes an index, which is why they read better
//! together than interleaved with the paths that do.

use std::path::Path;

use findery_core::api::{HealthReport, ModelInfo};
use findery_core::embedding::recipe::Recipe;
use findery_core::ids::{ModelAlias, ProjectId};
use findery_core::model::{IndexStats, LangShare};
use findery_core::{Error, Result};

use findery_store::{ExportFormat, ExportReport};

use crate::Indexer;
use crate::gates::Access;

impl Indexer {
    /// A project's counters under the model it uses now. A model the project
    /// was never indexed with reports zero files: under that model, nothing is
    /// indexed yet — which is the truth a status line needs.
    pub async fn stats(&self, id: &ProjectId) -> Result<IndexStats> {
        let lease = self.lease(id, Access::Read).await?;
        let (files, chunks, last_sync_at, vectors, disk_bytes) = match &lease.member {
            Some(member) => {
                let (files, chunks, last) = member.bucket.counts(id).await?;
                let vectors = member.store.count(id).await?;
                (
                    files,
                    chunks,
                    last,
                    vectors,
                    member.bucket.disk_bytes().await,
                )
            }
            None => (0, 0, None, 0, 0),
        };
        let langs = match &lease.member {
            Some(member) => member.bucket.lang_mix(id).await?,
            None => Vec::new(),
        };
        Ok(IndexStats {
            project: id.to_string(),
            bucket: lease.family.name.clone(),
            files,
            chunks,
            vectors,
            dim: lease.recipe.dim(),
            model: self.model_name(&lease.alias),
            store_backend: lease.member.as_ref().map_or_else(
                || self.cfg.store.backend.as_str().to_string(),
                |m| m.store.backend().to_string(),
            ),
            last_sync_at,
            disk_bytes,
            langs: langs
                .into_iter()
                .map(|(lang, chunks)| LangShare { lang, chunks })
                .collect(),
        })
    }

    /// Exports a project's vectors under its model to `dir`, or to the
    /// configured export directory when `dir` is `None`.
    pub async fn export(
        &self,
        id: &ProjectId,
        format: ExportFormat,
        dir: Option<&str>,
    ) -> Result<ExportReport> {
        let lease = self.lease(id, Access::Read).await?;
        let Some(member) = &lease.member else {
            return Err(Error::vectors_unavailable(id, lease.alias.as_str()));
        };
        member.ensure_serves(&lease.recipe)?;
        let target = match dir {
            Some(d) => self.mapper.to_container(d),
            None => self.cfg.store.export_dir.clone(),
        };
        findery_store::export_project(
            &member.bucket,
            id,
            lease.recipe.as_str(),
            &self.model_name(&lease.alias),
            format,
            Path::new(&target),
        )
        .await
    }

    /// Every model profile, without probing anything.
    pub fn models(&self) -> Vec<ModelInfo> {
        self.pool.list()
    }

    /// Makes `alias` the model of every project with no pin, from the next
    /// operation on. Nothing is rebuilt or moved: the other model's files stay,
    /// and switching back costs nothing.
    pub fn set_default_model(&self, alias: &str) -> Result<ModelInfo> {
        let alias = self.pool.set_default(alias)?;
        tracing::info!(profile = %alias, "default model switched");
        self.pool
            .list()
            .into_iter()
            .find(|m| m.alias == alias)
            .ok_or_else(|| Error::Internal(format!("profile `{alias}` vanished from the pool")))
    }

    /// The vector space of the default model, when its width is known.
    pub fn default_recipe(&self) -> Result<Recipe> {
        self.pool.recipe_of(&self.pool.default_alias())
    }

    /// The model name a profile reports: the built embedder's, else the one it
    /// will report once built.
    pub(crate) fn model_name(&self, alias: &ModelAlias) -> String {
        match self.pool.built(alias) {
            Some(embedder) => embedder.model().to_string(),
            None => self
                .cfg
                .profiles()
                .ok()
                .and_then(|p| {
                    p.get(alias)
                        .map(|c| findery_embed::served_model(c).to_string())
                })
                .unwrap_or_else(|| alias.to_string()),
        }
    }

    /// Probes the embedding runtimes and reports what the daemon is serving.
    pub async fn health(&self) -> Result<HealthReport> {
        let models = self.pool.health().await;
        let default = self.pool.default_alias();
        let Some(served) = models.iter().find(|m| m.profile.alias == default) else {
            return Err(Error::Internal(format!(
                "default profile `{default}` is not in the pool"
            )));
        };
        let embedder_ok = served.ok;
        // The daemon is up, so a broken bucket does not make `ok` false — but a
        // bucket it could not open is a project gone from every listing, and
        // belongs in the detail line.
        let mut detail: Vec<String> = Vec::new();
        if !embedder_ok && let Some(d) = &served.detail {
            detail.push(d.clone());
        }
        detail.extend(self.broken.lock().iter().cloned());
        Ok(HealthReport {
            ok: embedder_ok,
            version: env!("CARGO_PKG_VERSION").to_string(),
            model: self.model_name(&default),
            dim: served.profile.dim.unwrap_or(0),
            store_backend: self.cfg.store.backend.as_str().to_string(),
            projects: self.homes.read().len(),
            embedder_ok,
            detail: (!detail.is_empty()).then(|| detail.join("; ")),
            notices: self.notices.clone(),
            models,
        })
    }
}
