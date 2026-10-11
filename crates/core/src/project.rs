//! `.findery/config.toml` — the manifest of one project.
//!
//! INTENT: before this file existed, nothing on disk said which index a
//! directory belonged to. The answer lived in a daemon's database, so a cloned
//! repository arrived with no way to know what it was, and a command run in a
//! subdirectory had to ask the daemon and hope. A manifest beside the code makes
//! the project a thing you can read, commit, and carry — which is the whole
//! difference between a tool you operate and a service you consult.
//!
//! It holds two kinds of thing, and they are not the same kind:
//!
//! * a **request** — which bucket this project should live in. It is honoured at
//!   registration, and the daemon may substitute (a bucket inside the project is
//!   impossible when the daemon cannot see the disk). What the daemon answered
//!   is written back here, so the file never claims something untrue.
//! * **settings** — scopes and the default instruction, which are genuinely
//!   per-project and have no other home.
//!
//! It is never a source of truth for where a project's rows are: that is which
//! file holds them, and only the daemon knows it.
//!
//! Like [`crate::config`], this parses and validates but never opens a file.

use crate::config::{Config, StoreConfig, bucket_path};
use crate::filter::CompiledFilter;
use crate::ids::ProjectId;
use crate::model::{SearchFilter, is_bucket_name};
use crate::prompt::QueryTask;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The file name a project's manifest lives under, relative to the project root.
pub const MANIFEST_DIR: &str = ".findery";
/// The manifest's name inside [`MANIFEST_DIR`].
pub const MANIFEST_FILE: &str = "config.toml";

/// The project root-relative path of the manifest, for a message or a lookup.
pub fn manifest_path(root: &str) -> String {
    format!(
        "{}/{MANIFEST_DIR}/{MANIFEST_FILE}",
        root.trim_end_matches(['/', '\\'])
    )
}

/// Where a bucket lives.
///
/// INTENT: `path` wins when both are present, because naming a location is a
/// stronger statement than naming a thing. Neither present means the ordinary
/// case — a bucket inside the project — which is why the default needs no field.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BucketRef {
    /// A name in the daemon's bucket directory, shared by every project that
    /// names it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A database file. Relative paths resolve against the project root, so a
    /// committed manifest stays portable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl BucketRef {
    /// A bucket inside the project, which is what a fresh `init` asks for.
    pub fn local() -> Self {
        Self {
            path: Some(format!("{MANIFEST_DIR}/bucket.db")),
            ..Self::default()
        }
    }

    /// True when nothing was said, so the project-local default applies.
    pub fn is_unspecified(&self) -> bool {
        self.name.is_none() && self.path.is_none()
    }

    /// The `path`, resolved against `root` when it is relative.
    ///
    /// INTENT: a manifest is committed and read on another machine, so a
    /// relative path has to mean the same thing there. Resolving it here, once,
    /// keeps every caller from inventing its own idea of "relative to what".
    pub fn resolved_path(&self, root: &str) -> Option<String> {
        let path = self.path.as_deref()?;
        if is_absolute(path) {
            return Some(normalize_separators(path));
        }
        let base = root.trim_end_matches(['/', '\\']);
        Some(format!("{base}/{}", normalize_separators(path)))
    }

    fn validate(&self) -> Result<()> {
        if let Some(name) = &self.name
            && !is_bucket_name(name)
        {
            return Err(Error::Config(format!(
                "bucket.name: `{name}` is not a bucket name (letters, digits, `-`, `_`, `.`; \
                 must start with a letter or digit; at most 64 characters)"
            )));
        }
        if let Some(path) = &self.path
            && path.trim().is_empty()
        {
            return Err(Error::Config("bucket.path must not be empty".into()));
        }
        Ok(())
    }
}

/// True for a rooted path on either platform.
///
/// INTENT: `C:\shared\team.db` is as absolute as `/shared/team.db`, and treating
/// the first as relative silently produces `C:\work\axle\C:\shared\team.db` —
/// a path that exists nowhere, reported as a failure to create a directory.
fn is_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with('/')
        || path.starts_with('\\')
        || (bytes.len() >= 3 && bytes[1] == b':' && matches!(bytes[2], b'/' | b'\\'))
}

fn normalize_separators(path: &str) -> String {
    path.replace('\\', "/")
}

/// What a bucket reference resolves to: the name it is known by, and the file
/// that holds it.
///
/// A name is always present, even when the request gave only a path — a bucket
/// two projects point at has to be *one* bucket, and the name is what makes it
/// one. See [`BucketRef::resolve`] for how a path-only request gets one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedBucket {
    pub name: String,
    pub path: String,
    /// True when the request could not be honoured as written and the daemon
    /// chose a reachable bucket instead.
    ///
    /// INTENT: this is reported rather than refused, because a bucket inside a
    /// project the daemon cannot see is a fact about the deployment, not a
    /// mistake by the user — the same judgement that decides pull versus push.
    pub substituted: bool,
}

impl BucketRef {
    /// Resolves a request against the deployment that has to honour it.
    ///
    /// `root` is the project root **as the daemon sees it**, `reachable` says
    /// whether that root is on a filesystem the daemon can write to, and the
    /// four cases are:
    ///
    /// | request | name | file |
    /// |---|---|---|
    /// | nothing said | the project id | `<root>/.findery/bucket.db` |
    /// | `name` only | that name | `<data_dir>/buckets/<name>/bucket.db` |
    /// | `path` only | derived from the path | that path |
    /// | both | the name given | that path |
    ///
    /// An unreachable root sends the last three cases to the second one: a
    /// project-local or arbitrary path is impossible there, and the honest
    /// answer is a bucket where the daemon *can* write, named after the project,
    /// with `substituted` set so the manifest can record what really happened.
    pub fn resolve(
        &self,
        store: &StoreConfig,
        project: &ProjectId,
        root: &str,
        reachable: bool,
    ) -> Result<ResolvedBucket> {
        self.validate()?;
        let wanted = if let Some(path) = &self.path {
            let name = match &self.name {
                Some(name) => name.clone(),
                None => name_from_path(path)?,
            };
            let path = self
                .resolved_path(root)
                .ok_or_else(|| Error::Config("bucket.path must not be empty".into()))?;
            ResolvedBucket {
                name,
                path,
                substituted: false,
            }
        } else if let Some(name) = &self.name {
            ResolvedBucket {
                name: name.clone(),
                path: bucket_path(&store.data_dir, name),
                substituted: false,
            }
        } else {
            ResolvedBucket {
                name: project.to_string(),
                path: format!("{}/.findery/bucket.db", root.trim_end_matches(['/', '\\'])),
                substituted: false,
            }
        };

        // A bucket inside a project is only reachable when the daemon can see
        // that project at all. When it cannot, the file it was asked for is on
        // somebody else's disk.
        if reachable || self.name.is_some() && self.path.is_none() {
            return Ok(wanted);
        }
        Ok(ResolvedBucket {
            name: project.to_string(),
            path: bucket_path(&store.data_dir, project.as_str()),
            substituted: true,
        })
    }
}

/// A stable name for a bucket described only by its file.
///
/// INTENT: two projects pointing at the same file must reach the same bucket,
/// so the name has to be a function of the path and nothing else — no counter,
/// no clock, no registry lookup. Normalising first keeps `D:/x/team.db` and
/// `d:/x/team.db` from becoming two buckets over one file. The suffix keeps two
/// `team.db` in different directories apart without asking anyone to name them.
fn name_from_path(path: &str) -> Result<String> {
    let normalized = normalize_separators(path).to_lowercase();
    let file = normalized.rsplit('/').next().unwrap_or("bucket");
    let stem = file.strip_suffix(".db").unwrap_or(file);
    let digest = blake3::hash(normalized.as_bytes()).to_hex();
    Ok(ProjectId::slugify(&format!("{stem}-{}", &digest[..6]))?.to_string())
}

/// The manifest of one project.
///
/// INTENT: `deny_unknown_fields` is the same choice [`Config`] makes. A
/// misspelled key in a committed file is a setting that silently does nothing,
/// and the moment to say so is when it is written, not a month later when
/// someone wonders why their scope never applied.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectConfig {
    /// The registered id. Derived from the directory name when empty.
    pub project: String,
    /// Which bucket this project asks for. The daemon may substitute.
    pub bucket: BucketRef,
    /// Named, pre-scoring path restrictions over the SAME index.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub scopes: BTreeMap<String, SearchFilter>,
    /// Default retrieval intent for a query in this project.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<QueryTask>,
}

impl ProjectConfig {
    /// Parses a TOML document. An empty document yields the defaults, which
    /// describe an ordinary project-local bucket.
    pub fn from_toml_str(src: &str) -> Result<Self> {
        toml::from_str(src).map_err(|e| Error::Config(format!("invalid TOML: {e}")))
    }

    /// Renders the manifest, for `findery init` to write.
    pub fn to_toml_string(&self) -> Result<String> {
        toml::to_string_pretty(self)
            .map_err(|e| Error::Config(format!("cannot render the manifest: {e}")))
    }

    /// Rejects a manifest that would fail later, at the point where the failure
    /// is expensive and confusing.
    ///
    /// INTENT: scopes are compiled here rather than at search time (CONV-023). A
    /// malformed glob is a typo in a committed file, and the moment to name it is
    /// when it is written — not the first time someone searches with that scope
    /// and gets a 400 they cannot trace back to a line in a config file.
    pub fn validate(&self) -> Result<()> {
        if !self.project.is_empty() {
            ProjectId::new(self.project.clone())?;
        }
        self.bucket.validate()?;
        for (name, filter) in &self.scopes {
            if !is_bucket_name(name) {
                return Err(Error::Config(format!(
                    "scope `{name}`: a scope name takes letters, digits, `-`, `_` and `.`, \
                     starts with a letter or digit, and is at most 64 characters"
                )));
            }
            CompiledFilter::new(filter)
                .map_err(|e| Error::Config(format!("scope `{name}`: {e}")))?;
        }
        Ok(())
    }

    /// The project id, when the manifest names one.
    pub fn project_id(&self) -> Option<ProjectId> {
        ProjectId::new(self.project.clone()).ok()
    }

    /// The retrieval intent this project asks for by default.
    ///
    /// Mirrors [`crate::model::SearchRequest::resolved_instruction`], and lives
    /// here so the two levels of precedence cannot be resolved two ways.
    pub fn resolved_instruction(&self) -> Option<&str> {
        match (&self.instruction, self.task) {
            (Some(free), _) if !free.trim().is_empty() => Some(free),
            (_, Some(task)) => Some(task.instruction()),
            _ => None,
        }
    }
}

/// The default manifest a fresh `init` writes.
///
/// INTENT: written out rather than left empty, because a file whose every line
/// is commented is a file that teaches nothing. These four lines are the whole
/// vocabulary a user needs to change anything.
pub fn starter_manifest(project: &ProjectId, bucket: &BucketRef) -> ProjectConfig {
    ProjectConfig {
        project: project.to_string(),
        bucket: bucket.clone(),
        ..ProjectConfig::default()
    }
}

/// True when the daemon's own store configuration already covers this manifest's
/// bucket, so an `init` need not re-state it.
///
/// Kept next to the type rather than in the CLI: the comparison is between a
/// manifest and a configuration, and both live here.
pub fn matches_default_bucket(cfg: &Config, bucket: &BucketRef) -> bool {
    bucket.is_unspecified() || bucket.name.as_deref() == Some(cfg.store.default_bucket.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_id(raw: &str) -> ProjectId {
        ProjectId::new(raw).expect("id")
    }

    /// The whole vocabulary, read back exactly as written — top-level keys
    /// first, then the tables TOML requires them to precede.
    #[test]
    fn a_manifest_declaring_a_bucket_and_scopes_round_trips() {
        let src = r#"
project = "axle"
instruction = "Given a question about axle, retrieve the passage that answers it."

[bucket]
path = "D:/shared/team.db"

[scopes.api]
paths = ["crates/server/**"]
langs = ["rust"]

[scopes.docs]
paths = ["docs/**"]
"#;
        let parsed = ProjectConfig::from_toml_str(src).expect("parse");
        parsed.validate().expect("valid");
        assert_eq!(parsed.project, "axle");
        assert_eq!(
            parsed.bucket,
            BucketRef {
                name: None,
                path: Some("D:/shared/team.db".into())
            }
        );
        assert_eq!(parsed.scopes["api"].paths, vec!["crates/server/**"]);
        assert_eq!(parsed.scopes.len(), 2);
        assert!(parsed.resolved_instruction().is_some());

        let rendered = parsed.to_toml_string().expect("render");
        let again = ProjectConfig::from_toml_str(&rendered).expect("reparse");
        assert_eq!(again.project, parsed.project);
        assert_eq!(again.bucket, parsed.bucket);
        assert_eq!(again.scopes, parsed.scopes);
    }

    /// A typo in a committed file is a setting that does nothing. It has to be
    /// refused where it is read, not discovered a month later.
    #[test]
    fn an_unknown_key_is_refused_rather_than_ignored() {
        let err = ProjectConfig::from_toml_str("projekt = \"typo\"").expect_err("must be refused");
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    /// A scope is compiled when it is written: a bad glob is a typo in a file,
    /// and the moment to name it is while its author is looking at the file.
    #[test]
    fn a_scope_with_a_malformed_glob_is_refused_before_it_is_ever_used() {
        let manifest = ProjectConfig {
            project: "axle".into(),
            scopes: BTreeMap::from([(
                "api".to_string(),
                SearchFilter {
                    paths: vec!["crates/[unclosed".into()],
                    ..SearchFilter::default()
                },
            )]),
            ..ProjectConfig::default()
        };
        let err = manifest.validate().expect_err("must be refused");
        assert!(err.to_string().contains("scope `api`"), "{err}");
    }

    /// A scope name ends up on a command line, so it may not need quoting.
    #[test]
    fn a_scope_name_that_would_need_quoting_is_refused() {
        let manifest = ProjectConfig {
            project: "axle".into(),
            scopes: BTreeMap::from([("not a name".to_string(), SearchFilter::default())]),
            ..ProjectConfig::default()
        };
        let err = manifest.validate().expect_err("must be refused");
        assert!(err.to_string().contains("not a name"), "{err}");
    }

    /// A bucket name becomes a directory, so the same rule as everywhere else.
    #[test]
    fn a_bucket_name_that_could_escape_its_directory_is_refused() {
        let manifest = ProjectConfig {
            bucket: BucketRef {
                name: Some("../etc".into()),
                path: None,
            },
            ..ProjectConfig::default()
        };
        assert!(manifest.validate().is_err());
    }

    /// A relative bucket path means the same thing on every machine that clones
    /// the repository; an absolute one is taken as written.
    #[test]
    fn a_relative_bucket_path_resolves_against_the_project_root() {
        let relative = BucketRef {
            path: Some(".findery\\bucket.db".into()),
            ..BucketRef::default()
        };
        assert_eq!(
            relative.resolved_path("D:/work/axle").as_deref(),
            Some("D:/work/axle/.findery/bucket.db")
        );

        for absolute in [
            "D:/shared/team.db",
            "D:\\shared\\team.db",
            "/shared/team.db",
        ] {
            let ref_ = BucketRef {
                path: Some(absolute.into()),
                ..BucketRef::default()
            };
            let resolved = ref_.resolved_path("D:/work/axle").expect("resolved");
            assert!(
                !resolved.starts_with("D:/work/axle"),
                "`{absolute}` is rooted, not relative to the project: {resolved}"
            );
        }
    }

    fn store_at(data_dir: &str) -> StoreConfig {
        StoreConfig {
            data_dir: data_dir.to_string(),
            ..StoreConfig::default()
        }
    }

    /// The four cases of the resolution table, each one a decision the user can
    /// see in the file they wrote.
    #[test]
    fn a_bucket_request_resolves_to_a_name_and_a_file() {
        let store = store_at("C:/data");
        let project = project_id("axle");
        let root = "C:/work/axle";

        // Nothing said: a bucket inside the project, named after it.
        let local = BucketRef::default()
            .resolve(&store, &project, root, true)
            .expect("resolve");
        assert_eq!(local.name, "axle");
        assert_eq!(local.path, "C:/work/axle/.findery/bucket.db");
        assert!(!local.substituted);

        // A name only: shared, and somewhere the daemon owns.
        let named = BucketRef {
            name: Some("work".into()),
            ..BucketRef::default()
        }
        .resolve(&store, &project, root, true)
        .expect("resolve");
        assert_eq!(named.name, "work");
        assert_eq!(named.path, "C:/data/buckets/work/bucket.db");

        // Both: the name is the identity, the path is the location.
        let shared = BucketRef {
            name: Some("team".into()),
            path: Some("D:/shared/team.db".into()),
        }
        .resolve(&store, &project, root, true)
        .expect("resolve");
        assert_eq!(shared.name, "team");
        assert_eq!(shared.path, "D:/shared/team.db");

        // A path only: the name has to come from somewhere, and it must be the
        // same name for every project that points at that file.
        let path_only = BucketRef {
            path: Some("D:/shared/team.db".into()),
            ..BucketRef::default()
        };
        let first = path_only
            .resolve(&store, &project, root, true)
            .expect("resolve");
        let second = path_only
            .resolve(&store, &project_id("other"), "D:/work/other", true)
            .expect("resolve");
        assert_eq!(first.name, second.name, "one file is one bucket");
        assert!(first.name.starts_with("team-"), "{}", first.name);
        assert_eq!(first.path, "D:/shared/team.db");
    }

    /// A bucket inside a project is impossible when the daemon cannot see that
    /// project — a container, another machine. The request is honoured as far as
    /// it can be, and the substitution is flagged so the manifest can record
    /// what actually happened instead of what was asked.
    #[test]
    fn an_unreachable_project_falls_back_to_a_bucket_the_daemon_can_write() {
        let store = store_at("C:/data");
        let project = project_id("axle");

        let local = BucketRef::default()
            .resolve(&store, &project, "C:/work/axle", false)
            .expect("resolve");
        assert!(local.substituted);
        assert_eq!(local.name, "axle");
        assert_eq!(local.path, "C:/data/buckets/axle/bucket.db");

        let arbitrary = BucketRef {
            path: Some("D:/shared/team.db".into()),
            ..BucketRef::default()
        }
        .resolve(&store, &project, "C:/work/axle", false)
        .expect("resolve");
        assert!(arbitrary.substituted);

        // A named bucket lives where the daemon owns the files either way, so
        // there is nothing to substitute and nothing to report.
        let named = BucketRef {
            name: Some("work".into()),
            ..BucketRef::default()
        }
        .resolve(&store, &project, "C:/work/axle", false)
        .expect("resolve");
        assert!(!named.substituted);
        assert_eq!(named.name, "work");
    }

    /// Nothing said means the project-local bucket, which is what makes a fresh
    /// `init` need no decision from anyone.
    #[test]
    fn an_empty_manifest_describes_a_bucket_inside_the_project() {
        let manifest = ProjectConfig::from_toml_str("").expect("parse");
        manifest.validate().expect("valid");
        assert!(manifest.bucket.is_unspecified());
        assert_eq!(manifest.project_id(), None, "the id gets derived");
        assert_eq!(
            BucketRef::local().resolved_path("D:/work/axle").as_deref(),
            Some("D:/work/axle/.findery/bucket.db")
        );
        assert_eq!(
            manifest_path("D:/work/axle"),
            "D:/work/axle/.findery/config.toml"
        );
    }

    /// The starter manifest names the project and the bucket it asked for, and
    /// nothing else — a file whose lines are all guesses teaches bad habits.
    #[test]
    fn the_starter_manifest_states_what_was_decided_and_nothing_more() {
        let id = project_id("axle");
        let starter = starter_manifest(&id, &BucketRef::local());
        assert_eq!(starter.project, "axle");
        assert!(starter.scopes.is_empty());
        assert!(starter.resolved_instruction().is_none());
        let rendered = starter.to_toml_string().expect("render");
        let parsed = ProjectConfig::from_toml_str(&rendered).expect("parse");
        assert_eq!(parsed.bucket, starter.bucket);
    }

    /// The instruction precedence is resolved in one place, so a transport
    /// cannot invent its own ordering.
    #[test]
    fn the_project_instruction_beats_its_preset_and_a_blank_one_is_ignored() {
        let explicit = ProjectConfig {
            instruction: Some("Given a question, retrieve the answer.".into()),
            task: Some(QueryTask::Symbol),
            ..ProjectConfig::default()
        };
        assert_eq!(
            explicit.resolved_instruction(),
            Some("Given a question, retrieve the answer.")
        );

        let blank = ProjectConfig {
            instruction: Some("   ".into()),
            task: Some(QueryTask::Symbol),
            ..ProjectConfig::default()
        };
        assert_eq!(
            blank.resolved_instruction(),
            Some(QueryTask::Symbol.instruction())
        );

        assert_eq!(ProjectConfig::default().resolved_instruction(), None);
    }
}
