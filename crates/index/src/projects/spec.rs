//! The spec a registration writes: what the request names, merged over what
//! the project already declared, and the excludes that keep a bucket out of
//! the tree it indexes.
//!
//! INTENT: split from `projects.rs` (CONV-029). These are pure — no lookup, no
//! file — so the merge rule can be read, and tested, without an engine.

use findery_core::api::RegisterProject;
use findery_core::config::ProjectDefaults;
use findery_core::ids::ModelAlias;
use findery_core::model::ProjectSpec;

/// What a registration decided before its spec is merged.
pub(crate) struct Decided {
    pub root: String,
    pub push: bool,
    /// `None` keeps the current pin, `Some(None)` removes it.
    pub pin: Option<Option<ModelAlias>>,
}

/// The spec a registration writes: each field the request names, else the
/// project's current one, else the daemon's default.
pub(crate) fn merged_spec(
    defaults: &ProjectDefaults,
    req: RegisterProject,
    decided: Decided,
    previous: Option<&ProjectSpec>,
) -> ProjectSpec {
    let kept = |field: fn(&ProjectSpec) -> &Vec<String>| previous.map(field);
    ProjectSpec {
        root: decided.root,
        include: pick(req.include, kept(|p| &p.include), Vec::new),
        exclude: pick(req.exclude, kept(|p| &p.exclude), || {
            defaults.exclude.clone()
        }),
        extensions: pick(req.extensions, kept(|p| &p.extensions), || {
            defaults.extensions.clone()
        }),
        follow_gitignore: req
            .follow_gitignore
            .or(previous.map(|p| p.follow_gitignore))
            .unwrap_or(defaults.follow_gitignore),
        include_hidden: req
            .include_hidden
            .or(previous.map(|p| p.include_hidden))
            .unwrap_or(defaults.include_hidden),
        max_file_bytes: req
            .max_file_bytes
            .or(previous.map(|p| p.max_file_bytes))
            .unwrap_or(defaults.max_file_bytes),
        push: decided.push,
        // The project's own defaults, carried to the daemon so a transport
        // that never read the manifest can still resolve them.
        scopes: pick(req.scopes, previous.map(|p| &p.scopes), Default::default),
        instruction: match req.instruction {
            Some(text) if text.trim().is_empty() => None,
            Some(text) => Some(text),
            None => previous.and_then(|p| p.instruction.clone()),
        },
        task: req.task.or(previous.and_then(|p| p.task)),
        intents: pick(req.intents, previous.map(|p| &p.intents), Default::default),
        model: match decided.pin {
            Some(pin) => pin,
            None => previous.and_then(|p| p.model.clone()),
        },
    }
}

/// The value a request names, else the one kept, else the default.
fn pick<T: Clone>(asked: Option<T>, kept: Option<&T>, default: impl FnOnce() -> T) -> T {
    asked.or_else(|| kept.cloned()).unwrap_or_else(default)
}

/// The globs that keep a bucket out of the tree it indexes.
///
/// Two locks, both needed: the fixed pattern covers the ordinary case — the
/// walker prunes `.findery` structurally anyway — and the resolved path covers
/// a bucket pointed at an arbitrary location *inside* the root, which no fixed
/// pattern can name. A bucket is a family of files, `{stem}.db` and
/// `{stem}.<slug>-<tag>.db`, each with its journals, so the patterns cover the
/// stem and not one file name.
pub(crate) fn exclusions_for(bucket_path: &str, root: &str) -> Vec<String> {
    let mut patterns = vec![".findery/**".to_string(), "**/.findery/**".to_string()];
    let root = root.trim_end_matches(['/', '\\']);
    let normalized = bucket_path.replace('\\', "/");
    let prefix = format!("{}/", root.replace('\\', "/"));
    if let Some(rel) = normalized.strip_prefix(&prefix)
        && !rel.is_empty()
    {
        let (dir, file) = match rel.rsplit_once('/') {
            Some((dir, file)) => (format!("{dir}/"), file),
            None => (String::new(), rel),
        };
        let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
        // `{stem}.db*` is the legacy file and its journals; `{stem}.*.db*` is
        // every member and theirs. A base with another extension keeps its own.
        patterns.push(format!("{dir}{file}*"));
        patterns.push(format!("{dir}{stem}.*.db*"));
    }
    patterns
}

/// Last path segment of a `/`-separated path.
pub(crate) fn basename(path: &str) -> Option<String> {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use globset::Glob;

    use super::*;

    #[test]
    fn basename_survives_trailing_slashes() {
        assert_eq!(basename("/workspaces/axle/"), Some("axle".into()));
        assert_eq!(basename("/"), None);
    }

    /// Every file a bucket inside the tree can grow — the legacy base, each
    /// member, and their journals — is excluded; the user's files beside it are
    /// not.
    #[test]
    fn a_bucket_inside_the_root_excludes_every_member_and_journal() {
        let patterns = exclusions_for("/w/p/index/team.db", "/w/p");
        let matches = |rel: &str| {
            patterns
                .iter()
                .any(|p| Glob::new(p).expect("glob").compile_matcher().is_match(rel))
        };
        for excluded in [
            "index/team.db",
            "index/team.db-wal",
            "index/team.harrier-oss-v1-0-6b-zvpmtsdf.db",
            "index/team.harrier-oss-v1-0-6b-zvpmtsdf.db-shm",
        ] {
            assert!(
                matches(excluded),
                "{excluded} must be excluded: {patterns:?}"
            );
        }
        for kept in ["index/teams.rs", "index/notes.md", "team.db"] {
            assert!(!matches(kept), "{kept} is the user's: {patterns:?}");
        }
    }

    #[test]
    fn a_bucket_outside_the_root_adds_only_the_fixed_patterns() {
        assert_eq!(exclusions_for("/data/bucket.db", "/w/p").len(), 2);
    }
}
