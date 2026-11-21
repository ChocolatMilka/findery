//! The five tools. Each reads its arguments strictly, resolves the project
//! when none was named, makes the daemon calls it needs, and renders the
//! answer. None decides anything the daemon should (CONV-002).

mod note;
mod projects;
mod read;
mod search;
mod sync;

use serde_json::Value;

use findery_core::{Error, Result};

use crate::Session;
use crate::args::Args;
use crate::catalogue;

/// Dispatches one `tools/call`, returning the text the model reads.
pub async fn call(session: &Session, name: &str, raw: &Value) -> Result<String> {
    let Some(declared) = catalogue::declared(name) else {
        return Err(Error::Invalid(format!(
            "unknown tool `{name}`; the tools are: {}",
            catalogue::TOOLS.join(", ")
        )));
    };
    let args = Args::new(name, raw, &declared)?;
    match name {
        "search" => search::run(session, &args).await,
        "read" => read::run(session, &args).await,
        "projects" => projects::run(session, &args).await,
        "sync" => sync::run(session, &args).await,
        "note" => note::run(session, &args).await,
        other => Err(Error::Internal(format!(
            "tool `{other}` is advertised but has no handler"
        ))),
    }
}

/// A count from a tool argument as a `usize`, saturating: the daemon caps it.
fn as_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// A count from a tool argument as a `u32`, saturating: the daemon caps it.
fn as_u32(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}
