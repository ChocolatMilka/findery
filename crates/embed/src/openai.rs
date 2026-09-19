//! Client for any OpenAI-compatible `/v1/embeddings` runtime.
//!
//! INTENT: one client covers the whole spectrum the user may want — a local TEI
//! or llama.cpp container, a GPU box on the LAN, or a hosted provider behind
//! TLS. Nothing here names a model: the model, the route, the auth header and
//! the vector width all arrive as configuration.
//!
//! Two response shapes are accepted, because the same code must talk to both
//! `/v1/embeddings` (OpenAI envelope) and TEI's native `/embed` (bare matrix).

use findery_core::config::EmbedderConfig;
use findery_core::prompt;
use findery_core::traits::Embedder;
use findery_core::{Error, Result};
use futures::stream::{self, StreamExt};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

const SERVICE: &str = "embedder";

pub struct OpenAiCompatEmbedder {
    http: reqwest::Client,
    cfg: EmbedderConfig,
    endpoint: String,
    /// Width actually served by the runtime, probed at connect time.
    dim: usize,
    /// Caps concurrent in-flight batches so a large sync cannot stampede a
    /// shared or rate-limited endpoint.
    permits: Arc<Semaphore>,
}

impl OpenAiCompatEmbedder {
    /// Builds the client and, unless disabled, probes the runtime once.
    ///
    /// Probing is not ceremony: a corpus half-embedded at 1024 dims and half at
    /// 2560 is silently unsearchable, and the only cheap moment to catch that
    /// is before the first byte is written.
    ///
    /// An unreachable runtime is *not* fatal as long as the width is known —
    /// from the configuration, or from `known_dim`, which the caller reads off
    /// an existing catalog. The daemon starts, `/v1/health` reports the model as
    /// down, and only the operations that actually need a vector fail. A daemon
    /// that refused to boot would instead take the diagnostic down with it, at
    /// the exact moment the user needs it.
    pub async fn connect(cfg: EmbedderConfig, known_dim: Option<usize>) -> Result<Self> {
        let mut http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .user_agent(concat!("findery/", env!("CARGO_PKG_VERSION")));
        if cfg.base_url.starts_with("http://") {
            http = http.http1_only();
        }
        let http = http
            .build()
            .map_err(|e| Error::Config(format!("http client: {e}")))?;

        let endpoint = cfg.endpoint();
        let permits = Arc::new(Semaphore::new(cfg.concurrency.max(1)));
        let mut me = Self {
            http,
            cfg,
            endpoint,
            dim: 0,
            permits,
        };

        // Everything we could fall back on if the runtime does not answer.
        let assumed = (me.cfg.dim != 0).then_some(me.cfg.dim).or(known_dim);
        let mut verified = true;

        if me.cfg.dim == 0 || me.cfg.verify_dim {
            match me.probe_dim().await {
                Ok(probed) => {
                    if me.cfg.dim != 0 && me.cfg.dim != probed {
                        return Err(Error::Config(format!(
                            "embedder.dim is {} but `{}` returned {} dimensions; \
                             fix the config or set send_dimensions=true",
                            me.cfg.dim, me.cfg.model, probed
                        )));
                    }
                    me.dim = probed;
                }
                Err(e) => match assumed {
                    Some(dim) => {
                        tracing::warn!(
                            endpoint = %me.endpoint,
                            model = %me.cfg.model,
                            dim,
                            detail = %e,
                            "embedding runtime unreachable; starting degraded"
                        );
                        me.dim = dim;
                        verified = false;
                    }
                    None => {
                        return Err(Error::Config(format!(
                            "`{}` is unreachable and the vector width is unknown, so nothing \
                             can be opened: start the embedding runtime, or set \
                             FINDERY_EMBED_DIM to the width `{}` produces ({e})",
                            me.endpoint, me.cfg.model
                        )));
                    }
                },
            }
        } else {
            me.dim = me.cfg.dim;
        }
        tracing::info!(
            model = %me.cfg.model,
            endpoint = %me.endpoint,
            dim = me.dim,
            "{}",
            if verified { "embedder ready" } else { "embedder configured but not answering" }
        );
        Ok(me)
    }

    /// Sends one throwaway input and reports the width that comes back.
    async fn probe_dim(&self) -> Result<usize> {
        let vectors = self.post_batch(&["dimension probe".to_string()]).await?;
        let first = vectors
            .first()
            .ok_or_else(|| Error::Embed("probe returned no vector".into()))?;
        if first.is_empty() {
            return Err(Error::Embed("probe returned an empty vector".into()));
        }
        Ok(first.len())
    }

    /// One HTTP round trip, with retries on transient failures.
    async fn post_batch(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        let truncated: Vec<&str> = inputs
            .iter()
            .map(|s| truncate_chars(s, self.cfg.max_input_chars))
            .collect();

        let mut body = serde_json::json!({
            "model": self.cfg.model,
            "input": truncated,
            "encoding_format": "float",
        });
        if self.cfg.send_dimensions && self.cfg.dim > 0 {
            body["dimensions"] = serde_json::json!(self.cfg.dim);
        }

        let mut attempt = 0u32;
        loop {
            let mut req = self.http.post(&self.endpoint).json(&body);
            if let Some(key) = &self.cfg.api_key {
                req = req.header(
                    &self.cfg.auth_header,
                    format!("{}{}", self.cfg.auth_prefix, key),
                );
            }
            for (name, value) in &self.cfg.extra_headers {
                req = req.header(name, value);
            }

            // The success path returns from inside the match; what survives it
            // is always a failure, tagged with whether retrying can help.
            let (retryable, detail) = match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        let text = resp
                            .text()
                            .await
                            .map_err(|e| Error::upstream(SERVICE, format!("read body: {e}")))?;
                        return parse_vectors(&text);
                    }
                    let detail = resp.text().await.unwrap_or_default();
                    let retryable = status.as_u16() == 429 || status.is_server_error();
                    (
                        retryable,
                        format!("HTTP {status}: {}", first_chars(&detail, 400)),
                    )
                }
                Err(e) => (
                    e.is_timeout() || e.is_connect() || e.is_request(),
                    e.to_string(),
                ),
            };
            if !retryable || attempt >= self.cfg.max_retries {
                return Err(Error::upstream(SERVICE, detail));
            }
            let backoff = Duration::from_millis((200u64 << attempt.min(5)).min(5_000));
            tracing::warn!(attempt, ?backoff, %detail, "embedding batch failed, retrying");
            tokio::time::sleep(backoff).await;
            attempt += 1;
        }
    }

    /// Validates width and applies L2 normalization when configured.
    fn finish(&self, mut vectors: Vec<Vec<f32>>, expected: usize) -> Result<Vec<Vec<f32>>> {
        if vectors.len() != expected {
            return Err(Error::Embed(format!(
                "runtime returned {} vectors for {} inputs",
                vectors.len(),
                expected
            )));
        }
        for v in &mut vectors {
            if v.len() != self.dim {
                return Err(Error::Embed(format!(
                    "runtime returned {} dimensions, expected {}",
                    v.len(),
                    self.dim
                )));
            }
            if self.cfg.normalize {
                normalize(v);
            }
        }
        Ok(vectors)
    }
}

#[async_trait::async_trait]
impl Embedder for OpenAiCompatEmbedder {
    fn model(&self) -> &str {
        &self.cfg.model
    }

    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed_documents(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        // Futures are materialized eagerly rather than through a closure: the
        // higher-ranked lifetime a `map` closure would demand does not survive
        // the `async_trait` desugaring of `&self`.
        let mut pending = Vec::new();
        for batch in inputs.chunks(self.cfg.batch_size.max(1)) {
            pending.push(async move {
                let _permit = self
                    .permits
                    .acquire()
                    .await
                    .map_err(|_| Error::Internal("embedder semaphore closed".into()))?;
                let vectors = self.post_batch(batch).await?;
                self.finish(vectors, batch.len())
            });
        }
        // `buffered` preserves input order, which is what lets callers zip the
        // result back onto their chunks without carrying an index.
        let results = stream::iter(pending)
            .buffered(self.cfg.concurrency.max(1))
            .collect::<Vec<_>>()
            .await;

        let mut out = Vec::with_capacity(inputs.len());
        for batch in results {
            out.extend(batch?);
        }
        Ok(out)
    }

    async fn embed_query(&self, query: &str, instruction: Option<&str>) -> Result<Vec<f32>> {
        let task = instruction.unwrap_or(&self.cfg.query_instruction);
        let input = prompt::format_query(task, query);
        let vectors = self.post_batch(std::slice::from_ref(&input)).await?;
        let mut vectors = self.finish(vectors, 1)?;
        vectors
            .pop()
            .ok_or_else(|| Error::Embed("empty query embedding".into()))
    }

    async fn health(&self) -> Result<()> {
        self.post_batch(&["health".to_string()]).await.map(|_| ())
    }
}

// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct OpenAiEnvelope {
    data: Vec<OpenAiItem>,
}

#[derive(Deserialize)]
struct OpenAiItem {
    #[serde(default)]
    index: usize,
    embedding: Vec<f32>,
}

/// Accepts both the OpenAI envelope and TEI's bare `[[f32]]`.
fn parse_vectors(body: &str) -> Result<Vec<Vec<f32>>> {
    if let Ok(envelope) = serde_json::from_str::<OpenAiEnvelope>(body) {
        let mut items = envelope.data;
        // The spec does not promise ordering, so restore it by `index`.
        items.sort_by_key(|i| i.index);
        return Ok(items.into_iter().map(|i| i.embedding).collect());
    }
    if let Ok(matrix) = serde_json::from_str::<Vec<Vec<f32>>>(body) {
        return Ok(matrix);
    }
    Err(Error::upstream(
        SERVICE,
        format!("unrecognized response: {}", first_chars(body, 300)),
    ))
}

/// L2-normalizes in place; a zero vector is left alone rather than producing
/// NaNs that would poison every later comparison.
pub fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Truncates on a char boundary, never mid-codepoint.
fn truncate_chars(s: &str, max: usize) -> &str {
    if max == 0 || s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn first_chars(s: &str, max: usize) -> &str {
    truncate_chars(s, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_envelope_is_reordered_by_index() {
        let body =
            r#"{"data":[{"index":1,"embedding":[0.0,1.0]},{"index":0,"embedding":[1.0,0.0]}]}"#;
        let v = parse_vectors(body).expect("parsed");
        assert_eq!(v, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
    }

    #[test]
    fn tei_bare_matrix_is_accepted() {
        let v = parse_vectors("[[1.0,2.0],[3.0,4.0]]").expect("parsed");
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn an_unrecognized_body_is_an_upstream_error() {
        let err = parse_vectors("<html>502</html>").expect_err("must fail");
        assert_eq!(err.code(), "upstream");
    }

    #[test]
    fn normalization_makes_cosine_a_dot_product() {
        let mut v = vec![3.0f32, 4.0];
        normalize(&mut v);
        let norm = v.iter().map(|x| x * x).sum::<f32>();
        assert!((norm - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_zero_vector_survives_normalization() {
        let mut v = vec![0.0f32; 4];
        normalize(&mut v);
        assert!(v.iter().all(|x| *x == 0.0));
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        let s = "héllo wörld";
        let t = truncate_chars(s, 2);
        assert!(s.starts_with(t));
        assert!(t.len() <= 2);
    }
}
