//! The single error type of the workspace. Every layer maps ITS failures into
//! this enum; nobody panics, nobody calls `unwrap()`.

/// The failure families of findery.
///
/// INTENT: one variant means one distinct caller decision. `NotFound` becomes a
/// 404, `Invalid` a 400, `Upstream` a 502, everything else a 500.
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

    /// The catalog or the vector store refused the operation.
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

    /// The HTTP status the server layer MUST use for this error.
    ///
    /// INTENT: the mapping lives here (single source of truth) instead of in
    /// every handler, otherwise two routes render two codes for one failure.
    pub fn http_status(&self) -> u16 {
        match self {
            Self::Invalid(_) | Self::Config(_) => 400,
            Self::NotFound(_) => 404,
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
            Self::Store(_) => "store",
            Self::Embed(_) => "embed",
            Self::Upstream { .. } => "upstream",
            Self::Io(_) => "io",
            Self::Internal(_) => "internal",
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Internal(format!("json: {e}"))
    }
}

/// Workspace-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;
