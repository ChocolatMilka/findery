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
pub async fn build(cfg: &EmbedderConfig) -> Result<Arc<dyn Embedder>> {
    match cfg.backend {
        EmbedderBackend::OpenAiCompat => {
            let e = OpenAiCompatEmbedder::connect(cfg.clone()).await?;
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
        let e = build(&cfg).await.expect("built");
        assert_eq!(e.dim(), 128);
        assert_eq!(e.model(), "mock");
        e.health().await.expect("healthy");
    }

    #[tokio::test]
    async fn an_unreachable_runtime_fails_at_build_time_not_at_query_time() {
        let cfg = EmbedderConfig {
            backend: EmbedderBackend::OpenAiCompat,
            base_url: "http://127.0.0.1:1".into(),
            max_retries: 0,
            timeout_secs: 2,
            ..EmbedderConfig::default()
        };
        // `Arc<dyn Embedder>` is not `Debug`, so the Ok side is discarded
        // before asserting on the error.
        let err = match build(&cfg).await {
            Ok(_) => panic!("a closed port must not yield an embedder"),
            Err(e) => e,
        };
        assert_eq!(err.code(), "upstream");
    }
}
