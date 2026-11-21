//! Planning a pass before doing it: the walk, then gate 1 over the whole list.
//!
//! INTENT: gate 1 is a map lookup per file — microseconds for the whole tree —
//! so running it up front costs nothing and buys the honest denominator: how
//! many files are left to read is known before the first one is read. A bar
//! over every walked file would race through the unchanged ones and crawl
//! through the rest.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use findery_core::Result;
use findery_core::api::ProgressPhase;
use findery_core::model::{FileRecord, Project, SyncMode};

use crate::jobs::progress::ProgressHandle;
use crate::sanitize_rel;
use crate::storage::blocking::blocking;
use crate::walker::{self, Candidate};

/// What a pass will do, decided before it reads anything.
pub(super) struct Plan {
    /// Every path the walk accepted: what pruning keeps.
    pub(super) seen: HashSet<String>,
    /// The files gate 1 could not clear, in walk order.
    pub(super) pending: Vec<Candidate>,
    /// The files gate 1 cleared.
    pub(super) unchanged: u64,
}

impl Plan {
    /// Splits the walk with gate 1. `full` clears nothing: every file is read.
    pub(super) fn new(
        candidates: Vec<Candidate>,
        known: &HashMap<String, FileRecord>,
        full: bool,
    ) -> Self {
        let seen = candidates.iter().map(|c| c.rel_path.clone()).collect();
        let total = candidates.len();
        let pending: Vec<Candidate> = candidates
            .into_iter()
            .filter(|c| full || !untouched(c, known.get(&c.rel_path)))
            .collect();
        Self {
            seen,
            unchanged: (total - pending.len()) as u64,
            pending,
        }
    }

    /// Publishes the plan: the phase and every denominator, in one write.
    pub(super) fn publish(&self, progress: &ProgressHandle) {
        progress.write(|p| {
            p.phase = ProgressPhase::Indexing;
            p.files_total = self.seen.len() as u64;
            p.files_unchanged = self.unchanged;
            p.files_pending = self.pending.len() as u64;
        });
    }
}

/// Gate 1: same size, same mtime. An mtime of zero is no evidence at all.
fn untouched(candidate: &Candidate, previous: Option<&FileRecord>) -> bool {
    previous.is_some_and(|prev| {
        prev.size == candidate.size
            && prev.mtime_ns == candidate.mtime_ns
            && candidate.mtime_ns != 0
    })
}

/// The files a mode asks about, and the warnings found on the way. The walk
/// streams its count into `progress` as it goes.
pub(super) async fn candidates(
    project: &Project,
    mode: &SyncMode,
    progress: &ProgressHandle,
) -> Result<(Vec<Candidate>, Vec<String>)> {
    let (files, warnings) = match mode {
        SyncMode::Paths(paths) => targeted(project, paths).await?,
        _ => walked(project, progress).await?,
    };
    progress.write(|p| p.files_seen = files.len() as u64);
    Ok((files, warnings))
}

/// Every file the project's walk accepts.
async fn walked(
    project: &Project,
    progress: &ProgressHandle,
) -> Result<(Vec<Candidate>, Vec<String>)> {
    let (root, spec) = (PathBuf::from(&project.spec.root), project.spec.clone());
    // A clone of the handle, not of the counters: the walk runs on the blocking
    // pool and needs an owned way to report.
    let seen = progress.clone();
    let outcome =
        blocking(move || walker::walk_with(&root, &spec, |n| seen.write(|p| p.files_seen = n)))
            .await?;
    Ok((outcome.files, outcome.warnings))
}

/// The candidates of a targeted sync: each path sanitized, then admitted only
/// when the project's walk would have admitted it — `.gitignore`, the
/// excludes, hidden entries, the size ceiling and `.findery` bind a named path
/// as they bind the walk. Each refusal is a warning.
async fn targeted(project: &Project, paths: &[String]) -> Result<(Vec<Candidate>, Vec<String>)> {
    let mut warnings = Vec::new();
    let mut wanted = Vec::with_capacity(paths.len());
    for raw in paths {
        match sanitize_rel(raw) {
            Ok(rel) => wanted.push(rel),
            Err(e) => warnings.push(format!("{raw}: rejected, {}", e.detail())),
        }
    }
    let (root, spec) = (PathBuf::from(&project.spec.root), project.spec.clone());
    let outcome = blocking(move || walker::walk_paths(&root, &spec, &wanted)).await?;
    warnings.extend(outcome.warnings);
    Ok((outcome.files, warnings))
}

#[cfg(test)]
mod tests {
    use findery_core::ids::ContentHash;
    use findery_core::model::{Lang, Origin};

    use super::*;

    /// Candidates whose files do not exist: a plan that read anything would
    /// fail, so a plan that succeeds read nothing.
    fn ghost(rel: &str, size: u64, mtime_ns: i64) -> Candidate {
        Candidate {
            rel_path: rel.into(),
            abs_path: PathBuf::from("/nowhere/at/all").join(rel),
            size,
            mtime_ns,
            lang: Lang::from_path(rel),
        }
    }

    fn record(rel: &str, size: u64, mtime_ns: i64) -> (String, FileRecord) {
        let record = FileRecord {
            path: rel.into(),
            lang: Lang::from_path(rel),
            origin: Origin::File,
            content_hash: ContentHash::of(rel.as_bytes()),
            size,
            mtime_ns,
            chunk_count: 1,
            indexed_at: 0,
        };
        (rel.into(), record)
    }

    fn known() -> HashMap<String, FileRecord> {
        [
            record("a.rs", 10, 5),
            record("b.rs", 20, 5),
            record("c.rs", 30, 0),
        ]
        .into_iter()
        .collect()
    }

    fn walked() -> Vec<Candidate> {
        vec![
            ghost("a.rs", 10, 5), // untouched
            ghost("b.rs", 21, 5), // grew
            ghost("c.rs", 30, 0), // no mtime: no evidence
            ghost("d.rs", 40, 5), // new
        ]
    }

    #[test]
    fn the_totals_are_known_before_the_first_file_is_read() {
        let plan = Plan::new(walked(), &known(), false);
        assert_eq!(plan.seen.len(), 4);
        assert_eq!(plan.unchanged, 1);
        let pending: Vec<&str> = plan.pending.iter().map(|c| c.rel_path.as_str()).collect();
        assert_eq!(pending, ["b.rs", "c.rs", "d.rs"]);

        let progress = ProgressHandle::detached();
        plan.publish(&progress);
        let p = progress.snapshot();
        assert_eq!(p.phase, ProgressPhase::Indexing);
        assert_eq!(
            (p.files_total, p.files_unchanged, p.files_pending),
            (4, 1, 3)
        );
        assert_eq!(p.files_done, 0);
    }

    #[test]
    fn a_full_pass_clears_nothing_at_gate_one() {
        let plan = Plan::new(walked(), &known(), true);
        assert_eq!(plan.unchanged, 0);
        assert_eq!(plan.pending.len(), 4);
    }

    #[tokio::test]
    async fn a_targeted_path_that_escapes_the_root_is_a_warning_not_a_candidate() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("ok.rs"), "fn a() {}").expect("write");
        let project = Project {
            id: findery_core::ids::ProjectId::new("p").expect("id"),
            name: "p".into(),
            bucket: findery_core::model::DEFAULT_BUCKET.into(),
            model: None,
            spec: findery_core::model::ProjectSpec {
                root: dir.path().display().to_string(),
                ..Default::default()
            },
            host_root: String::new(),
            created_at: 0,
            updated_at: 0,
        };
        let paths = [
            "ok.rs".to_string(),
            "../etc/passwd".into(),
            "gone.rs".into(),
        ];
        let (files, warnings) = targeted(&project, &paths).await.expect("targeted");
        assert_eq!(files.len(), 1);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("escapes the project root"));
    }
}
