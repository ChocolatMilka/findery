//! `findery bucket` — the independent indexes the server holds.

use findery_core::{Error, Result};

use findery_client::Client;

use crate::args::BucketAction;
use crate::ui::render;

pub(crate) async fn run(client: &Client, action: BucketAction, json: bool) -> Result<()> {
    match action {
        BucketAction::Ls => {
            let list = client.list_buckets().await?;
            if json {
                return render::json(&list);
            }
            render::buckets(&list);
        }
        BucketAction::Create { name } => {
            let info = client.create_bucket(&name).await?;
            if json {
                return render::json(&info);
            }
            println!("{} — {}", info.name, info.path);
        }
        BucketAction::Rm { name, force } => {
            // The count is what the decision needs, and reading it is not a
            // change, so the refusal happens before anything is dropped.
            let Some(info) = client
                .list_buckets()
                .await?
                .into_iter()
                .find(|c| c.name == name)
            else {
                return Err(Error::NotFound(format!("bucket `{name}`")));
            };
            if info.projects > 0 && !force {
                return Err(Error::Invalid(format!(
                    "bucket `{name}` holds {} project(s); removing it removes them and their \
                     vectors. Re-run with --force to go ahead.",
                    info.projects
                )));
            }
            let removed = client.delete_bucket(&name).await?;
            if json {
                return render::json(&removed);
            }
            println!(
                "removed `{name}` — {} project(s) went with it",
                removed.projects_removed
            );
        }
    }
    Ok(())
}
