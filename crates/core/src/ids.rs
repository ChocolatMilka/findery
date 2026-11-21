//! Newtyped identifiers. A bare `String` crossing three layers always ends up
//! swapped with another bare `String`.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

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

/// Name of a model profile: `[models.<alias>]`, `default_model = "<alias>"`.
///
/// INTENT: letters, digits, `-` and `_`, starting with a letter or digit, at
/// most 64 — the bucket names' charset **without `.`**. An alias is a TOML key
/// and one segment of a dotted setting (`findery config set models.qwen3.dim
/// 1024`), where a dot would split it in two; it must also never look like a
/// path. So `qwen3` and `harrier-v1` are aliases, `qwen3-0.6b` and `../x` are
/// not.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModelAlias(String);

/// The alias charset: see [`ModelAlias`].
fn is_alias(raw: &str) -> bool {
    !raw.contains('.') && crate::model::is_bucket_name(raw)
}

impl ModelAlias {
    /// The alias of the implicit profile `[embedder]` itself. Reserved: no
    /// `[models.<alias>]` may take it.
    pub const IMPLICIT: &'static str = "embedder";

    /// Validates an alias as a caller names it — `embedder` included, since
    /// that is how a caller selects the implicit profile.
    pub fn new(raw: impl Into<String>) -> Result<Self> {
        let raw = raw.into();
        if !is_alias(&raw) {
            return Err(Error::Invalid(format!(
                "model alias `{raw}`: expected letters, digits, `-` or `_` (no `.`), starting \
                 with a letter or digit, at most 64 characters"
            )));
        }
        Ok(Self(raw))
    }

    /// Validates the alias of a user-defined `[models.<alias>]` profile, which
    /// may not shadow the implicit one.
    pub fn for_profile(raw: impl Into<String>) -> Result<Self> {
        let alias = Self::new(raw)?;
        if alias.is_implicit() {
            return Err(Error::Invalid(format!(
                "model alias `{}` is reserved for the `[embedder]` section itself; \
                 name the profile something else",
                Self::IMPLICIT
            )));
        }
        Ok(alias)
    }

    /// The implicit profile's alias.
    pub fn implicit() -> Self {
        Self(Self::IMPLICIT.to_string())
    }

    /// Whether this names the `[embedder]` section.
    pub fn is_implicit(&self) -> bool {
        self.0 == Self::IMPLICIT
    }

    /// The alias as the configuration spells it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelAlias {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// INTENT: through `ModelAlias::new` rather than `#[serde(transparent)]`, so an
// alias read off the wire or out of a stored project row is as valid as one a
// caller built (CONV-016) — `../x` never deserializes.
impl Serialize for ModelAlias {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ModelAlias {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::new(raw).map_err(|e| serde::de::Error::custom(e.detail()))
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
    fn a_model_alias_refuses_paths_and_dots() {
        assert_eq!(ModelAlias::new("qwen3").expect("valid").as_str(), "qwen3");
        assert!(ModelAlias::new("harrier_v1-oss").is_ok());
        // A dot would split `models.<alias>.<key>` in two.
        for bad in [
            "qwen3-0.6b",
            "../x",
            "",
            ".hidden",
            "a/b",
            "a b",
            &"x".repeat(65),
        ] {
            assert!(ModelAlias::new(bad).is_err(), "{bad:?}");
            assert!(ModelAlias::for_profile(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_implicit_alias_is_reserved_for_the_embedder_section() {
        let implicit = ModelAlias::new("embedder").expect("a caller may name it");
        assert!(implicit.is_implicit());
        assert_eq!(implicit, ModelAlias::implicit());
        let err = ModelAlias::for_profile("embedder").expect_err("reserved");
        assert_eq!(err.code(), "invalid");
        assert!(
            !ModelAlias::for_profile("harrier")
                .expect("valid")
                .is_implicit()
        );
    }

    #[test]
    fn a_model_alias_round_trips_through_json_and_refuses_a_bad_one() {
        let alias = ModelAlias::new("qwen3").expect("valid");
        let text = serde_json::to_string(&alias).expect("serialize");
        assert_eq!(text, "\"qwen3\"");
        let back: ModelAlias = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back, alias);
        assert!(serde_json::from_str::<ModelAlias>("\"../x\"").is_err());
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
