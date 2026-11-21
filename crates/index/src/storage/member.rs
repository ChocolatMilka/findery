//! One open **member** of a bucket family: a file holding one recipe's
//! vectors, and the guard that refuses to trust what its name promises.
//!
//! INTENT: split from `families.rs` (CONV-029). A family decides which file an
//! operation works in; a member answers whether that file holds what it is
//! asked for — two responsibilities, each short enough to read whole.

use std::path::Path;
use std::sync::Arc;

use findery_core::api::BucketMember;
use findery_core::config::StoreConfig;
use findery_core::embedding::recipe::{Recipe, RecipeTag};
use findery_core::traits::VectorStore;
use findery_core::{Error, Result};

use findery_store::{Bucket, FamilyPath};

use crate::storage::families::BucketFamily;

/// A path as findery shows it — in a listing, a registry entry, a report:
/// with `/` separators on every platform.
///
/// INTENT: a family's member paths are joined by the OS (`dir.join(name)`)
/// while the paths a client sent are written with `/`; shown raw, one listing
/// would print `C:/p/.findery\bucket.x.db`. Windows accepts either separator,
/// so a single spelling costs nothing and compares as text.
pub fn shown(path: &Path) -> String {
    let text = path.display().to_string();
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text
    }
}

/// One member file the daemon has open.
pub struct BucketHandle {
    /// The family name it answers to.
    pub name: String,
    /// The tag the resolver finds it under: the one its file name carries, or
    /// for the legacy base file, the tag of the recipe it records.
    pub tag: RecipeTag,
    /// The recipe this member is expected to hold: the one that was asked for
    /// when it was created, else the one a profile resolves its tag to, else
    /// what the file records.
    pub recipe: Recipe,
    /// What the file actually records (`meta.embedding_recipe`). The
    /// anti-collision guard: an operation whose recipe differs is refused.
    pub stored_recipe: String,
    /// True for the base file of the family (`bucket.db`), which carries no tag
    /// in its name — a legacy file adoption could not rename yet.
    pub legacy: bool,
    /// The open file.
    pub bucket: Bucket,
    /// Where this member's vectors are written and searched.
    pub store: Arc<dyn VectorStore>,
    /// True when the family is where the name alone would put it. A conflict's
    /// repair depends on it: re-creating a bucket by name re-creates it THERE.
    pub at_derived_path: bool,
    family: FamilyPath,
}

/// What a member is opened as: where it is found, and what it should hold.
pub(crate) struct MemberSpec {
    pub tag: RecipeTag,
    pub recipe: Recipe,
    pub legacy: bool,
}

impl BucketHandle {
    /// Wraps an open member file of `family`.
    pub(crate) async fn open(
        cfg: &StoreConfig,
        family: &BucketFamily,
        bucket: Bucket,
        spec: MemberSpec,
    ) -> Result<Self> {
        let stored_recipe = bucket
            .get_meta("embedding_recipe")
            .await?
            .unwrap_or_default();
        let namespace = bucket.vector_namespace().await?;
        let store = findery_store::build_vector_store(cfg, bucket.clone(), &namespace)?;
        Ok(Self {
            name: family.name.clone(),
            tag: spec.tag,
            recipe: spec.recipe,
            stored_recipe,
            legacy: spec.legacy,
            bucket,
            store,
            at_derived_path: family.at_derived_path,
            family: family.base.clone(),
        })
    }

    /// True when this file does not hold what its name promises.
    pub fn conflicted(&self) -> bool {
        self.stored_recipe != self.recipe.as_str() || self.recipe.tag() != self.tag
    }

    /// Refuses with [`Error::RecipeMismatch`] unless this member holds exactly
    /// `wanted`'s vectors.
    ///
    /// INTENT: refused at the operation, never at boot. The file stays open,
    /// listed and deletable, and every operation that touches no vector —
    /// keyword search, listing, `bucket rm` — keeps working meanwhile.
    pub fn ensure_serves(&self, wanted: &Recipe) -> Result<()> {
        if !self.conflicted() && self.recipe == *wanted {
            return Ok(());
        }
        Err(self.mismatch(wanted.as_str()))
    }

    /// The refusal a vector operation gets, as text for a listing.
    pub fn recipe_conflict(&self) -> Option<String> {
        self.conflicted()
            .then(|| self.mismatch(self.recipe.as_str()).detail())
    }

    /// This file as `GET /v1/buckets` lists it.
    pub async fn info(&self, serving: bool) -> Result<BucketMember> {
        let (projects, chunks) = self.bucket.totals().await?;
        Ok(BucketMember {
            path: shown(self.bucket.path()),
            tag: self.tag.to_string(),
            recipe: self.recipe.to_string(),
            legacy: self.legacy,
            serving,
            projects,
            chunks,
            disk_bytes: self.bucket.disk_bytes().await,
            conflict: self.recipe_conflict(),
        })
    }

    fn mismatch(&self, serves: &str) -> Error {
        let holds = if self.stored_recipe.is_empty() {
            "no recipe at all"
        } else {
            self.stored_recipe.as_str()
        };
        // The name its content earns, when that content names a recipe at all.
        let rename_to = Recipe::parse(&self.stored_recipe)
            .map(|r| shown(&self.family.member(&r)))
            .unwrap_or_else(|_| "a name that is not this one".to_string());
        Error::misnamed_member(
            &self.name,
            &shown(self.bucket.path()),
            holds,
            serves,
            &rename_to,
            self.at_derived_path,
        )
    }
}
