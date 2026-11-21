//! `sync`: start or follow a sync, and wait for it within a bound.

use std::time::Duration;

use findery_core::Result;
use findery_core::api::{SyncRequest, SyncResponse};

use crate::Session;
use crate::args::Args;
use crate::catalogue::SYNC_MODES;
use crate::render;

/// Seconds a call waits when it does not say: long enough for an incremental
/// sync, short enough for any MCP client's timeout.
const DEFAULT_WAIT_SECS: u64 = 20;
/// The most a call may ask for; the daemon caps it again
/// (`server.max_sync_wait_ms`).
const MAX_WAIT_SECS: u64 = 120;

pub(super) async fn run(session: &Session, args: &Args<'_>) -> Result<String> {
    let wait = args
        .count("wait")?
        .unwrap_or(DEFAULT_WAIT_SECS)
        .min(MAX_WAIT_SECS);
    if let Some(job) = args.text("job")? {
        let within = Duration::from_secs(wait);
        let job = session.client().await_job_within(&job, within).await?;
        return Ok(render::jobs::job(&job, wait));
    }
    let request = SyncRequest {
        mode: args.choice("mode", known_mode, SYNC_MODES)?,
        paths: Some(args.texts("paths")?).filter(|p| !p.is_empty()),
        wait: false,
        wait_ms: Some(wait * 1_000),
    };
    request.resolve()?;
    let project = session.project(args).await?;
    Ok(match session.client().sync(&project, &request).await? {
        SyncResponse::Completed { report } => render::jobs::report(&report),
        SyncResponse::Accepted { job } => render::jobs::job(&job, wait),
    })
}

fn known_mode(raw: &str) -> Option<String> {
    SYNC_MODES.contains(&raw).then(|| raw.to_string())
}
