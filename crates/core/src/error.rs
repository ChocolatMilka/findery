//! The single error type of the workspace. Every layer maps ITS failures into
//! this enum; nobody panics, nobody calls `unwrap()`.
//!
//! INTENT: an error crosses the wire as `{ code, message }` and is rebuilt on
//! the other side by [`Error::from_wire`], so a variant is only as useful as its
//! round trip. Every variant therefore carries its payload as text, and every
//! user-actionable one has a constructor that writes the message once — naming
//! the command that repairs it — instead of letting each call site improvise.

use std::time::Duration;

use crate::ids::ProjectId;
use crate::model::DEFAULT_BUCKET;

/// The failure families of findery.
///
/// INTENT: one variant means one distinct caller decision. `NotFound` becomes a
/// 404, `Invalid` and `ModelUnknown` a 400, the conflicts a user can resolve
/// (`RecipeMismatch`, `VectorsUnavailable`, `Busy`, `Cancelled`) a 409,
/// `Upstream` a 502, everything else a 500.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Configuration unreadable, inconsistent or incomplete.
    #[error("config: {0}")]
    Config(String),

    /// Caller input rejected before any side effect happened.
    #[error("invalid: {0}")]
    Invalid(String),

    /// The requested resource (project, document, job) does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// Stored vectors belong to another embedding space than the one this
    /// daemon serves. The caller re-indexes, or restores the settings that
    /// built them — retrying changes nothing.
    #[error("recipe mismatch: {0}")]
    RecipeMismatch(String),

    /// The project exists but has no vectors under the recipe being asked for
    /// yet. The caller syncs it, or searches by keyword meanwhile.
    #[error("vectors unavailable: {0}")]
    VectorsUnavailable(String),

    /// A model name that no profile defines. The caller fixes the name or adds
    /// the profile.
    #[error("unknown model: {0}")]
    ModelUnknown(String),

    /// Something else holds what this operation needs (a running job, a file
    /// another process has open). Nothing was changed; the caller retries later.
    #[error("busy: {0}")]
    Busy(String),

    /// The operation was stopped on purpose before it finished.
    #[error("cancelled: {0}")]
    Cancelled(String),

    /// The bucket or the vector store refused the operation.
    #[error("store: {0}")]
    Store(String),

    /// The embedding service failed or answered off-contract.
    #[error("embed: {0}")]
    Embed(String),

    /// A third-party service (TEI, Qdrant, llama.cpp) is unreachable or broken.
    #[error("upstream {service}: {detail}")]
    Upstream { service: String, detail: String },

    /// Disk I/O failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Internal invariant violated — a bug, not hostile input.
    #[error("internal: {0}")]
    Internal(String),
}

impl Error {
    /// Builds an upstream error without formatting at every call site.
    pub fn upstream(service: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Upstream {
            service: service.into(),
            detail: detail.into(),
        }
    }

    /// A bucket whose vectors were built with `holds` while the daemon serves
    /// `serves`.
    ///
    /// `at_derived_path` says whether the bucket's file is where its name puts
    /// it (`<data_dir>/buckets/<name>/bucket.db`). The repair depends on it:
    /// `findery init --bucket <name>` re-creates a bucket at that derived path,
    /// so recommending it for a bucket inside a project would silently move the
    /// project out of its own directory. Such a bucket is re-created by a plain
    /// `findery init` from the project root, whose manifest remembers where the
    /// bucket was. The default bucket cannot be removed at all, so its projects
    /// move out instead.
    pub fn recipe_mismatch(bucket: &str, holds: &str, serves: &str, at_derived_path: bool) -> Self {
        let rebuild = rebuild_hint(bucket, at_derived_path);
        Self::RecipeMismatch(format!(
            "bucket `{bucket}` holds vectors built with `{holds}`, but this daemon serves \
             `{serves}`. To re-index it under `{serves}`: {rebuild}. To keep it as it is: restart \
             the daemon with the embedder settings that built `{holds}`"
        ))
    }

    /// A member file of `bucket` whose name says `serves` while its content
    /// says `holds` — renamed by hand, or the one-in-10^10 tag collision.
    ///
    /// INTENT: the file name is how the daemon finds a model's vectors, so a
    /// file that lies about them is refused at the operation, never trusted.
    /// The repair is a rename back to the name its content earns
    /// (`rename_to`), or a rebuild, which the same three-way rule as
    /// [`Error::recipe_mismatch`] spells.
    pub fn misnamed_member(
        bucket: &str,
        file: &str,
        holds: &str,
        serves: &str,
        rename_to: &str,
        at_derived_path: bool,
    ) -> Self {
        let rebuild = rebuild_hint(bucket, at_derived_path);
        Self::RecipeMismatch(format!(
            "bucket `{bucket}` holds vectors built with `{holds}` in {file}, a file named for \
             `{serves}`. To keep them: rename the file to {rename_to}. To re-index it under \
             `{serves}`: {rebuild}"
        ))
    }

    /// A project whose stored vectors are `stored` wide while the embedder
    /// produces `served`.
    ///
    /// INTENT: this is the conflict the bucket's recipe failed to record — a
    /// bucket written before recipes were stamped. `findery sync --full` cannot
    /// repair it: gate 3 keeps every vector already there, so the old width
    /// would meet the new one again. The vectors have to go with the project.
    pub fn width_mismatch(project: &ProjectId, stored: usize, served: usize) -> Self {
        Self::RecipeMismatch(format!(
            "project `{project}` holds {stored}-dim vectors but the embedder produces {served}-dim \
             ones. To re-index it: `findery rm -p {project}`, then `findery init --sync` from the \
             project root. To keep it as it is: restart the daemon with the embedder settings that \
             produced {stored}-dim vectors"
        ))
    }

    /// A project with no vectors under the model profile `model` yet.
    pub fn vectors_unavailable(project: &ProjectId, model: &str) -> Self {
        Self::VectorsUnavailable(format!(
            "project `{project}` has no vectors under `{model}` yet; run `findery sync -p \
             {project}` — keyword search (`--mode keyword`) works meanwhile"
        ))
    }

    /// A model name no profile defines, with the ones that are and where a new
    /// one goes.
    pub fn model_unknown(name: &str, known: &[&str], config_path: &str) -> Self {
        let known = if known.is_empty() {
            "none".to_string()
        } else {
            known.join(", ")
        };
        Self::ModelUnknown(format!(
            "`{name}` is not a defined model (known: {known}); add `[models.{name}]` to \
             `{config_path}`, or pick one of the known names"
        ))
    }

    /// A running job that did not let go within `waited`.
    pub fn busy_job(job: &str, project: &ProjectId, waited: Duration) -> Self {
        Self::Busy(format!(
            "job `{job}` on `{project}` did not stop within {} s; nothing was changed — retry once \
             `findery jobs {job}` shows it finished",
            waited.as_secs()
        ))
    }

    /// A project something holds that is not a job — a wait-mode sync, a
    /// search — and that did not let go within `waited`.
    pub fn busy_project(project: &ProjectId, waited: Duration) -> Self {
        Self::Busy(format!(
            "`{project}` is still in use (a sync started with `wait`, or a search) after {} s; \
             nothing was changed — retry once it finished",
            waited.as_secs()
        ))
    }

    /// A file another process holds open, and the command to run again once it
    /// is released.
    pub fn busy_file(path: &str, cause: &std::io::Error, retry: &str) -> Self {
        Self::Busy(format!(
            "`{path}` is held by another process ({cause}); nothing was changed — close whatever \
             has it open, then run `{retry}` again"
        ))
    }

    /// A file another process holds open while a removal is under way. Unlike
    /// [`Error::busy_file`], something already changed: the removal stopped
    /// part-way, its tombstone stays, and it resumes on the retry or at the
    /// next start.
    pub fn busy_removal(path: &str, cause: &std::io::Error, retry: &str) -> Self {
        Self::Busy(format!(
            "`{path}` is held by another process ({cause}); the removal stopped part-way and \
             resumes on its own at the next start — close whatever has it open, then run \
             `{retry}` again"
        ))
    }

    /// A model whose runtime now serves another vector space than the one its
    /// buckets remember: writing or comparing its vectors would mix two.
    pub fn recipe_drift(model: &str, served: &str, remembered: &str) -> Self {
        Self::RecipeMismatch(format!(
            "model `{model}` now serves `{served}`, not `{remembered}` as its buckets remember; \
             retry the operation"
        ))
    }

    /// A sync of `project` stopped on purpose by `by` (the command that asked).
    pub fn cancelled(project: &ProjectId, by: &str) -> Self {
        Self::Cancelled(format!("sync of `{project}` stopped by `{by}`"))
    }

    /// The payload of the error, without the prefix its `Display` adds.
    ///
    /// INTENT: this is what travels on the wire, beside the machine-readable
    /// `code`. A server that sent `Display` instead would send "not found: x"
    /// under the code `not_found`, and the client rebuilding the variant from
    /// `code` would render "not found: not found: x". An upstream error keeps its
    /// service in the payload: dropping it would make a client say the daemon
    /// failed when it was the embedder.
    pub fn detail(&self) -> String {
        match self {
            Self::Config(m)
            | Self::Invalid(m)
            | Self::NotFound(m)
            | Self::RecipeMismatch(m)
            | Self::VectorsUnavailable(m)
            | Self::ModelUnknown(m)
            | Self::Busy(m)
            | Self::Cancelled(m)
            | Self::Store(m)
            | Self::Embed(m)
            | Self::Internal(m) => m.clone(),
            Self::Upstream { service, detail } => format!("{service}: {detail}"),
            Self::Io(e) => e.to_string(),
        }
    }

    /// The HTTP status the server layer MUST use for this error.
    ///
    /// INTENT: the mapping lives here (single source of truth) instead of in
    /// every handler, otherwise two routes render two codes for one failure.
    pub fn http_status(&self) -> u16 {
        match self {
            Self::Invalid(_) | Self::Config(_) | Self::ModelUnknown(_) => 400,
            Self::NotFound(_) => 404,
            Self::RecipeMismatch(_)
            | Self::VectorsUnavailable(_)
            | Self::Busy(_)
            | Self::Cancelled(_) => 409,
            Self::Upstream { .. } | Self::Embed(_) => 502,
            Self::Store(_) | Self::Io(_) | Self::Internal(_) => 500,
        }
    }

    /// Stable machine-readable tag for clients (never for human logs).
    pub fn code(&self) -> &'static str {
        match self {
            Self::Config(_) => "config",
            Self::Invalid(_) => "invalid",
            Self::NotFound(_) => "not_found",
            Self::RecipeMismatch(_) => "recipe_mismatch",
            Self::VectorsUnavailable(_) => "vectors_unavailable",
            Self::ModelUnknown(_) => "model_unknown",
            Self::Busy(_) => "busy",
            Self::Cancelled(_) => "cancelled",
            Self::Store(_) => "store",
            Self::Embed(_) => "embed",
            Self::Upstream { .. } => "upstream",
            Self::Io(_) => "io",
            Self::Internal(_) => "internal",
        }
    }

    /// Rebuilds the variant [`Error::code`] named from the [`Error::detail`]
    /// that travelled with it — the inverse of the pair, for every variant.
    ///
    /// INTENT: one inverse, in L0, beside the mapping it inverts. A client, a
    /// job record and an MCP bridge that each decoded codes by hand would each
    /// forget the next variant differently. A code this build does not know (a
    /// newer daemon) becomes `Internal` with the message intact: unreadable as a
    /// decision, still readable by a human.
    pub fn from_wire(code: &str, message: String) -> Self {
        match code {
            "config" => Self::Config(message),
            "invalid" => Self::Invalid(message),
            "not_found" => Self::NotFound(message),
            "recipe_mismatch" => Self::RecipeMismatch(message),
            "vectors_unavailable" => Self::VectorsUnavailable(message),
            "model_unknown" => Self::ModelUnknown(message),
            "busy" => Self::Busy(message),
            "cancelled" => Self::Cancelled(message),
            "store" => Self::Store(message),
            "embed" => Self::Embed(message),
            "upstream" => match message.split_once(": ") {
                Some((service, detail)) => Self::upstream(service, detail),
                // An older daemon sent the detail alone; the daemon is the one
                // service the client knows it was talking to.
                None => Self::upstream("findery", message),
            },
            "io" => Self::Io(std::io::Error::other(message)),
            _ => Self::Internal(message),
        }
    }
}

/// How to rebuild a bucket under the served recipe, which depends on where its
/// file is (see [`Error::recipe_mismatch`]).
fn rebuild_hint(bucket: &str, at_derived_path: bool) -> String {
    if bucket == DEFAULT_BUCKET {
        format!(
            "move each of its projects to a fresh bucket with `findery init --bucket <new-name> \
             --sync` from the project root (`{DEFAULT_BUCKET}` itself cannot be removed)"
        )
    } else if at_derived_path {
        format!(
            "`findery bucket rm {bucket} --force`, then `findery init --bucket {bucket} --sync` \
             from the root of each project it held"
        )
    } else {
        format!(
            "`findery bucket rm {bucket} --force`, then `findery init --sync` from the root of \
             each project it held (the project manifest puts the bucket back where it was)"
        )
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Internal(format!("json: {e}"))
    }
}

/// Workspace-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> ProjectId {
        ProjectId::new("axle").expect("a valid id")
    }

    /// One of each variant. A variant added to the enum and not here fails the
    /// exhaustive match in `every_variant_is_listed`, which is the point.
    fn one_of_each() -> Vec<Error> {
        vec![
            Error::Config("bad toml".into()),
            Error::Invalid("bad glob".into()),
            Error::NotFound("project `x`".into()),
            Error::recipe_mismatch("work", "a:128", "b:128", true),
            Error::width_mismatch(&project(), 1024, 2560),
            Error::vectors_unavailable(&project(), "harrier:1024"),
            Error::model_unknown("harier", &["harrier"], "/etc/findery/config.toml"),
            Error::busy_job("3f2a", &project(), Duration::from_secs(15)),
            Error::busy_file(
                "/data/bucket.db",
                &std::io::Error::other("os error 32"),
                "findery bucket rm work",
            ),
            Error::busy_removal(
                "/data/bucket.db",
                &std::io::Error::other("os error 32"),
                "findery rm -p axle",
            ),
            Error::recipe_drift("harrier", "harrier:2560", "harrier:1024"),
            Error::cancelled(&project(), "findery detach"),
            Error::busy_project(&project(), Duration::from_secs(15)),
            Error::misnamed_member(
                "work",
                "/d/bucket.mock-abcdefgh.db",
                "other:8",
                "mock:8",
                "/d/bucket.other-ijklmnop.db",
                true,
            ),
            Error::Store("locked".into()),
            Error::Embed("width".into()),
            Error::upstream("embedder", "http://x unreachable: refused"),
            Error::Io(std::io::Error::other("disk full")),
            Error::Internal("boom".into()),
        ]
    }

    #[test]
    fn every_variant_is_listed() {
        let mut seen = std::collections::BTreeSet::new();
        for e in one_of_each() {
            // Exhaustive on purpose: a new variant does not compile until it is
            // added to `one_of_each`, and so to the round trip below.
            match &e {
                Error::Config(_)
                | Error::Invalid(_)
                | Error::NotFound(_)
                | Error::RecipeMismatch(_)
                | Error::VectorsUnavailable(_)
                | Error::ModelUnknown(_)
                | Error::Busy(_)
                | Error::Cancelled(_)
                | Error::Store(_)
                | Error::Embed(_)
                | Error::Upstream { .. }
                | Error::Io(_)
                | Error::Internal(_) => {}
            }
            seen.insert(e.code());
        }
        assert_eq!(seen.len(), 13, "one code per variant: {seen:?}");
    }

    #[test]
    fn every_variant_survives_the_wire_round_trip() {
        for original in one_of_each() {
            let rebuilt = Error::from_wire(original.code(), original.detail());
            assert_eq!(rebuilt.code(), original.code(), "{original}");
            assert_eq!(rebuilt.detail(), original.detail(), "{original}");
            assert_eq!(rebuilt.to_string(), original.to_string(), "{original}");
            assert_eq!(rebuilt.http_status(), original.http_status(), "{original}");
        }
    }

    #[test]
    fn an_unknown_code_becomes_internal_with_its_message_intact() {
        let e = Error::from_wire("from_a_newer_daemon", "something new".into());
        assert_eq!(e.code(), "internal");
        assert_eq!(e.detail(), "something new");
    }

    /// An older daemon sent an upstream error's detail without its service.
    #[test]
    fn an_upstream_detail_without_a_service_is_blamed_on_the_daemon() {
        let e = Error::from_wire("upstream", "down".into());
        assert_eq!(e.to_string(), "upstream findery: down");
    }

    #[test]
    fn user_resolvable_conflicts_are_409_and_an_unknown_model_is_400() {
        assert_eq!(
            Error::recipe_mismatch("w", "a:1", "b:1", true).http_status(),
            409
        );
        assert_eq!(
            Error::vectors_unavailable(&project(), "a:1").http_status(),
            409
        );
        assert_eq!(Error::Busy("x".into()).http_status(), 409);
        assert_eq!(Error::Cancelled("x".into()).http_status(), 409);
        assert_eq!(Error::model_unknown("x", &[], "c").http_status(), 400);
        assert_eq!(Error::Internal("x".into()).http_status(), 500);
    }

    #[test]
    fn a_named_bucket_is_rebuilt_under_its_name() {
        let text = Error::recipe_mismatch("work", "a:128", "b:128", true).to_string();
        assert!(text.starts_with("recipe mismatch: bucket `work`"), "{text}");
        assert!(
            text.contains("`a:128`") && text.contains("`b:128`"),
            "{text}"
        );
        assert!(text.contains("findery bucket rm work --force"), "{text}");
        assert!(text.contains("findery init --bucket work --sync"), "{text}");
    }

    /// `init --bucket <name>` derives the path from the name: for a bucket inside
    /// a project, that is a silent move out of the project.
    #[test]
    fn a_project_local_bucket_is_never_rebuilt_with_init_bucket() {
        let text = Error::recipe_mismatch("axle", "a:128", "b:128", false).to_string();
        assert!(text.contains("findery bucket rm axle --force"), "{text}");
        assert!(text.contains("`findery init --sync`"), "{text}");
        assert!(!text.contains("--bucket axle"), "{text}");
    }

    /// `bucket rm default` is refused, so recommending it is a dead end.
    #[test]
    fn the_default_bucket_is_never_told_to_remove_itself() {
        let text = Error::recipe_mismatch(DEFAULT_BUCKET, "a:1", "b:1", true).to_string();
        assert!(!text.contains("bucket rm"), "{text}");
        assert!(text.contains("findery init --bucket <new-name>"), "{text}");
    }

    /// A misnamed member names the file, both recipes and both repairs.
    #[test]
    fn a_misnamed_member_names_the_rename_and_the_rebuild() {
        let text = Error::misnamed_member(
            "axle",
            "/p/bucket.mock-aaaaaaaa.db",
            "other:8",
            "mock:8",
            "/p/bucket.other-bbbbbbbb.db",
            false,
        )
        .to_string();
        assert!(
            text.starts_with("recipe mismatch: bucket `axle` holds vectors built with `other:8`"),
            "{text}"
        );
        assert!(
            text.contains("rename the file to /p/bucket.other-bbbbbbbb.db"),
            "{text}"
        );
        assert!(text.contains("findery bucket rm axle --force"), "{text}");
        assert!(!text.contains("--bucket axle"), "{text}");
    }

    /// A removal that stopped part-way changed something; saying otherwise
    /// would tell the user their index is intact when it is half gone.
    #[test]
    fn a_busy_removal_never_claims_nothing_changed() {
        let held = std::io::Error::other("os error 32");
        let text = Error::busy_removal("/d/x.db", &held, "findery rm -p axle").to_string();
        assert!(!text.contains("nothing was changed"), "{text}");
        assert!(text.contains("findery rm -p axle"), "{text}");
        assert!(
            Error::busy_file("/d/x.db", &held, "retry")
                .to_string()
                .contains("nothing was changed")
        );
    }

    /// `--full` keeps the vectors gate 3 finds, so it cannot be the advice.
    #[test]
    fn a_width_mismatch_never_recommends_a_full_sync() {
        let text = Error::width_mismatch(&project(), 1024, 2560).to_string();
        assert!(!text.contains("--full"), "{text}");
        assert!(text.contains("findery rm -p axle"), "{text}");
    }

    #[test]
    fn an_unknown_model_lists_the_known_ones_and_where_to_add_it() {
        let text = Error::model_unknown("harier", &["harrier", "qwen"], "/c.toml").to_string();
        assert!(text.contains("known: harrier, qwen"), "{text}");
        assert!(text.contains("[models.harier]"), "{text}");
        assert!(text.contains("/c.toml"), "{text}");
    }
}
