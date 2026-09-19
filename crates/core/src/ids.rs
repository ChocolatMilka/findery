//! Newtyped identifiers. A bare `String` crossing three layers always ends up
//! swapped with another bare `String`.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Stable slug of an indexed project (`[a-z0-9_-]{1,64}`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProjectId(String);

impl ProjectId {
    /// Validates and adopts a slug. Rejects anything that could become a path
    /// segment, a hostile Qdrant collection name, or a visual SQL ambiguity.
    pub fn new(raw: impl Into<String>) -> Result<Self> {
        let raw = raw.into();
        if raw.is_empty() || raw.len() > 64 {
            return Err(Error::Invalid(format!(
                "project id: expected length 1..=64, got {}",
                raw.len()
            )));
        }
        let ok = raw
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
        if !ok {
            return Err(Error::Invalid(format!(
                "project id `{raw}`: expected [a-z0-9_-]"
            )));
        }
        Ok(Self(raw))
    }

    /// Derives a valid slug from any human-facing name.
    pub fn slugify(raw: &str) -> Result<Self> {
        let mut out = String::with_capacity(raw.len());
        let mut last_dash = false;
        for ch in raw.chars() {
            let c = ch.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                out.push(c);
                last_dash = false;
            } else if !last_dash && !out.is_empty() {
                out.push('-');
                last_dash = true;
            }
        }
        while out.ends_with('-') {
            out.pop();
        }
        out.truncate(64);
        Self::new(out)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Content fingerprint (blake3, 32 bytes, hex encoded).
///
/// INTENT: this is the embedding cache key. Two chunks with identical content —
/// even moved, even renamed — share one vector and are embedded exactly once in
/// the lifetime of the repository.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentHash(String);

impl ContentHash {
    /// Hashes arbitrary content.
    pub fn of(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).to_hex().to_string())
    }

    /// Hashes content qualified by a prefix (model, dimension, chunker
    /// version) so that a recipe change can never reuse a stale vector.
    pub fn qualified(prefix: &str, bytes: &[u8]) -> Self {
        let mut h = blake3::Hasher::new();
        h.update(prefix.as_bytes());
        h.update(b"\x00");
        h.update(bytes);
        Self(h.finalize().to_hex().to_string())
    }

    /// Adopts an already computed fingerprint (read back from SQLite).
    pub fn from_hex(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Stable chunk identity: `blake3(project | rel_path | ordinal)`.
///
/// INTENT: stable across re-indexing as long as the chunk keeps the same
/// position in the same file — so an upsert overwrites cleanly instead of
/// leaving an orphan behind in the vector store.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChunkId(String);

impl ChunkId {
    pub fn derive(project: &ProjectId, rel_path: &str, ordinal: u32) -> Self {
        let mut h = blake3::Hasher::new();
        h.update(project.as_str().as_bytes());
        h.update(b"\x00");
        h.update(rel_path.as_bytes());
        h.update(b"\x00");
        h.update(&ordinal.to_le_bytes());
        Self(h.finalize().to_hex().to_string())
    }

    pub fn from_hex(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Projection to `u64` for stores whose point id is numeric (Qdrant).
    /// 64 bits of blake3: collision probability stays negligible at repository
    /// scale (< 1e-7 for 10^6 chunks).
    pub fn as_u64(&self) -> u64 {
        let head = &self.0[..self.0.len().min(16)];
        u64::from_str_radix(head, 16).unwrap_or(0)
    }
}

impl fmt::Display for ChunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_id_rejects_hostile_characters() {
        assert!(ProjectId::new("axle-compiler").is_ok());
        assert!(ProjectId::new("../etc").is_err());
        assert!(ProjectId::new("Axle").is_err());
        assert!(ProjectId::new("").is_err());
    }

    #[test]
    fn slugify_yields_a_valid_id() {
        let id = ProjectId::slugify("C:/Users/My Rust Project!").expect("valid slug");
        assert_eq!(id.as_str(), "c-users-my-rust-project");
    }

    #[test]
    fn chunk_id_is_stable_and_positional() {
        let p = ProjectId::new("demo").expect("valid id");
        let a = ChunkId::derive(&p, "src/lib.rs", 3);
        let b = ChunkId::derive(&p, "src/lib.rs", 3);
        let c = ChunkId::derive(&p, "src/lib.rs", 4);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a.as_u64(), 0);
    }

    #[test]
    fn qualified_hash_separates_recipes() {
        let a = ContentHash::qualified("qwen3:1024", b"fn main() {}");
        let b = ContentHash::qualified("qwen3:2560", b"fn main() {}");
        assert_ne!(a, b);
    }
}
