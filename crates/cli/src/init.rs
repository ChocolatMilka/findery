//! `findery init` — claim a directory, and leave a manifest behind.
//!
//! Split from `run.rs` because it is the one command that does two things: it
//! registers the project and it writes the file that makes the project readable
//! by anyone who clones it. They belong together because the second is built
//! from the first's *answer*, never from the request.

use crate::render;
use crate::scope::{absolute, derive_id};
use findery_client::Client;
use findery_core::Result;
use findery_core::api::RegisterProject;
use findery_core::ids::ProjectId;
use findery_core::project::BucketRef;
use std::path::Path;

/// What the command line asked for.
pub(crate) struct Options {
    pub(crate) path: String,
    pub(crate) id: Option<String>,
    pub(crate) ext: Option<Vec<String>>,
    pub(crate) bucket: Option<String>,
    pub(crate) sync: bool,
    pub(crate) push: bool,
}

pub(crate) async fn run(client: &Client, options: Options, json: bool) -> Result<()> {
    let root = absolute(&options.path)?;
    let id = match options.id {
        Some(explicit) => ProjectId::new(explicit)?,
        None => derive_id(client, &root).await?,
    };

    // Whatever the manifest already declares is the user's, and re-registering
    // must not be a way to lose it. The bucket is the one field `init` owns: the
    // others were written by hand.
    let existing = crate::manifest::read_at(Path::new(&root))?;

    // No `--bucket` means the ordinary case, which the daemon resolves from the
    // project root: a bucket inside the project, named after it. The manifest
    // records whichever bucket the daemon actually used, substitution included.
    let project = client
        .register(&RegisterProject {
            root: root.clone(),
            id: Some(id.to_string()),
            extensions: options.ext,
            bucket: options
                .bucket
                .map(|name| BucketRef {
                    name: Some(name),
                    path: None,
                })
                .or_else(|| (!existing.bucket.is_unspecified()).then(|| existing.bucket.clone())),
            scopes: (!existing.scopes.is_empty()).then(|| existing.scopes.clone()),
            instruction: existing.instruction.clone(),
            task: existing.task,
            push: options.push.then_some(true),
            ..Default::default()
        })
        .await?;

    // The bucket's location is the bucket's own fact, so it is asked of the
    // resource that owns it rather than assumed from the name it answered with.
    let bucket_path = client
        .list_buckets()
        .await?
        .into_iter()
        .find(|b| b.name == project.bucket)
        .map(|b| b.path)
        .unwrap_or_default();
    let project_root = Path::new(&project.host_root);
    let mut config = crate::manifest::from_registration(project_root, &project, &bucket_path);
    config.scopes = existing.scopes;
    config.instruction = existing.instruction;
    config.task = existing.task;
    let manifest = crate::manifest::write(project_root, &config)?;

    if json {
        render::json(&project)?;
    } else {
        println!(
            "registered `{}` at {} (bucket {}, recorded in {})",
            project.id,
            project.host_root,
            project.bucket,
            manifest.display()
        );
    }

    // Registration and indexing are two steps on purpose: a bucket can be
    // chosen and a repository claimed before any model is reachable.
    if !options.sync {
        if !json {
            println!("run `findery sync` when you are ready to index it");
        }
        return Ok(());
    }
    let report = if project.spec.push {
        crate::push::sync(client, &project).await?
    } else {
        crate::progress::sync_now(client, &project.id, json).await?
    };
    if json {
        render::json(&report)?;
    } else {
        render::sync(&report);
        println!("\nready — try: findery search \"<your question>\"");
    }
    Ok(())
}
