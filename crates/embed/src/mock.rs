//! Deterministic offline embedder.
//!
//! INTENT: every test above L1, and `findery doctor` on a machine with no runtime
//! reachable, needs vectors that behave like vectors — stable, normalized, and
//! closer for similar text — without a model. This is that, and it is never a
//! silent fallback: selecting it is an explicit `backend = "mock"`.

use std::sync::{Arc, Mutex};

use findery_core::embedding::prompt::{self, DocumentTemplate, QueryPrompt, QueryTemplate};
use findery_core::traits::Embedder;
use findery_core::{Error, Result};

/// The model name the mock reports, whatever the profile's `model` says.
pub const MOCK_MODEL: &str = "mock";

/// The narrowest vector the mock produces.
pub const MIN_DIM: usize = 8;

/// Every query a mock embedded, as the model would have read it.
pub type QueryLog = Arc<Mutex<Vec<String>>>;

pub struct MockEmbedder {
    dim: usize,
    query_template: QueryTemplate,
    document_template: DocumentTemplate,
    /// Where each rendered query goes, when a test asked to see them.
    query_log: Option<QueryLog>,
}

impl MockEmbedder {
    /// A mock with the default prompt formats.
    pub fn new(dim: usize) -> Self {
        Self::with_templates(dim, QueryTemplate::default(), DocumentTemplate::default())
    }

    /// A mock that renders queries and documents the way a profile says, so a
    /// test can observe that a template reaches the vectors.
    pub fn with_templates(dim: usize, query: QueryTemplate, document: DocumentTemplate) -> Self {
        Self {
            dim: dim.max(MIN_DIM),
            query_template: query,
            document_template: document,
            query_log: None,
        }
    }

    /// This mock, recording every query it embeds into `log`: what lets a
    /// test see the exact text a search handed the model.
    pub fn with_query_log(mut self, log: QueryLog) -> Self {
        self.query_log = Some(log);
        self
    }

    /// Hashes text into a unit vector by seeding a counter-mode blake3 stream,
    /// so that similar strings share a good share of their coordinates.
    fn vector_of(&self, text: &str) -> Vec<f32> {
        let mut out = Vec::with_capacity(self.dim);
        let mut counter = 0u32;
        while out.len() < self.dim {
            let mut h = blake3::Hasher::new();
            h.update(text.as_bytes());
            h.update(&counter.to_le_bytes());
            // A 32-byte digest, read two bytes at a time; the tail `as_chunks`
            // leaves over is empty, 32 dividing evenly.
            let digest = h.finalize();
            let (pairs, _) = digest.as_bytes().as_chunks::<2>();
            for pair in pairs {
                if out.len() == self.dim {
                    break;
                }
                let raw = u16::from_le_bytes(*pair);
                out.push((raw as f32 / u16::MAX as f32) - 0.5);
            }
            counter += 1;
        }
        crate::openai::normalize(&mut out);
        out
    }
}

#[async_trait::async_trait]
impl Embedder for MockEmbedder {
    fn model(&self) -> &str {
        MOCK_MODEL
    }

    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed_documents(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        // Rendered here, like the real client, for the reason given there.
        Ok(inputs
            .iter()
            .map(|t| self.vector_of(&prompt::format_document(&self.document_template, t)))
            .collect())
    }

    async fn embed_query(&self, query: &str, how: QueryPrompt<'_>) -> Result<Vec<f32>> {
        // The mock has no configured instruction: its default is none.
        let input = prompt::render_query(&self.query_template, "", query, how);
        if let Some(log) = &self.query_log {
            let mut entries = log
                .lock()
                .map_err(|_| Error::Internal("the mock query log is poisoned".into()))?;
            entries.push(input.clone());
        }
        Ok(self.vector_of(&input))
    }

    async fn health(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn vectors_are_stable_unit_length_and_of_the_right_width() {
        let e = MockEmbedder::new(64);
        let a = e
            .embed_documents(&["fn main() {}".to_string()])
            .await
            .expect("embedded");
        let b = e
            .embed_documents(&["fn main() {}".to_string()])
            .await
            .expect("embedded");
        assert_eq!(a, b);
        assert_eq!(a[0].len(), 64);
        let norm: f32 = a[0].iter().map(|x| x * x).sum();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[tokio::test]
    async fn a_document_template_reaches_the_vectors_and_the_identity_does_not() {
        let plain = MockEmbedder::new(32);
        let nomic = MockEmbedder::with_templates(
            32,
            QueryTemplate::parse("search_query: {query}").expect("template"),
            DocumentTemplate::parse("search_document: {text}").expect("template"),
        );
        let doc = ["fn a() {}".to_string()];
        let raw = plain.embed_documents(&doc).await.expect("embedded");
        let wrapped = nomic.embed_documents(&doc).await.expect("embedded");
        assert_ne!(raw, wrapped);
        assert_eq!(
            raw[0],
            plain.vector_of("fn a() {}"),
            "identity sends the input as is"
        );
        assert_eq!(
            wrapped[0],
            plain.vector_of("search_document: fn a() {}"),
            "the template wraps the input, nothing else"
        );
        let q = nomic
            .embed_query("retry", QueryPrompt::Default)
            .await
            .expect("embedded");
        assert_eq!(q, plain.vector_of("search_query: retry"));
    }

    #[tokio::test]
    async fn the_log_records_each_query_as_the_model_reads_it() {
        let log = QueryLog::default();
        let e = MockEmbedder::new(16).with_query_log(log.clone());
        e.embed_query("q", QueryPrompt::Instruct("t"))
            .await
            .expect("embedded");
        e.embed_query("verbatim", QueryPrompt::Raw)
            .await
            .expect("embedded");
        let seen = log.lock().expect("unpoisoned").clone();
        assert_eq!(seen, ["Instruct: t\nQuery: q", "verbatim"]);
    }

    #[tokio::test]
    async fn different_texts_give_different_vectors() {
        let e = MockEmbedder::new(32);
        let v = e
            .embed_documents(&["alpha".to_string(), "beta".to_string()])
            .await
            .expect("embedded");
        assert_ne!(v[0], v[1]);
    }
}
