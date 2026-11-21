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

use std::sync::Arc;

use findery_core::Result;
use findery_core::config::{EmbedderBackend, EmbedderConfig};
use findery_core::traits::Embedder;

pub use mock::MockEmbedder;
pub use openai::{OpenAiCompatEmbedder, SERVICE as UPSTREAM_SERVICE};

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
        EmbedderBackend::Mock => Ok(Arc::new(MockEmbedder::with_templates(
            mock_dim(cfg),
            cfg.query_template()?,
            cfg.document_template()?,
        ))),
    }
}

/// The model name an embedder built from `cfg` will report through
/// [`Embedder::model`] — what a recipe's identity falls back on when the
/// profile names no `weights`.
///
/// INTENT: a profile that has not been built yet still has a vector space, and
/// the engine needs its name to find the bucket file holding it — a read must
/// not have to dial a runtime to learn which file to read. The mock reports
/// `mock` whatever `model` says, so this is the one place that knows it.
pub fn served_model(cfg: &EmbedderConfig) -> &str {
    match cfg.backend {
        EmbedderBackend::OpenAiCompat => &cfg.model,
        EmbedderBackend::Mock => mock::MOCK_MODEL,
    }
}

/// The width an embedder built from `cfg` will serve, when the configuration
/// alone decides it: a configured `dim`, or the mock's. `None` means only the
/// runtime can say.
pub fn configured_dim(cfg: &EmbedderConfig) -> Option<usize> {
    match cfg.backend {
        EmbedderBackend::OpenAiCompat => (cfg.dim != 0).then_some(cfg.dim),
        EmbedderBackend::Mock => Some(mock_dim(cfg)),
    }
}

/// The mock's width: `dim`, 256 when unset, never below its floor.
fn mock_dim(cfg: &EmbedderConfig) -> usize {
    let dim = if cfg.dim == 0 { 256 } else { cfg.dim };
    dim.max(mock::MIN_DIM)
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

    /// A template that cannot be parsed never builds an embedder: half a corpus
    /// rendered one way and half another is unsearchable.
    #[tokio::test]
    async fn a_broken_template_is_refused_at_build() {
        let cfg = EmbedderConfig {
            backend: EmbedderBackend::Mock,
            document_template: Some("no slot".into()),
            ..EmbedderConfig::default()
        };
        let err = match build(&cfg, None).await {
            Ok(_) => panic!("a template without a text slot must not build"),
            Err(e) => e,
        };
        assert_eq!(err.code(), "config");
    }

    /// What the engine reads before building anything agrees with what the
    /// built embedder reports — otherwise a profile would look for its bucket
    /// file under one name and write it under another.
    #[tokio::test]
    async fn the_unbuilt_name_and_width_are_the_built_ones() {
        for dim in [0, 4, 128] {
            let cfg = EmbedderConfig {
                backend: EmbedderBackend::Mock,
                dim,
                ..EmbedderConfig::default()
            };
            let e = build(&cfg, None).await.expect("built");
            assert_eq!(served_model(&cfg), e.model());
            assert_eq!(configured_dim(&cfg), Some(e.dim()));
        }
        let remote = unreachable_cfg(0);
        assert_eq!(served_model(&remote), remote.model);
        assert_eq!(configured_dim(&remote), None, "only the runtime can say");
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
