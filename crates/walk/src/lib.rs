//! # findery-walk — layer L1
//!
//! Filesystem walk: which files of a project are candidates for indexing.
//!
//! INTENT: this lives in its own crate because BOTH sides need it. The daemon
//! walks when it can see the project; a client walks when it cannot, and pushes
//! the contents over the API. One walk implementation, so the two modes can
//! never disagree about which files belong to a project.
//!
//! INTENT: `.gitignore` is honoured by default, so `target/` — which outweighs
//! the source by two orders of magnitude — never reaches the model. Every other
//! exclusion is explicit configuration, and the walk reports what it skipped
//! rather than dropping it in silence.

use findery_core::model::{Lang, ProjectSpec};
use findery_core::{Error, Result};
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

/// A file the walker accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// Path relative to the project root, `/`-separated.
    pub rel_path: String,
    pub abs_path: PathBuf,
    pub size: u64,
    /// Modification time in nanoseconds since the epoch; `0` when unavailable.
    pub mtime_ns: i64,
    pub lang: Lang,
}

/// Result of one walk.
#[derive(Debug, Default)]
pub struct WalkOutcome {
    pub files: Vec<Candidate>,
    /// Non-fatal problems: unreadable entries, files above the size ceiling.
    pub warnings: Vec<String>,
}

/// Walks `root` under the rules of `spec`.
///
/// Blocking: `ignore` is a synchronous crate. Callers run this inside
/// `spawn_blocking` rather than paying for an async filesystem abstraction the
/// operating system does not actually provide.
pub fn walk(root: &Path, spec: &ProjectSpec) -> Result<WalkOutcome> {
    if !root.is_dir() {
        return Err(Error::Invalid(format!(
            "project root `{}` is not a directory",
            root.display()
        )));
    }
    let include = build_globs(&spec.include)?;
    let exclude = build_globs(&spec.exclude)?;
    let extensions: Vec<String> = spec
        .extensions
        .iter()
        .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
        .collect();

    let mut outcome = WalkOutcome::default();
    let walker = WalkBuilder::new(root)
        .standard_filters(spec.follow_gitignore)
        .git_ignore(spec.follow_gitignore)
        .git_global(spec.follow_gitignore)
        .git_exclude(spec.follow_gitignore)
        // `.gitignore` is honoured even outside a git repository: a project
        // root is often a subdirectory or a vendored tree, and `target/` is
        // just as unwelcome there.
        .require_git(false)
        .ignore(spec.follow_gitignore)
        // `hidden(true)` means "skip hidden entries". Inverted here: a project
        // opts OUT of its dot-directories, it does not have to opt in.
        .hidden(!spec.include_hidden)
        .parents(spec.follow_gitignore)
        .follow_links(false)
        // `.git` is pruned structurally rather than by glob: an exclude pattern
        // still walks the thousands of loose objects before rejecting them.
        .filter_entry(|entry| entry.file_name() != std::ffi::OsStr::new(".git"))
        .build();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                outcome.warnings.push(format!("walk: {e}"));
                continue;
            }
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let abs = entry.path().to_path_buf();
        let Some(rel) = relative(root, &abs) else {
            continue;
        };
        if !extensions.is_empty() {
            let ext = rel
                .rsplit('.')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if !extensions.contains(&ext) {
                continue;
            }
        }
        if let Some(set) = &include
            && !set.is_match(&rel)
        {
            continue;
        }
        if let Some(set) = &exclude
            && set.is_match(&rel)
        {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                outcome.warnings.push(format!("{rel}: {e}"));
                continue;
            }
        };
        if spec.max_file_bytes > 0 && meta.len() > spec.max_file_bytes {
            outcome.warnings.push(format!(
                "{rel}: {} bytes exceeds max_file_bytes",
                meta.len()
            ));
            continue;
        }
        outcome.files.push(Candidate {
            lang: Lang::from_path(&rel),
            rel_path: rel,
            abs_path: abs,
            size: meta.len(),
            mtime_ns: mtime_ns(&meta),
        });
    }
    outcome.files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(outcome)
}

/// Compiles a glob list, or `None` when the list is empty.
fn build_globs(patterns: &[String]) -> Result<Option<globset::GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut b = globset::GlobSetBuilder::new();
    for pat in patterns {
        let glob =
            globset::Glob::new(pat).map_err(|e| Error::Invalid(format!("glob `{pat}`: {e}")))?;
        b.add(glob);
    }
    Ok(Some(
        b.build()
            .map_err(|e| Error::Invalid(format!("globs: {e}")))?,
    ))
}

/// Project-relative, `/`-separated path. `None` when `abs` escapes `root`.
pub fn relative(root: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    let s = rel.to_string_lossy().replace('\\', "/");
    if s.is_empty() { None } else { Some(s) }
}

/// mtime in nanoseconds, or `0` on platforms or filesystems that refuse it.
pub fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir_all(root.join("src")).expect("mkdir");
        fs::create_dir_all(root.join("target/debug")).expect("mkdir");
        fs::create_dir_all(root.join("docs")).expect("mkdir");
        fs::write(root.join("src/lib.rs"), "fn a() {}").expect("write");
        fs::write(root.join("src/big.rs"), "x".repeat(10_000)).expect("write");
        fs::write(root.join("docs/guide.md"), "# Guide").expect("write");
        fs::write(root.join("notes.txt"), "ignored by extension").expect("write");
        fs::write(root.join("target/debug/huge.rs"), "fn generated() {}").expect("write");
        fs::write(root.join(".gitignore"), "target/\n").expect("write");
        // Dot-directories carry real answers; dotfiles can carry secrets.
        fs::create_dir_all(root.join(".github/workflows")).expect("mkdir");
        fs::write(root.join(".github/workflows/ci.md"), "# CI").expect("write");
        fs::write(root.join(".env"), "SECRET=hunter2").expect("write");
        dir
    }

    fn spec() -> ProjectSpec {
        ProjectSpec {
            extensions: vec!["rs".into(), "md".into()],
            ..ProjectSpec::default()
        }
    }

    #[test]
    fn gitignored_directories_never_reach_the_model() {
        let dir = fixture();
        let out = walk(dir.path(), &spec()).expect("walk");
        let paths: Vec<_> = out.files.iter().map(|f| f.rel_path.as_str()).collect();
        assert!(paths.contains(&"src/lib.rs"), "got {paths:?}");
        assert!(paths.contains(&"docs/guide.md"), "got {paths:?}");
        assert!(
            !paths.iter().any(|p| p.starts_with("target/")),
            "got {paths:?}"
        );
        assert!(!paths.contains(&"notes.txt"), "extension filter: {paths:?}");
    }

    #[test]
    fn disabling_gitignore_exposes_the_build_directory() {
        let dir = fixture();
        let s = ProjectSpec {
            follow_gitignore: false,
            ..spec()
        };
        let out = walk(dir.path(), &s).expect("walk");
        assert!(out.files.iter().any(|f| f.rel_path.starts_with("target/")));
    }

    #[test]
    fn include_narrows_and_exclude_subtracts() {
        let dir = fixture();
        let s = ProjectSpec {
            include: vec!["src/**".into()],
            exclude: vec!["**/big.rs".into()],
            ..spec()
        };
        let out = walk(dir.path(), &s).expect("walk");
        let paths: Vec<_> = out.files.iter().map(|f| f.rel_path.as_str()).collect();
        assert_eq!(paths, vec!["src/lib.rs"]);
    }

    #[test]
    fn oversized_files_are_skipped_with_a_warning_not_dropped_silently() {
        let dir = fixture();
        let s = ProjectSpec {
            max_file_bytes: 100,
            ..spec()
        };
        let out = walk(dir.path(), &s).expect("walk");
        assert!(out.files.iter().all(|f| f.rel_path != "src/big.rs"));
        assert!(
            out.warnings.iter().any(|w| w.contains("big.rs")),
            "got {:?}",
            out.warnings
        );
    }

    #[test]
    fn results_are_sorted_so_a_sync_is_reproducible() {
        let dir = fixture();
        let a = walk(dir.path(), &spec()).expect("walk");
        let b = walk(dir.path(), &spec()).expect("walk");
        assert_eq!(a.files, b.files);
        assert!(a.files.windows(2).all(|w| w[0].rel_path <= w[1].rel_path));
    }

    #[test]
    fn a_missing_root_is_an_invalid_request_not_a_panic() {
        let err = walk(Path::new("/definitely/not/here"), &spec()).expect_err("must fail");
        assert_eq!(err.code(), "invalid");
    }

    #[test]
    fn dot_directories_are_indexed_by_default() {
        let dir = fixture();
        let out = walk(dir.path(), &spec()).expect("walk");
        let paths: Vec<_> = out.files.iter().map(|f| f.rel_path.as_str()).collect();
        assert!(
            paths.contains(&".github/workflows/ci.md"),
            "workflows and .claude/docs are exactly what a developer asks about: {paths:?}"
        );
    }

    #[test]
    fn a_project_can_opt_out_of_hidden_entries() {
        let dir = fixture();
        let s = ProjectSpec {
            include_hidden: false,
            ..spec()
        };
        let out = walk(dir.path(), &s).expect("walk");
        assert!(out.files.iter().all(|f| !f.rel_path.starts_with('.')));
    }

    #[test]
    fn the_default_excludes_keep_secrets_out_of_the_index() {
        let dir = fixture();
        // The shipped defaults, not the bare spec used by the other tests.
        let defaults = findery_core::config::ProjectDefaults::default();
        let s = ProjectSpec {
            extensions: Vec::new(), // index everything, the worst case
            exclude: defaults.exclude.clone(),
            include_hidden: true,
            ..spec()
        };
        let out = walk(dir.path(), &s).expect("walk");
        let paths: Vec<_> = out.files.iter().map(|f| f.rel_path.as_str()).collect();
        assert!(
            !paths.contains(&".env"),
            "a dotfile with secrets must stay out: {paths:?}"
        );
        assert!(paths.contains(&".github/workflows/ci.md"), "{paths:?}");
    }

    #[test]
    fn a_malformed_glob_is_refused_up_front() {
        let dir = fixture();
        let s = ProjectSpec {
            include: vec!["[".into()],
            ..spec()
        };
        assert!(walk(dir.path(), &s).is_err());
    }
}
