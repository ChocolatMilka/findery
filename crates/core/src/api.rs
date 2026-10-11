//! The wire vocabulary: what clients send and what the daemon answers.
//!
//! INTENT: these types live in L0, not in the server, so the HTTP transport,
//! the Rust client, the MCP bridge and the CLI all compile against the SAME
//! definitions. A field added here reaches every consumer or breaks their build
//! — which is exactly the failure mode we want, instead of a client silently
//! dropping a field the server started sending.

use crate::model::{SearchFilter, SyncMode, SyncReport};
use crate::project::BucketRef;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// `POST /v1/projects` — register or update a project.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RegisterProject {
    /// Slug; derived from `name`, or from the root, when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Project root as a HOST path. Translated through the configured mounts,
    /// so a client never has to know the daemon runs in a container.
    pub root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_gitignore: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_hidden: Option<bool>,
    /// Force push mode instead of letting the daemon test the path.
    ///
    /// The automatic choice is right almost always; this exists for the cases
    /// it cannot see — a stale mount, a daemon that will move, or a client that
    /// simply prefers to own the reading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_file_bytes: Option<u64>,
    /// Which bucket should hold this project.
    ///
    /// Chosen once, at registration: every later command follows the project, so
    /// nobody has to remember which index a directory belongs to. Absent means
    /// the ordinary case — a bucket inside the project, named after it. The
    /// daemon answers with what it actually used, which is not always what was
    /// asked (see [`crate::project::BucketRef::resolve`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<BucketRef>,
    /// Named restrictions a search can ask for by name.
    ///
    /// Sent at registration because the daemon has to resolve the name for a
    /// client that never read the manifest — the MCP bridge is a separate
    /// process and does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<BTreeMap<String, SearchFilter>>,
    /// Default retrieval intent for a query in this project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<crate::prompt::QueryTask>,
}

impl RegisterProject {
    /// The minimal registration: just a root.
    pub fn new(root: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            ..Self::default()
        }
    }
}

/// One bucket, as `GET /v1/buckets` reports it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BucketInfo {
    pub name: String,
    /// Absolute path of the database file.
    pub path: String,
    /// The recipe this bucket was built with; `None` while it holds no vectors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<String>,
    /// Why this bucket cannot serve a vector query, when it cannot.
    ///
    /// INTENT: a recipe mismatch is reported here rather than raised at boot,
    /// so the daemon that owns the bucket stays up and can still delete it —
    /// a conflict you can read about and repair, not a conflict that locks the
    /// door you would repair it through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict: Option<String>,
    pub projects: u64,
    pub chunks: u64,
    pub disk_bytes: u64,
    /// True when a project registering without naming one lands here.
    pub is_default: bool,
}

/// `POST /v1/buckets` — create a bucket.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CreateBucket {
    pub name: String,
}

/// `DELETE /v1/buckets/{name}` — what the removal swept away.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BucketRemoved {
    pub removed: bool,
    /// Projects that were registered in it, and went with it.
    pub projects_removed: u64,
    /// Their ids, so a caller holding per-project state — a watch, a job — can
    /// let go of it.
    #[serde(default)]
    pub projects: Vec<String>,
}

/// `GET /v1/health` — liveness of the daemon and of what it depends on.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HealthReport {
    pub ok: bool,
    /// Version of the daemon binary that answered.
    ///
    /// INTENT: a CLI talking to an older daemon cannot tell it from a broken
    /// one — the symptom is a 404 on a route this build knows exists, which
    /// points nowhere. The field is itself recent, so a daemon that reports
    /// NOTHING is the older one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    pub model: String,
    pub dim: usize,
    pub store_backend: String,
    pub projects: usize,
    pub embedder_ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Lifecycle of a background job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Succeeded,
    Failed,
}

impl JobState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }
}

/// What a running job is doing, in the order the phases happen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressPhase {
    /// Walking the project. The denominator does not exist yet.
    #[default]
    Scanning,
    /// Reading, chunking and embedding what the walk found.
    Indexing,
    /// Dropping what the walk no longer sees.
    Pruning,
}

impl ProgressPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Scanning => "scanning",
            Self::Indexing => "indexing",
            Self::Pruning => "pruning",
        }
    }
}

/// Live counters of a running job. An ingestion that runs for minutes in
/// silence is indistinguishable from one that has hung, and the fix is a
/// denominator — which the walk supplies before the first byte is read.
///
/// A terminal job carries none: its numbers are its [`SyncReport`], and two
/// records of one fact are free to disagree.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Progress {
    pub phase: ProgressPhase,
    pub files_done: u64,
    /// Zero while [`ProgressPhase::Scanning`]: the walk is what establishes it.
    pub files_total: u64,
    pub chunks_done: u64,
    pub embeddings_computed: u64,
    pub embeddings_cached: u64,
    pub elapsed_ms: u64,
}

impl Progress {
    /// How far along the walk is, in `[0, 1]`. `0 / 0` is not a proportion, and
    /// the total stays zero until the walk returns.
    pub fn fraction(&self) -> f32 {
        if self.files_total == 0 {
            return 0.0;
        }
        (self.files_done as f32 / self.files_total as f32).clamp(0.0, 1.0)
    }

    /// The one-line description a terminal, a log or an agent can print.
    pub fn summary(&self) -> String {
        let mut out = format!(
            "{} · {}/{} files · {} chunks",
            self.phase.as_str(),
            self.files_done,
            self.files_total,
            self.chunks_done
        );
        if self.embeddings_computed > 0 || self.embeddings_cached > 0 {
            out.push_str(&format!(
                " · {} embedded, {} cached",
                self.embeddings_computed, self.embeddings_cached
            ));
        }
        out.push_str(&format!(" · {:.1}s", self.elapsed_ms as f64 / 1000.0));
        out
    }
}

/// One unit of background work, as clients observe it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub project: String,
    /// `incremental`, `full` or `paths`.
    pub kind: String,
    pub state: JobState,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    /// Counters of a job that is still running; absent before and after.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<Progress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<SyncReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `POST /v1/projects/{id}/sync`
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SyncRequest {
    /// `incremental` (default), `full`, or `paths` together with `paths`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Paths to sync; implies `mode = "paths"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    /// Block until the sync finishes and return the report inline. Convenient
    /// from a CLI, a bad idea from an MCP client facing a cold index.
    #[serde(default)]
    pub wait: bool,
}

impl SyncRequest {
    /// Resolves the request into a [`SyncMode`].
    pub fn resolve(&self) -> Result<SyncMode> {
        if let Some(paths) = &self.paths
            && !paths.is_empty()
        {
            return Ok(SyncMode::Paths(paths.clone()));
        }
        match self.mode.as_deref() {
            None | Some("incremental") => Ok(SyncMode::Incremental),
            Some("full") => Ok(SyncMode::Full),
            Some("paths") => Err(Error::Invalid(
                "mode `paths` requires a non-empty `paths` list".into(),
            )),
            Some(other) => Err(Error::Invalid(format!(
                "unknown sync mode `{other}`; expected incremental, full or paths"
            ))),
        }
    }
}

/// Either an accepted job or a finished report, depending on `wait`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SyncResponse {
    Completed { report: SyncReport },
    Accepted { job: Job },
}

impl SyncResponse {
    /// The report, when the call was synchronous.
    pub fn report(&self) -> Option<&SyncReport> {
        match self {
            Self::Completed { report } => Some(report),
            Self::Accepted { .. } => None,
        }
    }

    /// The job, when the call was asynchronous.
    pub fn job(&self) -> Option<&Job> {
        match self {
            Self::Accepted { job } => Some(job),
            Self::Completed { .. } => None,
        }
    }
}

/// `POST /v1/projects/{id}/documents`
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AddDocument {
    /// Path relative to the project root.
    pub path: String,
    /// Inline content. When omitted the file is read from disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// `POST /v1/projects/{id}/documents/batch` — push several documents at once.
///
/// INTENT: a first index of a large repository is thousands of files. One HTTP
/// request each, even on loopback, spends minutes in round trips.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PushDocuments {
    pub documents: Vec<AddDocument>,
}

/// `POST /v1/projects/{id}/prune` — delete pushed files the client no longer has.
///
/// The client sends every path it saw; whatever the bucket holds beyond that
/// list disappeared on its side.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PruneRequest {
    pub keep: Vec<String>,
}

/// `POST /v1/projects/{id}/watch`
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WatchRequest {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_debounce")]
    pub debounce_ms: u64,
}

impl Default for WatchRequest {
    fn default() -> Self {
        Self {
            enabled: true,
            debounce_ms: default_debounce(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_debounce() -> u64 {
    1_500
}

/// `POST /v1/projects/{id}/export`
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExportRequest {
    /// `jsonl` (default), `json` or `binary`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Destination directory, host-side. Defaults to `store.export_dir`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

/// What an export produced. Returned by `POST /v1/projects/{id}/export`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportReport {
    pub project: String,
    pub format: String,
    /// Directory the files landed in, as the daemon sees it.
    pub directory: String,
    pub files: Vec<String>,
    pub chunks: u64,
    /// Chunks whose vector was found; the rest were never embedded under this
    /// recipe and are reported rather than silently dropped.
    pub vectors: u64,
    pub dim: usize,
    pub bytes: u64,
}

/// Uniform error body. Clients branch on `code`, humans read `message`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

/// Envelope for endpoints returning a plain count.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Removed {
    pub removed: u64,
}

/// Envelope for `DELETE /v1/projects/{id}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Deleted {
    pub deleted: bool,
}

/// Envelope for `POST /v1/shutdown`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Stopping {
    pub stopping: bool,
}

/// Envelope for watch toggling.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Watching {
    pub project: String,
    pub watching: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_sync_is_incremental() {
        let req: SyncRequest = serde_json::from_str("{}").expect("parse");
        assert_eq!(req.resolve().expect("mode"), SyncMode::Incremental);
        assert!(!req.wait);
    }

    #[test]
    fn a_path_list_wins_over_the_mode_field() {
        let req: SyncRequest =
            serde_json::from_str(r#"{"mode":"full","paths":["src/a.rs"]}"#).expect("parse");
        assert_eq!(
            req.resolve().expect("mode"),
            SyncMode::Paths(vec!["src/a.rs".into()])
        );
    }

    #[test]
    fn an_unknown_mode_is_refused_rather_than_defaulted() {
        let req: SyncRequest = serde_json::from_str(r#"{"mode":"turbo"}"#).expect("parse");
        assert_eq!(req.resolve().expect_err("must fail").code(), "invalid");
        let req: SyncRequest = serde_json::from_str(r#"{"mode":"paths"}"#).expect("parse");
        assert!(req.resolve().is_err());
    }

    #[test]
    fn unknown_fields_are_rejected_so_typos_are_not_silent() {
        assert!(serde_json::from_str::<SyncRequest>(r#"{"waitt":true}"#).is_err());
        assert!(serde_json::from_str::<AddDocument>(r#"{"path":"a","txt":"x"}"#).is_err());
    }

    #[test]
    fn the_sync_response_discriminates_both_shapes() {
        let completed: SyncResponse =
            serde_json::from_str(r#"{"report":{"project":"p","mode":"full"}}"#).expect("parse");
        assert!(completed.report().is_some());
        assert!(completed.job().is_none());

        let accepted: SyncResponse = serde_json::from_str(
            r#"{"job":{"id":"1","project":"p","kind":"incremental","state":"queued","created_at":0}}"#,
        )
        .expect("parse");
        assert_eq!(accepted.job().expect("job").state, JobState::Queued);
        assert!(accepted.report().is_none());
    }

    /// A job read from an older daemon has no `progress` key at all, and that
    /// must deserialize rather than fail: a client newer than its server is the
    /// ordinary case during a rolling upgrade.
    #[test]
    fn a_job_without_progress_still_deserializes() {
        let job: Job = serde_json::from_str(
            r#"{"id":"1","project":"p","kind":"full","state":"running","created_at":0}"#,
        )
        .expect("parse");
        assert!(job.progress.is_none());
    }

    /// The denominator arrives with the walk, so a bar drawn before it returns
    /// has to read 0% — not a panic, and not a jump to 100%.
    #[test]
    fn progress_without_a_denominator_is_zero_not_a_division_by_zero() {
        let scanning = Progress::default();
        assert_eq!(scanning.fraction(), 0.0);
        assert!(scanning.summary().contains("scanning"));
    }

    /// Progress never reports more than everything: a finished file is counted
    /// before the walk length is known to have changed, and a bar that overflows
    /// its own length reads as a bug in whatever is drawing it.
    #[test]
    fn progress_never_exceeds_one_and_says_what_it_is_doing() {
        let indexing = Progress {
            phase: ProgressPhase::Indexing,
            files_done: 150,
            files_total: 100,
            chunks_done: 40,
            embeddings_computed: 30,
            embeddings_cached: 10,
            elapsed_ms: 1500,
        };
        assert_eq!(indexing.fraction(), 1.0);
        let text = indexing.summary();
        assert!(text.contains("indexing"), "{text}");
        assert!(text.contains("150/100 files"), "{text}");
        assert!(text.contains("30 embedded"), "{text}");
        assert!(text.contains("1.5s"), "{text}");
    }
}
