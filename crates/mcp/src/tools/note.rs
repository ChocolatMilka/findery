//! `note`: a document written from text, or removed.

use findery_core::api::AddDocument;
use findery_core::{Error, Result};

use crate::Session;
use crate::args::Args;
use crate::catalogue::NOTE_ACTIONS;

#[derive(Clone, Copy)]
enum Action {
    Add,
    Remove,
}

impl Action {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "add" => Some(Self::Add),
            "remove" => Some(Self::Remove),
            _ => None,
        }
    }
}

pub(super) async fn run(session: &Session, args: &Args<'_>) -> Result<String> {
    let action = args
        .choice("action", Action::parse, NOTE_ACTIONS)?
        .unwrap_or(Action::Add);
    let path = args.required("path")?;
    match action {
        Action::Add => add(session, args, path).await,
        Action::Remove => remove(session, args, path).await,
    }
}

async fn add(session: &Session, args: &Args<'_>, path: String) -> Result<String> {
    let Some(text) = args.text("text")? else {
        return Err(Error::Invalid(
            "`note` add needs `text`; to index a file that exists on disk, use sync {paths}".into(),
        ));
    };
    let project = session.project(args).await?;
    let request = AddDocument {
        path,
        text: Some(text),
    };
    let report = session.client().add_document(&project, &request).await?;
    Ok(format!(
        "noted `{}` in `{project}`: {} chunk(s), {} embedded, {} from cache. A sync never \
         prunes it; note {{action: \"remove\"}} does.",
        request.path, report.chunks_written, report.embeddings_computed, report.embeddings_cached
    ))
}

async fn remove(session: &Session, args: &Args<'_>, path: String) -> Result<String> {
    if args.text("text")?.is_some() {
        return Err(Error::Invalid("`note` remove takes no `text`".into()));
    }
    let project = session.project(args).await?;
    let removed = session.client().remove_document(&project, &path).await?;
    Ok(match removed {
        0 => format!("nothing indexed at `{path}` in `{project}`; nothing removed"),
        n => format!("removed `{path}` from `{project}`: {n} chunk(s)"),
    })
}
