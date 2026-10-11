//! # findery-embed — layer L1
//!
//! Turns text into vectors. One HTTP client covers every OpenAI-compatible
//! runtime, local or remote; one mock covers offline tests.
//!
//! The model is never compiled in. [`build`] reads the configuration and hands
//! back an `Arc<dyn Embedder>`; the indexing layer above never learns which
//! runtime it is talking to.

pub mod mock;
pub mod openai;

use findery_core::Result;
use findery_core::config::{EmbedderBackend, EmbedderConfig};
use findery_core::traits::Embedder;
use std::sync::Arc;

pub use mock::MockEmbedder;
pub use openai::OpenAiCompatEmbedder;

/// Builds the configured embedder, connecting and probing where applicable.
///
/// `known_dim` is the width the caller already knows from somewhere else —
/// typically a bucket that was built earlier. It is what lets the daemon come
/// up while the embedding runtime is still down; see
/// [`OpenAiCompatEmbedder::connect`].
pub async fn build(cfg: &EmbedderConfig, known_dim: Option<usize>) -> Result<Arc<dyn Embedder>> {
    match cfg.backend {
        EmbedderBackend::OpenAiCompat => {
            let e = OpenAiCompatEmbedder::connect(cfg.clone(), known_dim).await?;
            Ok(Arc::new(e))
        }
        EmbedderBackend::Mock => {
            let dim = if cfg.dim == 0 { 256 } else { cfg.dim };
            Ok(Arc::new(MockEmbedder::new(dim)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_mock_backend_needs_no_runtime() {
        let cfg = EmbedderConfig {
            backend: EmbedderBackend::Mock,
            dim: 128,
            ..EmbedderConfig::default()
        };
        let e = build(&cfg, None).await.expect("built");
        assert_eq!(e.dim(), 128);
        assert_eq!(e.model(), "mock");
        e.health().await.expect("healthy");
    }

    /// The width has to come from somewhere, and a closed port cannot supply it.
    #[tokio::test]
    async fn an_unreachable_runtime_of_unknown_width_cannot_be_built() {
        let cfg = unreachable_cfg(0);
        // `Arc<dyn Embedder>` is not `Debug`, so the Ok side is discarded
        // before asserting on the error.
        let err = match build(&cfg, None).await {
            Ok(_) => panic!("there is no width to open a store with"),
            Err(e) => e,
        };
        assert_eq!(err.code(), "config");
        assert!(err.to_string().contains("FINDERY_EMBED_DIM"), "{err}");
    }

    /// The whole point of degraded boot: a daemon that cannot embed anything is
    /// still a daemon that can answer `findery doctor` and say why.
    #[tokio::test]
    async fn an_unreachable_runtime_of_known_width_builds_degraded() {
        for (cfg, hint) in [
            (unreachable_cfg(1024), None),
            (unreachable_cfg(0), Some(1024)),
        ] {
            let e = build(&cfg, hint).await.expect("degraded, not absent");
            assert_eq!(e.dim(), 1024);
            assert!(e.health().await.is_err(), "and it says so");
        }
    }

    fn unreachable_cfg(dim: usize) -> EmbedderConfig {
        EmbedderConfig {
            backend: EmbedderBackend::OpenAiCompat,
            base_url: "http://127.0.0.1:1".into(),
            max_retries: 0,
            timeout_secs: 2,
            dim,
            ..EmbedderConfig::default()
        }
    }
}
