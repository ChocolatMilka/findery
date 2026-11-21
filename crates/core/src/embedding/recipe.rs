//! The recipe — the name of one vector space — and the two short forms derived
//! from it: a tag that is safe in a file name, and a slug a human can read.
//!
//! A vector is only comparable with vectors built by the same weights, at the
//! same width, over the same rendering of the document. The recipe spells those
//! three facts in one string:
//!
//! ```text
//! {identity}[|doc={12 hex}]:{dim}
//! ```
//!
//! `identity` is the profile's `weights` when it names them, otherwise the model
//! name the runtime serves. The `|doc=` part is the blake3 fingerprint of a
//! non-default document template, and it is absent otherwise — so a profile
//! with no `weights` and no `document_template` yields exactly `"<model>:<dim>"`,
//! the bytes every bucket built so far carries, and no cached vector is lost.
//!
//! INTENT: this module is the only builder of a recipe (CONV-013). It used to
//! be assembled by `format!` in two places, one of which could write `:0`; two
//! builders of a cache key are a cache that silently stops hitting the day they
//! disagree.
//!
//! The tag is a cosmetic-free fingerprint (8 base32 characters, 40 bits) that
//! names a bucket member file. The slug is cosmetic only: it is never parsed
//! back, a member is found by its tag.

use std::fmt;

use crate::{Error, Result};

/// Hex characters of the document template fingerprint kept in the recipe.
const DOC_FINGERPRINT_HEX: usize = 12;

/// Context string of the tag's key derivation. Part of the frozen format: a
/// different context is a different tag for every recipe there is.
const TAG_CONTEXT: &str = "findery recipe tag v1";

/// Bytes of the derived key kept in a tag: 5 bytes are exactly 8 base32 digits.
const TAG_BYTES: usize = 5;

/// Length of a tag, in characters.
pub const TAG_LEN: usize = 8;

/// Longest slug, in characters. Keeps a member's file name readable in a
/// directory listing without risking a path-length limit.
const SLUG_MAX: usize = 40;

/// RFC 4648 base32, lowercased: case-folding file systems (NTFS, APFS) cannot
/// merge two distinct tags.
const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// The vector-space name: `"{identity}[|doc={fingerprint}]:{dim}"`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Recipe(String);

impl Recipe {
    /// Builds the recipe of a profile.
    ///
    /// `identity` is the profile's `weights` when set, else the served model
    /// name. `document_template` enters the recipe only when it is not the
    /// identity template: a template changes every stored vector, while the
    /// chunk's `content_hash` only covers `Chunk::embed_input()`.
    pub fn of(identity: &str, dim: usize, document_template: Option<&str>) -> Result<Self> {
        check_identity(identity)?;
        if dim == 0 {
            return Err(Error::Invalid(format!(
                "recipe of `{identity}`: the width is unknown (0); a recipe names a width"
            )));
        }
        let mut out = String::with_capacity(identity.len() + 24);
        out.push_str(identity);
        if let Some(template) = document_template
            && !crate::embedding::prompt::DocumentTemplate::is_identity_source(template)
        {
            let hex = blake3::hash(template.as_bytes()).to_hex();
            out.push_str("|doc=");
            out.push_str(&hex[..DOC_FINGERPRINT_HEX]);
        }
        out.push(':');
        out.push_str(&dim.to_string());
        Ok(Self(out))
    }

    /// Adopts a recipe read back from a bucket, refusing one that
    /// [`Recipe::of`] could not have built.
    pub fn parse(raw: &str) -> Result<Self> {
        split(raw)?;
        Ok(Self(raw.to_string()))
    }

    /// The recipe, byte for byte as it is stored.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The weights or model name the vectors came from.
    pub fn identity(&self) -> &str {
        self.parts().0
    }

    /// The document template fingerprint, when the template is not the
    /// identity.
    pub fn document_fingerprint(&self) -> Option<&str> {
        self.parts().1
    }

    /// The vector width.
    pub fn dim(&self) -> usize {
        self.parts().2
    }

    /// The fingerprint that names this recipe's bucket member file.
    pub fn tag(&self) -> RecipeTag {
        let key = blake3::derive_key(TAG_CONTEXT, self.0.as_bytes());
        let encoded = base32_lower(&key[..TAG_BYTES]);
        let mut out = [0u8; TAG_LEN];
        out.copy_from_slice(&encoded.as_bytes()[..TAG_LEN]);
        RecipeTag(out)
    }

    /// A readable rendering of the identity, for file names. Cosmetic only:
    /// never parsed, never compared — the tag carries the identity.
    pub fn slug(&self) -> String {
        let identity = self.identity();
        let last = identity.rsplit('/').next().unwrap_or(identity);
        let mut slug = String::with_capacity(last.len());
        for ch in last.chars() {
            let c = ch.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                slug.push(c);
            } else if !slug.ends_with('-') {
                slug.push('-');
            }
        }
        let mut slug = slug.trim_matches('-').to_string();
        if slug.len() > SLUG_MAX {
            // ASCII by construction, so any byte index is a char boundary.
            let cut = match slug[..SLUG_MAX].rfind('-') {
                Some(at) if at > 0 => at,
                _ => SLUG_MAX,
            };
            slug.truncate(cut);
            slug = slug.trim_matches('-').to_string();
        }
        if slug.is_empty() {
            "model".to_string()
        } else {
            slug
        }
    }

    /// The three parts. A `Recipe` only exists once [`split`] accepted it.
    fn parts(&self) -> (&str, Option<&str>, usize) {
        split(&self.0).unwrap_or((self.0.as_str(), None, 0))
    }
}

impl fmt::Display for Recipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A recipe's file-name fingerprint: 8 characters of lowercase base32.
///
/// INTENT: 40 bits put the odds of two recipes colliding in one directory at
/// about 1.7e-10 for twenty recipes, and a collision is detected anyway — the
/// member's stored recipe is compared to the one its tag came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RecipeTag([u8; TAG_LEN]);

impl RecipeTag {
    /// The tag as text.
    pub fn as_str(&self) -> &str {
        // Every byte is drawn from `BASE32`, so the bytes are ASCII and the
        // fallback is never taken.
        std::str::from_utf8(&self.0).unwrap_or_default()
    }

    /// Reads a tag back from a file name.
    pub fn parse(raw: &str) -> Result<Self> {
        let bytes = raw.as_bytes();
        if bytes.len() != TAG_LEN || !bytes.iter().all(|b| BASE32.contains(b)) {
            return Err(Error::Invalid(format!(
                "recipe tag `{raw}`: expected {TAG_LEN} characters of a-z2-7"
            )));
        }
        let mut out = [0u8; TAG_LEN];
        out.copy_from_slice(bytes);
        Ok(Self(out))
    }
}

impl fmt::Display for RecipeTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Refuses an identity no recipe can carry unambiguously.
fn check_identity(identity: &str) -> Result<()> {
    if identity.trim().is_empty() {
        return Err(Error::Invalid(
            "recipe: the model identity must not be empty".into(),
        ));
    }
    if identity.contains('|') {
        return Err(Error::Invalid(format!(
            "recipe: the model identity `{identity}` contains `|`, which separates the \
             document template fingerprint"
        )));
    }
    Ok(())
}

/// `(identity, doc fingerprint, dim)`. The width is split off the LAST `:`, so
/// an Ollama identity such as `nomic-embed-text:latest` survives.
fn split(raw: &str) -> Result<(&str, Option<&str>, usize)> {
    let bad = |why: &str| Error::Invalid(format!("recipe `{raw}`: {why}"));
    let (head, dim) = raw
        .rsplit_once(':')
        .ok_or_else(|| bad("expected `<identity>:<dim>`"))?;
    let dim: usize = dim.parse().map_err(|_| bad("the width is not a number"))?;
    if dim == 0 {
        return Err(bad("the width must be > 0"));
    }
    let (identity, fingerprint) = match head.split_once('|') {
        None => (head, None),
        Some((identity, rest)) => {
            let hex = rest
                .strip_prefix("doc=")
                .ok_or_else(|| bad("expected `|doc=<fingerprint>`"))?;
            if hex.len() != DOC_FINGERPRINT_HEX || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(bad("the document fingerprint is not 12 hex digits"));
            }
            (identity, Some(hex))
        }
    };
    check_identity(identity)?;
    Ok((identity, fingerprint, dim))
}

/// RFC 4648 base32, lowercase, no padding.
fn base32_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() * 8).div_ceil(5));
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(BASE32[((buffer >> bits) & 31) as usize]));
        }
        buffer &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(char::from(BASE32[((buffer << (5 - bits)) & 31) as usize]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EmbedderBackend, EmbedderConfig};

    #[test]
    fn base32_matches_the_rfc_4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "my"),
            ("fo", "mzxq"),
            ("foo", "mzxw6"),
            ("foob", "mzxw6yq"),
            ("fooba", "mzxw6ytb"),
            ("foobar", "mzxw6ytboi"),
        ] {
            assert_eq!(base32_lower(input.as_bytes()), expected, "{input:?}");
        }
    }

    /// FROZEN. A tag names a bucket member on disk: changing one of these
    /// values — the context string, the byte count, the alphabet — renames
    /// every user's bucket files out from under them, and their vectors become
    /// orphans nobody looks up. A failing assertion here is a format break, not
    /// a test to update.
    #[test]
    fn recipe_tags_are_frozen() {
        let doc = Recipe::of(
            "nomic-embed-text:latest",
            768,
            Some("search_document: {text}"),
        )
        .expect("recipe");
        // The fingerprint is frozen too: it is part of the recipe, so of the tag.
        assert_eq!(doc.as_str(), "nomic-embed-text:latest|doc=c5203c0ed15d:768");
        for (recipe, tag) in [
            ("text-embedding-qwen3-embedding-0.6b:1024", "47m32eui"),
            ("harrier-oss-v1-0.6b:1024", "zvpmtsdf"),
            ("qwen3.7-text-embedding:1024", "xkrji2az"),
            ("mock:256", "7nlk63up"),
            (doc.as_str(), "7dtmi2pe"),
        ] {
            let parsed = Recipe::parse(recipe).expect("a frozen recipe parses");
            assert_eq!(parsed.tag().as_str(), tag, "{recipe}");
        }
    }

    #[test]
    fn a_tag_round_trips_and_refuses_what_it_cannot_be() {
        let tag = Recipe::of("mock", 256, None).expect("recipe").tag();
        assert_eq!(tag.as_str().len(), TAG_LEN);
        assert_eq!(RecipeTag::parse(tag.as_str()).expect("round trip"), tag);
        assert_eq!(tag.to_string(), tag.as_str());
        for bad in [
            "",
            "abcdefg",
            "abcdefghi",
            "ABCDEFGH",
            "abcdefg1",
            "abcdef0h",
        ] {
            assert!(RecipeTag::parse(bad).is_err(), "{bad:?}");
        }
    }

    /// The whole compatibility claim: a profile with no `weights` and no
    /// document template names the space every existing bucket was built in.
    #[test]
    fn a_plain_profile_yields_todays_recipe_bytes() {
        let table = [
            ("text-embedding-qwen3-embedding-0.6b", 1024),
            ("harrier-oss-v1-0.6b", 1024),
            ("qwen3.7-text-embedding", 2560),
            ("nomic-embed-text:latest", 768),
            ("Qwen/Qwen3-Embedding-0.6B", 1024),
        ];
        for (model, dim) in table {
            let cfg = EmbedderConfig {
                backend: EmbedderBackend::OpenAiCompat,
                model: model.into(),
                dim,
                ..EmbedderConfig::default()
            };
            assert_eq!(cfg.weights, None);
            assert_eq!(cfg.document_template, None);
            let recipe = Recipe::of(
                cfg.identity(&cfg.model),
                cfg.dim,
                cfg.document_template.as_deref(),
            )
            .expect("recipe");
            assert_eq!(recipe.as_str(), format!("{model}:{dim}"));
        }
        // Spelling the identity template out changes nothing either.
        let explicit = Recipe::of("m", 8, Some(" {text} ")).expect("recipe");
        assert_eq!(explicit.as_str(), "m:8");
    }

    #[test]
    fn a_document_template_opens_its_own_space() {
        let plain = Recipe::of("nomic", 768, None).expect("recipe");
        let doc = Recipe::of("nomic", 768, Some("search_document: {text}")).expect("recipe");
        assert_ne!(plain, doc);
        assert_ne!(plain.tag(), doc.tag());
        assert!(doc.as_str().starts_with("nomic|doc="), "{doc}");
        assert_eq!(doc.identity(), "nomic");
        assert_eq!(doc.dim(), 768);
        assert_eq!(doc.document_fingerprint().map(str::len), Some(12));
        assert_eq!(Recipe::parse(doc.as_str()).expect("round trip"), doc);
    }

    #[test]
    fn an_ollama_identity_keeps_its_colon() {
        let r = Recipe::of("nomic-embed-text:latest", 768, None).expect("recipe");
        assert_eq!(r.identity(), "nomic-embed-text:latest");
        assert_eq!(r.dim(), 768);
        assert_eq!(r.document_fingerprint(), None);
    }

    #[test]
    fn an_identity_that_would_be_ambiguous_is_refused() {
        assert!(Recipe::of("", 1024, None).is_err());
        assert!(Recipe::of("   ", 1024, None).is_err());
        assert!(Recipe::of("a|b", 1024, None).is_err());
        assert!(Recipe::of("model", 0, None).is_err());
        for bad in [
            "no-width",
            "m:wide",
            "m:0",
            ":1024",
            "m|x:8",
            "m|doc=xyz:8",
            "m|doc=0123456789abc:8",
        ] {
            assert!(Recipe::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_slug_is_a_readable_rendering_of_the_identity() {
        let slug = |raw: &str| Recipe::parse(raw).expect("recipe").slug();
        assert_eq!(
            slug("Qwen/Qwen3-Embedding-0.6B:1024"),
            "qwen3-embedding-0-6b"
        );
        assert_eq!(
            slug("text-embedding-qwen3-embedding-0.6b:1024"),
            "text-embedding-qwen3-embedding-0-6b"
        );
        assert_eq!(slug("harrier-oss-v1-0.6b:1024"), "harrier-oss-v1-0-6b");
        assert_eq!(
            slug("nomic-embed-text:latest:768"),
            "nomic-embed-text-latest"
        );
        assert_eq!(slug("mock|doc=0123456789ab:256"), "mock");
        assert_eq!(slug("模型:1024"), "model");
        assert_eq!(slug("--__--:8"), "model");
        assert_eq!(slug("org/:8"), "model");
    }

    #[test]
    fn a_long_slug_is_cut_at_a_word_boundary() {
        let long = "a-very-long-model-name-that-keeps-going-and-going-forever:8";
        let slug = Recipe::parse(long).expect("recipe").slug();
        assert!(slug.len() <= SLUG_MAX, "{slug}");
        assert_eq!(slug, "a-very-long-model-name-that-keeps-going");
        let unbroken = format!("{}:8", "x".repeat(60));
        assert_eq!(
            Recipe::parse(&unbroken).expect("recipe").slug().len(),
            SLUG_MAX
        );
    }
}
