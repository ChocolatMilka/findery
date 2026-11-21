//! `findery bucket` — the independent indexes the server holds, and the files
//! each one is made of: one per model.
//!
//! INTENT: a bucket is what a manifest names; its files are what a user sees in
//! a file manager. The listing shows both, the bucket once and each file under
//! it, so "which file is the harrier index of axle" is answered by reading, not
//! by decoding file names.

use findery_core::api::{BucketInfo, BucketMember, BucketRemoved};
use findery_core::embedding::recipe::Recipe;
use findery_core::{Error, Result};

use findery_client::Client;

use crate::args::BucketAction;
use crate::ui::render;
use crate::ui::style::{Col, Columns, Paint, count, quoted, size};

pub(crate) async fn run(client: &Client, action: BucketAction, json: bool) -> Result<()> {
    match action {
        BucketAction::Ls => ls(client, json).await,
        BucketAction::Create { name } => create(client, &name, json).await,
        BucketAction::Rm { name, force, model } => {
            rm(client, &name, force, model.as_deref(), json).await
        }
    }
}

async fn ls(client: &Client, json: bool) -> Result<()> {
    let list = client.list_buckets().await?;
    if json {
        return render::json(&list);
    }
    print!("{}", render_ls(&list, Paint::for_stdout()));
    Ok(())
}

async fn create(client: &Client, name: &str, json: bool) -> Result<()> {
    let info = client.create_bucket(name).await?;
    if json {
        return render::json(&info);
    }
    let paint = Paint::for_stdout();
    println!(
        "bucket {}  {}",
        paint.bold(&info.name),
        paint.dim(&info.path)
    );
    Ok(())
}

async fn rm(
    client: &Client,
    name: &str,
    force: bool,
    model: Option<&str>,
    json: bool,
) -> Result<()> {
    // One model's file takes no project with it; the whole bucket does, so
    // that is the case the refusal guards — before anything is dropped.
    if model.is_none() && !force {
        refuse_unless_empty(client, name).await?;
    }
    let removed = client.delete_bucket(name, model).await?;
    if json {
        return render::json(&removed);
    }
    print!(
        "{}",
        render_removed(name, model, &removed, Paint::for_stdout())
    );
    Ok(())
}

async fn refuse_unless_empty(client: &Client, name: &str) -> Result<()> {
    let list = client.list_buckets().await?;
    let Some(info) = list.iter().find(|b| b.name == name) else {
        return Err(Error::NotFound(format!("bucket `{name}`")));
    };
    if info.projects == 0 {
        return Ok(());
    }
    Err(Error::Invalid(format!(
        "bucket `{name}` holds {}; removing it removes them and their vectors — re-run with \
         --force to go ahead, or `--model <alias>` to remove one model's file only",
        count(info.projects, "project", "projects")
    )))
}

fn render_ls(list: &[BucketInfo], paint: Paint) -> String {
    if list.is_empty() {
        return "no bucket yet — `findery init` in a project creates its own\n".to_string();
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .flat_map(|b| b.members.iter().map(member_row))
        .collect();
    let fit = Columns::fit(&rows);
    let width = list.iter().map(|b| b.name.len()).max().unwrap_or(0);
    let cols = [
        Col::LEFT,
        Col::DIM,
        Col::RIGHT,
        Col::RIGHT,
        Col::RIGHT,
        Col::BOLD,
    ];

    let mut out = String::new();
    for bucket in list {
        let mark = if bucket.is_default { "*" } else { " " };
        let name = format!("{:<width$}", bucket.name);
        out.push_str(&format!(
            "{mark} {}  {}\n",
            paint.bold(&name),
            paint.dim(&bucket.path)
        ));
        if bucket.members.is_empty() {
            out.push_str(&format!("    {}\n", paint.dim(&family_summary(bucket))));
        }
        for member in &bucket.members {
            out.push_str(&format!(
                "    {}\n",
                fit.line(&member_row(member), &cols, paint)
            ));
            if let Some(conflict) = &member.conflict {
                out.push_str(&format!("      {} {conflict}\n", paint.red("!")));
            }
        }
    }
    let legend = "* default bucket · serving = the file of the default model (`findery model ls`)";
    out.push_str(&format!("\n{}\n", paint.dim(legend)));
    out
}

/// `slug-tag  recipe  projects  chunks  size  flag`.
fn member_row(m: &BucketMember) -> Vec<String> {
    vec![
        member_name(m),
        m.recipe.clone(),
        count(m.projects, "project", "projects"),
        count(m.chunks, "chunk", "chunks"),
        size(m.disk_bytes),
        member_flag(m).to_string(),
    ]
}

/// The part of the file name that tells members apart; the legacy file has
/// none, so it is named as it is on disk.
fn member_name(m: &BucketMember) -> String {
    match (m.legacy, Recipe::parse(&m.recipe)) {
        (false, Ok(recipe)) => format!("{}-{}", recipe.slug(), m.tag),
        _ => m.path.rsplit('/').next().unwrap_or(&m.path).to_string(),
    }
}

fn member_flag(m: &BucketMember) -> &'static str {
    match (m.conflict.is_some(), m.serving, m.legacy) {
        (true, _, _) => "conflict",
        (false, true, true) => "serving, legacy",
        (false, true, false) => "serving",
        (false, false, true) => "legacy",
        (false, false, false) => "",
    }
}

/// A bucket with no member to list: none created yet, or a server that
/// predates members and reports the bucket as a whole.
fn family_summary(b: &BucketInfo) -> String {
    match &b.recipe {
        Some(recipe) => format!(
            "{recipe}  {}  {}  {}",
            count(b.projects, "project", "projects"),
            count(b.chunks, "chunk", "chunks"),
            size(b.disk_bytes)
        ),
        None => "no file yet — the first sync under a model creates its file".to_string(),
    }
}

fn render_removed(name: &str, model: Option<&str>, r: &BucketRemoved, paint: Paint) -> String {
    let bucket = paint.bold(name);
    if let Some(model) = model {
        return format!(
            "removed the `{model}` file of bucket {bucket} {}\n",
            paint.dim("— its projects stay, on their other models")
        );
    }
    let went = match r.projects.as_slice() {
        [] => "it held no project".to_string(),
        ids => format!(
            "with {}: {}",
            count(ids.len() as u64, "project", "projects"),
            quoted(ids)
        ),
    };
    let files = if r.removed {
        "its files are deleted"
    } else {
        "no file was left on disk"
    };
    format!(
        "removed bucket {bucket} {went} {}\n",
        paint.dim(&format!("— {files}"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(file: &str, recipe: &str, projects: u64, chunks: u64, bytes: u64) -> BucketMember {
        let parsed = Recipe::parse(recipe).expect("recipe");
        BucketMember {
            path: format!("D:/work/axle/.findery/{file}"),
            tag: parsed.tag().to_string(),
            recipe: recipe.into(),
            legacy: file == "bucket.db",
            serving: false,
            projects,
            chunks,
            disk_bytes: bytes,
            conflict: None,
        }
    }

    fn bucket(name: &str, path: &str, members: Vec<BucketMember>) -> BucketInfo {
        BucketInfo {
            name: name.into(),
            path: path.into(),
            recipe: None,
            conflict: None,
            projects: 1,
            chunks: 0,
            disk_bytes: 0,
            is_default: name == "default",
            members,
        }
    }

    fn fixture() -> Vec<BucketInfo> {
        let qwen = "text-embedding-qwen3-embedding-0.6b:1024";
        let mut harrier = member(
            "bucket.harrier.db",
            "harrier-oss-v1-0.6b:1024",
            1,
            812,
            3_100_000,
        );
        harrier.serving = true;
        let mut conflicted = member("bucket.mock.db", "mock:1024", 1, 5, 40_960);
        conflicted.conflict = Some("holds `other:1024`; rename it".into());
        vec![
            bucket("default", "C:/data/findery/bucket.db", Vec::new()),
            bucket(
                "axle",
                "D:/work/axle/.findery/bucket.db",
                vec![
                    harrier,
                    member("bucket.db", qwen, 1, 790, 2_900_000),
                    conflicted,
                ],
            ),
        ]
    }

    #[test]
    fn every_file_of_a_bucket_is_one_aligned_line_under_it() {
        let text = render_ls(&fixture(), Paint::plain());
        let expected = "\
* default  C:/data/findery/bucket.db
    no file yet — the first sync under a model creates its file
  axle     D:/work/axle/.findery/bucket.db
    harrier-oss-v1-0-6b-zvpmtsdf  harrier-oss-v1-0.6b:1024                  1 project  812 chunks   3.1 MB  serving
    bucket.db                     text-embedding-qwen3-embedding-0.6b:1024  1 project  790 chunks   2.9 MB  legacy
";
        assert!(text.starts_with(expected), "{text}");
        assert!(
            text.contains("conflict\n      ! holds `other:1024`"),
            "{text}"
        );
        assert!(text.contains("* default bucket"), "{text}");
    }

    #[test]
    fn a_server_without_members_is_listed_bucket_by_bucket() {
        let mut old = bucket("work", "C:/data/findery/buckets/work/bucket.db", Vec::new());
        old.recipe = Some("mock:8".into());
        old.chunks = 3;
        let text = render_ls(&[old], Paint::plain());
        assert!(
            text.contains("    mock:8  1 project  3 chunks  0 B\n"),
            "{text}"
        );
    }

    #[test]
    fn removing_one_model_file_says_the_projects_stay() {
        let removed = BucketRemoved {
            removed: true,
            projects_removed: 0,
            projects: Vec::new(),
        };
        assert_eq!(
            render_removed("axle", Some("harrier"), &removed, Paint::plain()),
            "removed the `harrier` file of bucket axle — its projects stay, on their other models\n"
        );
        let whole = BucketRemoved {
            projects_removed: 2,
            projects: vec!["axle".into(), "smalt".into()],
            ..removed
        };
        assert_eq!(
            render_removed("work", None, &whole, Paint::plain()),
            "removed bucket work with 2 projects: `axle`, `smalt` — its files are deleted\n"
        );
    }
}
