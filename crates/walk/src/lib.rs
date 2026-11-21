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

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use findery_core::manifest::MANIFEST_DIR;
use findery_core::model::{Lang, ProjectSpec};
use findery_core::{Error, Result};

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

/// The path rules of a spec, compiled once: what a project admits before the
/// disk is asked anything.
///
/// INTENT: the one matcher behind every way a file can enter the index — the
/// full walk, a targeted sync, a watcher's touched paths. Three copies of
/// "include, then exclude, then extension" would disagree the first time one
/// of them is edited, and the one that disagrees is the one that indexes
/// `.env`.
pub struct Rules {
    include: Option<globset::GlobSet>,
    exclude: Option<globset::GlobSet>,
    extensions: Vec<String>,
    include_hidden: bool,
}

impl Rules {
    /// Compiles `spec`'s globs; a malformed one is refused, never ignored.
    pub fn new(spec: &ProjectSpec) -> Result<Self> {
        Ok(Self {
            include: build_globs(&spec.include)?,
            exclude: build_globs(&spec.exclude)?,
            extensions: spec
                .extensions
                .iter()
                .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
                .collect(),
            include_hidden: spec.include_hidden,
        })
    }

    /// True when the project-relative `rel` passes every rule that needs no
    /// disk: under no pruned directory, not hidden when hidden entries are off,
    /// an extension the project walks, inside its includes, outside its
    /// excludes. `.gitignore` and the size ceiling need the disk; the walk
    /// applies them.
    pub fn admits(&self, rel: &str) -> bool {
        let hidden = |segment: &str| segment.starts_with('.');
        if rel.split('/').any(is_pruned) {
            return false;
        }
        if !self.include_hidden && rel.split('/').any(hidden) {
            return false;
        }
        self.has_walked_extension(rel)
            && self.include.as_ref().is_none_or(|set| set.is_match(rel))
            && !self.exclude.as_ref().is_some_and(|set| set.is_match(rel))
    }

    fn has_walked_extension(&self, rel: &str) -> bool {
        if self.extensions.is_empty() {
            return true;
        }
        let ext = rel.rsplit('.').next().unwrap_or_default();
        self.extensions.contains(&ext.to_ascii_lowercase())
    }
}

/// True for a directory no walk enters: `.git`, and `.findery`.
///
/// INTENT: `.git` is pruned structurally rather than by glob, because an
/// exclude pattern still walks the thousands of loose objects before
/// rejecting them. `.findery` for a stronger reason: it holds the bucket that
/// indexes this very tree, and indexing it is a write loop — every sync would
/// see a file it had just written. A glob in the project's exclude list would
/// do, until someone deletes the line; this survives that.
pub fn is_pruned(segment: &str) -> bool {
    segment == ".git" || segment == MANIFEST_DIR
}

/// Walks `root` under the rules of `spec`.
///
/// Blocking: `ignore` is a synchronous crate. Callers run this inside
/// `spawn_blocking` rather than paying for an async filesystem abstraction the
/// operating system does not actually provide.
pub fn walk(root: &Path, spec: &ProjectSpec) -> Result<WalkOutcome> {
    walk_with(root, spec, |_| {})
}

/// [`walk`], telling `found` how many files it has accepted after each one.
///
/// INTENT: a walk over a large tree takes seconds, and a client polling the
/// job's progress with nothing to show for them reads it as a hang. The count
/// goes out as an argument, not through a handle this crate would have to
/// know.
pub fn walk_with(root: &Path, spec: &ProjectSpec, found: impl FnMut(u64)) -> Result<WalkOutcome> {
    walk_scoped(root, spec, Scope::Everything, found)
}

/// [`walk`], limited to `paths` (project-relative, already sanitized): each
/// one is accepted only when the full walk would have accepted it.
///
/// INTENT: a targeted sync is the same walk narrowed, not a second matcher —
/// so `.gitignore`, hidden entries, the excludes, the extensions, the size
/// ceiling and the pruned directories bind a watcher event or an
/// `add_document` exactly as they bind `findery sync`. Only the directories on
/// the way to a requested path are listed. A path the walk does not accept is
/// a warning saying why, never a silent skip.
pub fn walk_paths(root: &Path, spec: &ProjectSpec, paths: &[String]) -> Result<WalkOutcome> {
    let mut outcome = walk_scoped(root, spec, Scope::only(paths), |_| {})?;
    let mut answered: HashSet<&str> = outcome.files.iter().map(|f| f.rel_path.as_str()).collect();
    let mut refusals = Vec::new();
    for rel in paths {
        let warned = outcome
            .warnings
            .iter()
            .any(|w| w.starts_with(&format!("{rel}: ")));
        if answered.insert(rel.as_str()) && !warned {
            refusals.push(refusal(root, rel));
        }
    }
    outcome.warnings.extend(refusals);
    Ok(outcome)
}

/// Why the walk did not accept `rel`, as a warning.
fn refusal(root: &Path, rel: &str) -> String {
    match std::fs::metadata(root.join(rel)) {
        Ok(meta) if !meta.is_file() => format!("{rel}: not a file"),
        Ok(_) => format!(
            "{rel}: excluded by the project's walk rules (extensions, include/exclude, \
             .gitignore, hidden entries)"
        ),
        Err(e) => format!("{rel}: {e}"),
    }
}

/// Which entries a walk descends into.
enum Scope {
    Everything,
    /// The requested files, and the directories on the way to them.
    Only {
        files: HashSet<String>,
        dirs: HashSet<String>,
    },
}

impl Scope {
    fn only(paths: &[String]) -> Self {
        let dirs = paths
            .iter()
            .flat_map(|rel| {
                rel.match_indices('/')
                    .map(move |(at, _)| rel[..at].to_string())
            })
            .collect();
        Self::Only {
            files: paths.iter().cloned().collect(),
            dirs,
        }
    }

    fn keeps(&self, root: &Path, entry: &ignore::DirEntry) -> bool {
        let Self::Only { files, dirs } = self else {
            return true;
        };
        let Some(rel) = relative(root, entry.path()) else {
            return true;
        };
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            dirs.contains(&rel)
        } else {
            files.contains(&rel)
        }
    }
}

fn walk_scoped(
    root: &Path,
    spec: &ProjectSpec,
    scope: Scope,
    mut found: impl FnMut(u64),
) -> Result<WalkOutcome> {
    if !root.is_dir() {
        return Err(Error::Invalid(format!(
            "project root `{}` is not a directory",
            root.display()
        )));
    }
    let rules = Rules::new(spec)?;
    let scope_root = root.to_path_buf();
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
        .filter_entry(move |entry| {
            let pruned = entry.file_name().to_str().is_some_and(is_pruned);
            !pruned && scope.keeps(&scope_root, entry)
        })
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
        if !rules.admits(&rel) {
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
        found(outcome.files.len() as u64);
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
    use std::fs;

    use super::*;

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
    fn the_count_moves_once_per_accepted_file_and_ends_on_the_total() {
        let dir = fixture();
        let mut counts = Vec::new();
        let out = walk_with(dir.path(), &spec(), |n| counts.push(n)).expect("walk");
        let expected: Vec<u64> = (1..=out.files.len() as u64).collect();
        assert_eq!(counts, expected);
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

    /// The bug this locks: a targeted path bypassed every rule, so a watcher
    /// event on `.env` indexed it. Each rule binds a named path as it binds the
    /// walk, and each refusal says so.
    #[test]
    fn a_targeted_path_obeys_every_rule_the_walk_obeys() {
        let dir = fixture();
        fs::create_dir_all(dir.path().join(".findery")).expect("mkdir");
        fs::write(dir.path().join(".findery/bucket.db"), "x").expect("write");
        let defaults = findery_core::config::ProjectDefaults::default();
        let s = ProjectSpec {
            exclude: defaults.exclude.clone(),
            max_file_bytes: 100,
            ..spec()
        };
        let asked: Vec<String> = [
            "src/lib.rs",
            ".env",
            ".findery/bucket.db",
            "target/debug/huge.rs",
            "notes.txt",
            "src/big.rs",
            "src/gone.rs",
            "src",
        ]
        .iter()
        .map(|p| p.to_string())
        .collect();
        let out = walk_paths(dir.path(), &s, &asked).expect("walk");

        let paths: Vec<_> = out.files.iter().map(|f| f.rel_path.as_str()).collect();
        assert_eq!(paths, ["src/lib.rs"]);
        assert_eq!(out.warnings.len(), asked.len() - 1, "{:?}", out.warnings);
        for refused in &asked[1..] {
            assert!(
                out.warnings
                    .iter()
                    .any(|w| w.starts_with(&format!("{refused}: "))),
                "{refused}: {:?}",
                out.warnings
            );
        }
    }

    #[test]
    fn the_rules_refuse_pruned_and_hidden_paths_without_the_disk() {
        let s = ProjectSpec {
            include_hidden: false,
            ..spec()
        };
        let rules = Rules::new(&s).expect("rules");
        assert!(rules.admits("src/lib.rs"));
        assert!(!rules.admits(".findery/bucket.db"));
        assert!(!rules.admits("a/.git/x.rs"));
        assert!(!rules.admits(".github/ci.md"), "hidden entries are off");
        assert!(!rules.admits("notes.txt"), "not a walked extension");
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
