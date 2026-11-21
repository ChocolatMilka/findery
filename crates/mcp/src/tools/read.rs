//! `read`: the lines behind a locator, or the indexed files behind a glob.

use findery_core::api::ReadQuery;
use findery_core::{Error, Result};

use super::as_u32;
use crate::Session;
use crate::args::Args;
use crate::render;

pub(super) async fn run(session: &Session, args: &Args<'_>) -> Result<String> {
    let glob = args.text("glob")?;
    let query = ReadQuery {
        locator: args.text("locator")?,
        id: args.text("id")?,
        context: args.count("context")?.map(as_u32),
    };
    let names_lines = query.locator.is_some() || query.id.is_some();
    match glob {
        Some(_) if names_lines => Err(Error::Invalid(
            "`glob` lists files; send it alone, or read one `locator` or `id`".into(),
        )),
        Some(glob) => {
            let project = session.project(args).await?;
            let files = session.client().files(&project, &glob).await?;
            Ok(render::read::files(&project, &glob, &files))
        }
        None => {
            // The target is checked before anything is asked of the daemon.
            query.target()?;
            let project = session.project(args).await?;
            let read = session.client().read(&project, &query).await?;
            Ok(render::read::lines(&read))
        }
    }
}
