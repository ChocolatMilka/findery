//! `findery detach` and `findery rm` — taking a project off the server.
//!
//! INTENT: both verbs exist for a file the user wants to do something with, so
//! both answers name files: which ones were closed or deleted, and when one
//! stays, why. `detach` keeps the index and releases it; `rm` deletes it. The
//! server decides what can go (CONV-002); this module says what went.

use findery_core::Result;
use findery_core::api::{ProjectDetached, ProjectRemoved};
use findery_core::model::Project;

use findery_client::Client;

use crate::args::Scope;
use crate::ui::render;
use crate::ui::style::{Paint, by_directory, quoted};
use crate::workspace::scope::resolve;

pub(crate) async fn detach(client: &Client, scope: &Scope, json: bool) -> Result<()> {
    let id = resolve(client, scope).await?;
    // Read first: once detached, the server no longer knows the project's root.
    let project = client.project(&id).await?;
    let detached = client.detach_project(&id).await?;
    if json {
        return render::json(&detached);
    }
    print!(
        "{}",
        render_detached(&detached, &project, Paint::for_stdout())
    );
    Ok(())
}

pub(crate) async fn rm(client: &Client, scope: &Scope, json: bool) -> Result<()> {
    let id = resolve(client, scope).await?;
    // A detached project is no longer listed, and is still removable.
    let bucket = client.project(&id).await.ok().map(|p| p.bucket);
    let removed = client.delete_project(&id).await?;
    if json {
        return render::json(&removed);
    }
    print!(
        "{}",
        render_removed(&removed, bucket.as_deref(), Paint::for_stdout())
    );
    Ok(())
}

fn render_detached(d: &ProjectDetached, project: &Project, paint: Paint) -> String {
    let mut out = format!(
        "detached {} {}\n",
        paint.bold(&d.project),
        paint.dim("— its index stays on disk")
    );
    match &d.still_open {
        Some(why) => out.push_str(&row(
            "open",
            &format!("bucket `{}` stays open: {why}", project.bucket),
            paint,
        )),
        None => out.push_str(&files("closed", &d.closed, paint)),
    }
    out.push_str(&row(
        "back",
        &format!(
            "`findery init` in {} re-attaches it; nothing is re-indexed",
            project.host_root
        ),
        paint,
    ));
    out
}

fn render_removed(r: &ProjectRemoved, bucket: Option<&str>, paint: Paint) -> String {
    if !r.deleted {
        return format!("nothing to remove: {} is not registered\n", r.project);
    }
    let name = paint.bold(&r.project);
    if r.rows_only {
        let bucket = bucket.unwrap_or("its bucket");
        let why = match r.shared_with.as_slice() {
            [] => "the bucket every install has".to_string(),
            shared => format!("shared with {}", quoted(shared)),
        };
        return format!(
            "removed {name} {}\n{}",
            paint.dim("— its chunks and vectors"),
            row(
                "kept",
                &format!("bucket `{bucket}` and its files: {why}"),
                paint
            )
        );
    }
    if r.files_deleted.is_empty() {
        // An older server says only that it removed the project.
        return format!("removed {name}\n");
    }
    format!(
        "removed {name} {}\n{}{}",
        paint.dim("— its chunks, vectors and bucket files"),
        files("deleted", &r.files_deleted, paint),
        row(
            "kept",
            ".findery/config.toml — `findery init` registers the project again",
            paint
        )
    )
}

/// `  label    text`, the label dim and padded so the texts line up.
fn row(label: &str, text: &str, paint: Paint) -> String {
    format!("  {}  {text}\n", paint.dim(&format!("{label:<7}")))
}

/// A labelled file list naming each directory once.
fn files(label: &str, paths: &[String], paint: Paint) -> String {
    let mut out = String::new();
    let mut label = label;
    for (dir, names) in by_directory(paths) {
        out.push_str(&row(label, &paint.dim(&dir), paint));
        for name in names {
            out.push_str(&row("", &format!("  {name}"), paint));
        }
        label = "";
    }
    out
}

#[cfg(test)]
mod tests {
    use findery_core::ids::ProjectId;
    use findery_core::model::ProjectSpec;

    use super::*;

    fn axle() -> Project {
        Project {
            id: ProjectId::new("axle").expect("id"),
            name: "axle".into(),
            bucket: "axle".into(),
            model: None,
            host_root: "D:/work/axle".into(),
            spec: ProjectSpec::default(),
            created_at: 0,
            updated_at: 0,
        }
    }

    const DIR: &str = "D:/work/axle/.findery/";

    fn members() -> Vec<String> {
        [
            "bucket.harrier-oss-v1-0-6b-zvpmtsdf.db",
            "bucket.text-embedding-qwen3-embedding-0-6b-47m32eui.db",
        ]
        .map(|f| format!("{DIR}{f}"))
        .to_vec()
    }

    #[test]
    fn a_detach_names_the_directory_once_and_how_to_come_back() {
        let detached = ProjectDetached {
            project: "axle".into(),
            closed: members(),
            still_open: None,
        };
        assert_eq!(
            render_detached(&detached, &axle(), Paint::plain()),
            "detached axle — its index stays on disk\n  \
             closed   D:/work/axle/.findery/\n             \
             bucket.harrier-oss-v1-0-6b-zvpmtsdf.db\n             \
             bucket.text-embedding-qwen3-embedding-0-6b-47m32eui.db\n  \
             back     `findery init` in D:/work/axle re-attaches it; nothing is re-indexed\n"
        );
    }

    #[test]
    fn a_detach_that_leaves_the_bucket_open_says_why() {
        let detached = ProjectDetached {
            project: "axle".into(),
            closed: Vec::new(),
            still_open: Some("shared with `smalt`".into()),
        };
        let text = render_detached(&detached, &axle(), Paint::plain());
        assert!(
            text.contains("open     bucket `axle` stays open: shared with `smalt`"),
            "{text}"
        );
        assert!(!text.contains("closed"), "{text}");
    }

    #[test]
    fn a_removal_lists_the_files_it_deleted_and_the_manifest_it_kept() {
        let removed = ProjectRemoved {
            project: "axle".into(),
            deleted: true,
            files_deleted: members()[..1].to_vec(),
            rows_only: false,
            shared_with: Vec::new(),
        };
        assert_eq!(
            render_removed(&removed, Some("axle"), Paint::plain()),
            "removed axle — its chunks, vectors and bucket files\n  \
             deleted  D:/work/axle/.findery/\n             \
             bucket.harrier-oss-v1-0-6b-zvpmtsdf.db\n  \
             kept     .findery/config.toml — `findery init` registers the project again\n"
        );
    }

    #[test]
    fn a_shared_bucket_keeps_its_files_and_names_who_shares_it() {
        let removed = ProjectRemoved {
            project: "axle".into(),
            deleted: true,
            files_deleted: Vec::new(),
            rows_only: true,
            shared_with: vec!["docs".into(), "smalt".into()],
        };
        assert_eq!(
            render_removed(&removed, Some("work"), Paint::plain()),
            "removed axle — its chunks and vectors\n  \
             kept     bucket `work` and its files: shared with `docs`, `smalt`\n"
        );
        let default = ProjectRemoved {
            shared_with: Vec::new(),
            ..removed
        };
        let text = render_removed(&default, Some("default"), Paint::plain());
        assert!(text.contains("the bucket every install has"), "{text}");
    }

    #[test]
    fn an_older_server_or_an_absent_project_still_reads_plainly() {
        let old = ProjectRemoved {
            project: "axle".into(),
            deleted: true,
            files_deleted: Vec::new(),
            rows_only: false,
            shared_with: Vec::new(),
        };
        assert_eq!(render_removed(&old, None, Paint::plain()), "removed axle\n");
        let absent = ProjectRemoved {
            deleted: false,
            ..old
        };
        assert!(render_removed(&absent, None, Paint::plain()).starts_with("nothing to remove"));
    }
}
