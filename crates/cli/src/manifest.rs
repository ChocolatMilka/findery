//! Finding a project's manifest, reading it, and writing it.
//!
//! The filesystem half of [`findery_core::project`], which parses but opens
//! nothing (CONV-006). Discovery walks up the way `package.json` lookup does.
//!
//! Only the CLI reads manifests: the MCP bridge names its project and lets the
//! daemon answer, which keeps the bridge a wire rather than a working directory.

use findery_core::Result;
use findery_core::model::Project;
use findery_core::project::{BucketRef, MANIFEST_DIR, MANIFEST_FILE, ProjectConfig};
use std::path::{Path, PathBuf};

/// A manifest found on disk, and the directory that holds it.
#[derive(Debug)]
pub(crate) struct Found {
    /// The project root: the directory containing `.findery/`.
    pub(crate) root: PathBuf,
    pub(crate) config: ProjectConfig,
}

/// The nearest manifest at or above `start`, if there is one.
///
/// A malformed manifest is an error and not a miss: searching on would answer
/// about a different project than the one asked about.
pub(crate) fn discover(start: &Path) -> Result<Option<Found>> {
    let mut current = Some(start);
    while let Some(dir) = current {
        let candidate = dir.join(MANIFEST_DIR).join(MANIFEST_FILE);
        if candidate.is_file() {
            return Ok(Some(Found {
                root: dir.to_path_buf(),
                config: read_file(&candidate)?,
            }));
        }
        current = dir.parent();
    }
    Ok(None)
}

/// The manifest's own path under a project root, for a message.
pub(crate) fn path_of(root: &Path) -> PathBuf {
    root.join(MANIFEST_DIR).join(MANIFEST_FILE)
}

/// The manifest at exactly this root, or the defaults.
///
/// `init` must not walk up: a manifest above the directory being registered
/// describes a different project.
pub(crate) fn read_at(root: &Path) -> Result<ProjectConfig> {
    let path = path_of(root);
    match path.is_file() {
        true => read_file(&path),
        false => Ok(ProjectConfig::default()),
    }
}

/// The manifest to record once the daemon has answered.
///
/// From the answer, not the request: a bucket inside the project is impossible
/// when the daemon cannot see the disk, and a manifest describing the request
/// would name an index that does not exist.
///
/// A bucket under the project is recorded relative to it, so the manifest can be
/// committed. One elsewhere keeps its absolute path — the name is the identity,
/// the path is only where it happens to be.
pub(crate) fn from_registration(
    root: &Path,
    project: &Project,
    bucket_path: &str,
) -> ProjectConfig {
    let relative = relative_to(root, bucket_path);
    ProjectConfig {
        project: project.id.to_string(),
        bucket: BucketRef {
            name: Some(project.bucket.clone()),
            path: relative
                .or_else(|| (!bucket_path.is_empty()).then(|| bucket_path.replace('\\', "/"))),
        },
        ..ProjectConfig::default()
    }
}

/// `path` expressed relative to `root`, when it is inside it.
fn relative_to(root: &Path, path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let root = root.display().to_string().replace('\\', "/");
    let path = path.replace('\\', "/");
    let prefix = format!("{}/", root.trim_end_matches('/'));
    path.strip_prefix(&prefix).map(str::to_string)
}

/// Keeps the bucket out of git without hiding the manifest: `.findery/` holds
/// one of each, so `*` alone would exclude the very file worth committing.
///
/// It sits inside `.findery/` because the project's own `.gitignore` is the
/// user's, and a tool that appends to it edits someone else's file.
const IGNORE: &str = "*\n!.gitignore\n!config.toml\n";

/// Writes the manifest, and the one line that keeps the index out of git.
pub(crate) fn write(root: &Path, config: &ProjectConfig) -> Result<PathBuf> {
    config.validate()?;
    let dir = root.join(MANIFEST_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| {
        findery_core::Error::Config(format!("cannot create {}: {e}", dir.display()))
    })?;
    let path = dir.join(MANIFEST_FILE);
    let rendered = format!(
        "# Written by `findery init`. Everything here can be edited by hand.\n\
         # Running `findery init` again rewrites it from what the daemon answered.\n\n{}",
        config.to_toml_string()?
    );
    std::fs::write(&path, rendered).map_err(|e| {
        findery_core::Error::Config(format!("cannot write {}: {e}", path.display()))
    })?;

    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(&ignore, IGNORE).map_err(|e| {
            findery_core::Error::Config(format!("cannot write {}: {e}", ignore.display()))
        })?;
    }
    Ok(path)
}

fn read_file(path: &Path) -> Result<ProjectConfig> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| findery_core::Error::Config(format!("cannot read {}: {e}", path.display())))?;
    ProjectConfig::from_toml_str(&source)
        .map_err(|e| findery_core::Error::Config(format!("{}: {e}", path.display())))
        .and_then(|config| {
            config.validate()?;
            Ok(config)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use findery_core::ids::ProjectId;
    use findery_core::project::BucketRef;

    fn starter() -> ProjectConfig {
        ProjectConfig {
            project: ProjectId::new("axle").expect("id").to_string(),
            bucket: BucketRef::local(),
            ..ProjectConfig::default()
        }
    }

    /// A command run in a subdirectory finds the project that owns it.
    #[test]
    fn a_nested_directory_finds_the_manifest_above_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let deep = dir.path().join("crates").join("core").join("src");
        std::fs::create_dir_all(&deep).expect("mkdir");
        write(dir.path(), &starter()).expect("write");

        let found = discover(&deep).expect("discover").expect("found");
        assert_eq!(found.root, dir.path());
        assert_eq!(found.config.project, "axle");
    }

    /// No manifest is a miss, not a failure: the caller falls back to asking the
    /// daemon which project contains this directory.
    #[test]
    fn a_directory_with_no_manifest_is_a_miss_and_not_a_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(discover(dir.path()).expect("discover").is_none());
    }

    /// The bucket the daemon answered with, relative when it lives inside the
    /// project: a committed file has to mean the same thing after a clone.
    #[test]
    fn a_registration_inside_the_project_is_recorded_as_a_relative_path() {
        let project = Project {
            id: ProjectId::new("axle").expect("id"),
            name: "axle".into(),
            bucket: "axle".into(),
            host_root: "D:/work/axle".into(),
            spec: findery_core::model::ProjectSpec::default(),
            created_at: 0,
            updated_at: 0,
        };
        let config = from_registration(
            Path::new("D:/work/axle"),
            &project,
            "D:/work/axle/.findery/bucket.db",
        );
        assert_eq!(config.project, "axle");
        assert_eq!(config.bucket.name.as_deref(), Some("axle"));
        assert_eq!(config.bucket.path.as_deref(), Some(".findery/bucket.db"));

        // Somewhere else keeps an absolute path: that is what makes it shared.
        let elsewhere = from_registration(Path::new("D:/work/axle"), &project, "D:/shared/team.db");
        assert_eq!(elsewhere.bucket.path.as_deref(), Some("D:/shared/team.db"));
    }

    /// The written file comes back identical, so `init` and every later read
    /// agree about what the project is.
    #[test]
    fn what_init_writes_is_what_gets_read_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let written = starter();
        let path = write(dir.path(), &written).expect("write");
        assert!(path.ends_with(".findery/config.toml"), "{path:?}");

        let back: ProjectConfig =
            ProjectConfig::from_toml_str(&std::fs::read_to_string(&path).expect("read"))
                .expect("parse");
        assert_eq!(back.project, written.project);
        assert_eq!(back.bucket, written.bucket);

        // The index is generated and stays out of the history; the manifest is
        // the point of the whole thing and must not.
        let ignore = dir.path().join(".findery").join(".gitignore");
        let rules = std::fs::read_to_string(&ignore).expect("ignore");
        assert!(rules.contains("!config.toml"), "{rules}");
        assert!(
            rules.contains("!/.gitignore") || rules.contains("!.gitignore"),
            "{rules}"
        );
        std::fs::write(&ignore, "# mine\n").expect("rewrite");
        write(dir.path(), &written).expect("rewrite manifest");
        assert_eq!(
            std::fs::read_to_string(&ignore).expect("ignore"),
            "# mine\n",
            "an ignore file that already exists is the user's"
        );
    }

    /// A manifest that cannot be parsed names itself. Searching on from a
    /// directory whose manifest is broken would answer about another project.
    #[test]
    fn a_malformed_manifest_names_itself_instead_of_being_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inner = dir.path().join(".findery");
        std::fs::create_dir_all(&inner).expect("mkdir");
        std::fs::write(inner.join("config.toml"), "projekt = \"typo\"\n").expect("write");

        let err = discover(dir.path()).expect_err("must be refused");
        let text = err.to_string();
        assert!(text.contains("config.toml"), "{text}");
    }

    /// A scope with an unusable glob is refused when the file is written, not
    /// the first time someone searches with it.
    #[test]
    fn a_manifest_that_would_fail_later_is_refused_before_it_is_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = starter();
        config.scopes.insert(
            "api".into(),
            findery_core::model::SearchFilter {
                paths: vec!["crates/[unclosed".into()],
                ..Default::default()
            },
        );
        assert!(write(dir.path(), &config).is_err());
        assert!(
            !dir.path().join(".findery").join("config.toml").exists(),
            "a refused manifest must not be half-written"
        );
    }
}
