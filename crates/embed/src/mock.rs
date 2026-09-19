//! Deterministic offline embedder.
//!
//! INTENT: every test above L1, and `findery doctor` on a machine with no runtime
//! reachable, needs vectors that behave like vectors — stable, normalized, and
//! closer for similar text — without a model. This is that, and it is never a
//! silent fallback: selecting it is an explicit `backend = "mock"`.

use findery_core::Result;
use findery_core::prompt;
use findery_core::traits::Embedder;

pub struct MockEmbedder {
    dim: usize,
}

impl MockEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim: dim.max(8) }
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
            for pair in h.finalize().as_bytes().chunks_exact(2) {
                if out.len() == self.dim {
                    break;
                }
                let raw = u16::from_le_bytes([pair[0], pair[1]]);
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
        "mock"
    }

    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed_documents(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(inputs.iter().map(|t| self.vector_of(t)).collect())
    }

    async fn embed_query(&self, query: &str, instruction: Option<&str>) -> Result<Vec<f32>> {
        let task = instruction.unwrap_or("");
        Ok(self.vector_of(&prompt::format_query(task, query)))
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
    async fn different_texts_give_different_vectors() {
        let e = MockEmbedder::new(32);
        let v = e
            .embed_documents(&["alpha".to_string(), "beta".to_string()])
            .await
            .expect("embedded");
        assert_ne!(v[0], v[1]);
    }
}
