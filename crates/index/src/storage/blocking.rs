//! Filesystem and registry work, moved off the async workers.
//!
//! INTENT: `buckets.toml` is a locked read-modify-write and a removal is a
//! string of unlinks; run on a tokio worker, either stalls every task queued
//! behind it for as long as the disk takes. One helper keeps each call site to
//! a line, so nothing is tempted to call the blocking API directly.

use findery_core::config::StoreConfig;
use findery_core::{Error, Result};

use findery_store::files::registry::Registry;

/// Runs `work` on the blocking pool.
pub(crate) async fn blocking<T, F>(work: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| Error::Internal(format!("blocking task: {e}")))?
}

/// The registry as it is on disk, refusing a file that cannot be read.
pub(crate) async fn read_registry(cfg: &StoreConfig) -> Result<Registry> {
    // Cloned: the blocking task must own what it reads, and a config is a
    // handful of short strings.
    let cfg = cfg.clone();
    blocking(move || Registry::read(&cfg)).await
}

/// One locked read-modify-write of the registry.
pub(crate) async fn update_registry<T, F>(cfg: &StoreConfig, change: F) -> Result<T>
where
    F: FnOnce(&mut Registry) -> T + Send + 'static,
    T: Send + 'static,
{
    let cfg = cfg.clone();
    blocking(move || Registry::update(&cfg, change)).await
}
