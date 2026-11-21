//! Compiled form of a [`SearchFilter`].
//!
//! INTENT: filtering semantics are defined once, here, so that the SQLite store
//! and the Qdrant store can never disagree on what `paths = ["src/**"]` means.
//! A backend may push part of the predicate down to its engine, but whatever it
//! cannot push down it evaluates with THIS matcher.

use crate::model::SearchFilter;
use crate::traits::VectorPayload;
use crate::{Error, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};

/// A [`SearchFilter`] with its globs compiled once per query.
#[derive(Debug)]
pub struct CompiledFilter {
    paths: Option<GlobSet>,
    langs: Vec<crate::model::Lang>,
    kinds: Vec<crate::model::ChunkKind>,
    symbol_contains: Option<String>,
}

impl CompiledFilter {
    /// Compiles the filter, rejecting malformed globs up front rather than
    /// silently matching nothing.
    pub fn new(filter: &SearchFilter) -> Result<Self> {
        let paths = if filter.paths.is_empty() {
            None
        } else {
            let mut b = GlobSetBuilder::new();
            for pat in &filter.paths {
                let glob = Glob::new(pat)
                    .map_err(|e| Error::Invalid(format!("path glob `{pat}`: {e}")))?;
                b.add(glob);
            }
            Some(
                b.build()
                    .map_err(|e| Error::Invalid(format!("path globs: {e}")))?,
            )
        };
        Ok(Self {
            paths,
            langs: filter.langs.clone(),
            kinds: filter.kinds.clone(),
            symbol_contains: filter.symbol_contains.as_ref().map(|s| s.to_lowercase()),
        })
    }

    /// True when the filter constrains nothing, letting callers skip the scan.
    pub fn is_pass_through(&self) -> bool {
        self.paths.is_none()
            && self.langs.is_empty()
            && self.kinds.is_empty()
            && self.symbol_contains.is_none()
    }

    /// Evaluates the predicate against a stored payload.
    pub fn matches(&self, payload: &VectorPayload) -> bool {
        if let Some(set) = &self.paths
            && !set.is_match(&payload.path)
        {
            return false;
        }
        if !self.langs.is_empty() && !self.langs.contains(&payload.lang) {
            return false;
        }
        if !self.kinds.is_empty() && !self.kinds.contains(&payload.kind) {
            return false;
        }
        if let Some(needle) = &self.symbol_contains {
            let hit = payload
                .symbol
                .as_ref()
                .is_some_and(|s| s.to_lowercase().contains(needle.as_str()));
            if !hit {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChunkKind, Lang};

    fn payload(path: &str, lang: Lang, kind: ChunkKind, symbol: Option<&str>) -> VectorPayload {
        VectorPayload {
            path: path.into(),
            lang,
            kind,
            symbol: symbol.map(str::to_string),
        }
    }

    #[test]
    fn empty_filter_passes_everything() {
        let f = CompiledFilter::new(&SearchFilter::default()).expect("compile");
        assert!(f.is_pass_through());
        assert!(f.matches(&payload("a.rs", Lang::Rust, ChunkKind::Function, None)));
    }

    #[test]
    fn path_globs_are_anchored_on_the_relative_path() {
        let f = CompiledFilter::new(&SearchFilter {
            paths: vec!["crates/core/**".into()],
            ..Default::default()
        })
        .expect("compile");
        assert!(f.matches(&payload(
            "crates/core/src/lib.rs",
            Lang::Rust,
            ChunkKind::File,
            None
        )));
        assert!(!f.matches(&payload(
            "crates/cli/src/lib.rs",
            Lang::Rust,
            ChunkKind::File,
            None
        )));
    }

    #[test]
    fn malformed_glob_is_refused_not_ignored() {
        let bad = SearchFilter {
            paths: vec!["[".into()],
            ..Default::default()
        };
        assert!(CompiledFilter::new(&bad).is_err());
    }

    #[test]
    fn symbol_match_is_case_insensitive() {
        let f = CompiledFilter::new(&SearchFilter {
            symbol_contains: Some("Chunk".into()),
            ..Default::default()
        })
        .expect("compile");
        assert!(f.matches(&payload(
            "a.rs",
            Lang::Rust,
            ChunkKind::Struct,
            Some("RawChunkId")
        )));
        assert!(!f.matches(&payload(
            "a.rs",
            Lang::Rust,
            ChunkKind::Struct,
            Some("Project")
        )));
    }
}
