//! Which project a caller means when it names none: the manifest at or above
//! its directory, else the registered project whose root contains it.
//!
//! INTENT: the CLI and the MCP bridge both stand in a directory and both must
//! answer the same way, so the rule lives once, in the crate they share. The
//! manifest comes first because it is the only answer that holds offline, from
//! a subdirectory, and in a clone somebody else registered; the deepest root
//! wins next, so a project nested inside another is picked over its parent.

use std::path::{Path, PathBuf};

use findery_core::ids::ProjectId;
use findery_core::manifest::{MANIFEST_DIR, MANIFEST_FILE, ProjectConfig};
use findery_core::model::Project;
use findery_core::support::path_map;
use findery_core::{Error, Result};

use crate::Client;

/// A manifest found on disk, and the directory that holds it.
#[derive(Debug)]
pub struct FoundManifest {
    /// The project root: the directory containing `.findery/`.
    pub root: PathBuf,
    pub config: ProjectConfig,
}

/// The nearest manifest at or above `start`, if there is one.
///
/// A malformed manifest is an error and not a miss: going on would answer
/// about a different project than the one the caller stands in.
pub fn discover_manifest(start: &Path) -> Result<Option<FoundManifest>> {
    for dir in start.ancestors() {
        let candidate = dir.join(MANIFEST_DIR).join(MANIFEST_FILE);
        if candidate.is_file() {
            return Ok(Some(FoundManifest {
                root: dir.to_path_buf(),
                config: read_manifest(&candidate)?,
            }));
        }
    }
    Ok(None)
}

/// Reads and validates one manifest file; the error names the file.
pub fn read_manifest(path: &Path) -> Result<ProjectConfig> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| Error::Config(format!("cannot read {}: {e}", path.display())))?;
    let config = ProjectConfig::from_toml_str(&source)
        .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
    config.validate()?;
    Ok(config)
}

/// The registered project whose root contains `dir`, the deepest one first.
pub fn deepest_root(projects: &[Project], dir: &str) -> Option<ProjectId> {
    let dir = clean(dir);
    projects
        .iter()
        .filter(|p| path_map::is_within(&p.host_root, &dir))
        .max_by_key(|p| path_map::normalize(&p.host_root).len())
        .map(|p| p.id.clone())
}

/// `dir` with `/` separators and no Windows verbatim prefix (`//?/C:/…`).
pub fn clean(dir: &str) -> String {
    let text = path_map::normalize(dir);
    text.strip_prefix("//?/").unwrap_or(&text).to_string()
}

impl Client {
    /// The project a command applies to: `explicit` when given, else the
    /// manifest at or above `dir`, else the deepest registered root holding
    /// `dir`. Nothing matching is an error that lists the known projects and
    /// ends with `repair`, the caller's own way out.
    pub async fn resolve_project(
        &self,
        explicit: Option<&str>,
        dir: &Path,
        repair: &str,
    ) -> Result<ProjectId> {
        if let Some(raw) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
            return ProjectId::new(raw.to_string());
        }
        if let Some(found) = discover_manifest(dir)?
            && let Some(id) = found.config.project_id()
        {
            return Ok(id);
        }
        let shown = clean(&dir.to_string_lossy());
        let projects = self.list_projects().await?;
        deepest_root(&projects, &shown).ok_or_else(|| {
            Error::Invalid(format!(
                "no indexed project contains `{shown}`; {}\n  {repair}",
                known(&projects)
            ))
        })
    }
}

/// `known projects: `a` (C:/x), `b` (D:/y)`, or that there is none.
fn known(projects: &[Project]) -> String {
    if projects.is_empty() {
        return "no project is registered yet".to_string();
    }
    let listed: Vec<String> = projects
        .iter()
        .map(|p| format!("`{}` ({})", p.id, p.host_root))
        .collect();
    format!("known projects: {}", listed.join(", "))
}

#[cfg(test)]
mod tests {
    use findery_core::model::ProjectSpec;

    use super::*;

    fn project(id: &str, root: &str) -> Project {
        Project {
            id: ProjectId::new(id).expect("id"),
            name: id.into(),
            bucket: id.into(),
            model: None,
            spec: ProjectSpec::default(),
            host_root: root.into(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn write_manifest(root: &Path, body: &str) {
        let dir = root.join(MANIFEST_DIR);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(MANIFEST_FILE), body).expect("write");
    }

    #[test]
    fn a_nested_directory_finds_the_manifest_above_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let deep = dir.path().join("crates").join("core").join("src");
        std::fs::create_dir_all(&deep).expect("mkdir");
        write_manifest(dir.path(), "project = \"axle\"\n");

        let found = discover_manifest(&deep).expect("discover").expect("found");
        assert_eq!(found.root, dir.path());
        assert_eq!(found.config.project, "axle");
    }

    #[test]
    fn a_directory_with_no_manifest_is_a_miss_and_not_a_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(discover_manifest(dir.path()).expect("discover").is_none());
    }

    #[test]
    fn a_malformed_manifest_names_itself_instead_of_being_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_manifest(dir.path(), "projekt = \"typo\"\n");
        let err = discover_manifest(dir.path()).expect_err("must be refused");
        assert!(err.to_string().contains("config.toml"), "{err}");
    }

    #[test]
    fn the_deepest_registered_root_wins_and_a_sibling_never_matches() {
        let projects = [
            project("mono", "C:/work/mono"),
            project("core", "C:/work/mono/crates/core"),
            project("other", "C:/work/mono-old"),
        ];
        let pick = |dir: &str| deepest_root(&projects, dir).map(|id| id.to_string());
        assert_eq!(
            pick("C:\\work\\mono\\crates\\core\\src").as_deref(),
            Some("core")
        );
        assert_eq!(pick("//?/C:/work/mono/docs").as_deref(), Some("mono"));
        assert_eq!(pick("C:/work/mono-old2"), None);
    }

    #[test]
    fn the_no_match_message_lists_what_is_known_and_the_repair() {
        assert_eq!(known(&[]), "no project is registered yet");
        let listed = known(&[project("axle", "D:/axle")]);
        assert_eq!(listed, "known projects: `axle` (D:/axle)");
    }
}
