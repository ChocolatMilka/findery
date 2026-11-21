//! `findery init` — claim a directory, and leave a manifest behind.
//!
//! Its own module because it is the one command that does two things: it
//! registers the project and it writes the file that makes the project readable
//! by anyone who clones it. They belong together because the second is built
//! from the first's *answer*, never from the request.

use std::path::Path;

use findery_core::Result;
use findery_core::api::RegisterProject;
use findery_core::ids::{ModelAlias, ProjectId};
use findery_core::manifest::BucketRef;
use findery_core::model::Project;

use findery_client::Client;

use crate::ui::render;
use crate::ui::style::Paint;
use crate::workspace::scope::{absolute, derive_id};

/// What the command line asked for.
pub(crate) struct Options {
    pub(crate) path: String,
    pub(crate) id: Option<String>,
    pub(crate) ext: Option<Vec<String>>,
    pub(crate) bucket: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) sync: bool,
    pub(crate) push: bool,
}

pub(crate) async fn run(client: &Client, options: Options, json: bool) -> Result<()> {
    let root = absolute(&options.path)?;
    let id = match options.id {
        Some(explicit) => ProjectId::new(explicit)?,
        None => derive_id(client, &root).await?,
    };
    // Refused here, before anything is registered or written.
    if let Some(model) = &options.model {
        ModelAlias::new(model.clone())?;
    }

    // Whatever the manifest already declares is the user's, and re-registering
    // must not be a way to lose it. The bucket is the one field `init` owns,
    // and the model pin is replaced only when `--model` names a new one.
    //
    // The daemon MERGES a registration, keeping what a request leaves out. The
    // manifest is the authority on what it declares, so each of those fields
    // is sent even when empty — an empty one is how a removal reaches the
    // daemon.
    let existing = crate::workspace::manifest::read_at(Path::new(&root))?;
    let model = options.model.or_else(|| existing.model.clone());

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
            scopes: Some(existing.scopes.clone()),
            instruction: Some(existing.instruction.clone().unwrap_or_default()),
            task: existing.task,
            model: Some(model.clone().unwrap_or_default()),
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
    let mut config =
        crate::workspace::manifest::from_registration(project_root, &project, &bucket_path);
    config.scopes = existing.scopes;
    config.instruction = existing.instruction;
    config.task = existing.task;
    config.model = model;
    let manifest = crate::workspace::manifest::write(project_root, &config)?;

    if json {
        render::json(&project)?;
    } else {
        print!(
            "{}",
            registered(
                &project,
                &manifest.display().to_string(),
                Paint::for_stdout()
            )
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
        let board = if json {
            None
        } else {
            crate::ui::board::Board::new()
        };
        crate::commands::sync::push::sync(client, &project, board.as_ref()).await?
    } else {
        crate::commands::sync::follow::sync_now(client, &project.id, json).await?
    };
    if json {
        render::json(&report)?;
    } else {
        render::sync(&report);
        println!("\nready — try: findery search \"<your question>\"");
    }
    Ok(())
}

/// What `init` registered, one fact per line.
fn registered(project: &Project, manifest: &str, paint: Paint) -> String {
    let model = match (&project.spec.model, &project.model) {
        (Some(pin), _) => format!("{pin} (pinned)"),
        (None, Some(current)) => format!("{current} (the default; `--model` pins one)"),
        (None, None) => "the server's default".to_string(),
    };
    let field =
        |label: &str, value: &str| format!("  {}  {value}\n", paint.dim(&format!("{label:<8}")));
    format!(
        "registered {}  {}\n{}{}{}",
        paint.bold(project.id.as_str()),
        paint.dim(&project.host_root),
        field("bucket", &project.bucket),
        field("model", &model),
        field("manifest", &manifest.replace('\\', "/")),
    )
}

#[cfg(test)]
mod tests {
    use findery_core::model::ProjectSpec;

    use super::*;

    fn project(pin: Option<&str>) -> Project {
        let alias = |a: &str| ModelAlias::new(a).expect("alias");
        Project {
            id: ProjectId::new("axle").expect("id"),
            name: "axle".into(),
            bucket: "axle".into(),
            model: Some(alias(pin.unwrap_or("harrier"))),
            host_root: "D:/work/axle".into(),
            spec: ProjectSpec {
                model: pin.map(alias),
                ..ProjectSpec::default()
            },
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn a_registration_says_which_bucket_and_model_it_got() {
        let text = registered(
            &project(Some("qwen3")),
            "D:\\work\\axle\\.findery\\config.toml",
            Paint::plain(),
        );
        assert_eq!(
            text,
            "registered axle  D:/work/axle\n  \
             bucket    axle\n  \
             model     qwen3 (pinned)\n  \
             manifest  D:/work/axle/.findery/config.toml\n"
        );
        let unpinned = registered(&project(None), "m", Paint::plain());
        assert!(
            unpinned.contains("harrier (the default; `--model` pins one)"),
            "{unpinned}"
        );
    }
}
