//! Terminal rendering. Kept apart from dispatch so `main.rs` stays a table of
//! commands rather than a table of commands interleaved with `println!`.

use findery_core::Result;
use findery_core::api::{ExportReport, HealthReport, Job};
use findery_core::model::{IndexStats, Project, SearchHit, SyncReport};

/// Prints any serializable value as indented JSON.
pub fn json<T: serde::Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub fn health(h: &HealthReport) {
    println!(
        "{} · model {} ({} dims) · store {} · {} project(s){}",
        if h.ok { "ready" } else { "DEGRADED" },
        h.model,
        h.dim,
        h.store_backend,
        h.projects,
        h.detail
            .as_deref()
            .map(|d| format!("\n  {d}"))
            .unwrap_or_default()
    );
}

/// Lists projects, marking the one that contains the current directory.
pub fn projects(list: &[Project], current: Option<&str>) {
    if list.is_empty() {
        println!("no project indexed yet — run `findery init` inside one");
        return;
    }
    for p in list {
        let here = if current == Some(p.id.as_str()) {
            "*"
        } else {
            " "
        };
        println!("{here} {:<22} {}", p.id.to_string(), p.host_root);
    }
    if current.is_none() {
        println!("\n(no project contains the current directory)");
    }
}

pub fn stats(s: &IndexStats) {
    println!(
        "{}: {} files · {} chunks · {} vectors · {} ({} dims) · {} · {:.1} MB",
        s.project,
        s.files,
        s.chunks,
        s.vectors,
        s.model,
        s.dim,
        s.store_backend,
        s.disk_bytes as f64 / 1e6
    );
}

pub fn sync(report: &SyncReport) {
    println!(
        "{} in {} ms — scanned {} · unchanged {} · +{} ~{} -{} · chunks +{} -{} · \
         embeddings {} computed / {} cached",
        report.mode,
        report.duration_ms,
        report.files_scanned,
        report.files_unchanged,
        report.files_added,
        report.files_updated,
        report.files_removed,
        report.chunks_written,
        report.chunks_deleted,
        report.embeddings_computed,
        report.embeddings_cached
    );
    for warning in report.warnings.iter().take(10) {
        eprintln!("  warning: {warning}");
    }
    let hidden = report.warnings.len().saturating_sub(10);
    if hidden > 0 {
        eprintln!("  … and {hidden} more warning(s)");
    }
}

pub fn hits(hits: &[SearchHit]) {
    if hits.is_empty() {
        println!("no match");
        return;
    }
    for (i, hit) in hits.iter().enumerate() {
        println!(
            "\n[{}] {:.3}  {}  ({} {})",
            i + 1,
            hit.score,
            hit.chunk.locator(),
            hit.chunk.kind.as_str(),
            hit.chunk.breadcrumb.as_deref().unwrap_or("-")
        );
        println!("{}", hit.host_path);
        for line in hit.chunk.text.lines() {
            println!("    {line}");
        }
    }
}

pub fn export(report: &ExportReport) {
    println!(
        "{} vectors ({} dims) → {}\n  {}",
        report.vectors,
        report.dim,
        report.directory,
        report.files.join(", ")
    );
}

pub fn jobs(list: &[Job]) {
    for job in list {
        println!("{:<38} {:<16} {:?}", job.id, job.project, job.state);
    }
}

/// Turns a transport failure into something the reader can act on.
///
/// INTENT: "connection refused" tells a user nothing. The daemon starts itself,
/// so reaching this message means something stopped it from coming up — and
/// naming the one command that reports why is the whole point.
pub fn explain(err: &findery_core::Error, server: &str) -> String {
    let base = err.to_string();
    if base.contains("unreachable") || base.contains("Connection refused") {
        return format!(
            "{base}\n\n  Nothing answers at {server}, and starting a daemon did not help.\n  \
             Run `findery doctor` — it names what is missing and the command that fixes it."
        );
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use findery_core::Error;

    #[test]
    fn a_refused_connection_names_the_fix() {
        let err = Error::upstream("findery", "http://127.0.0.1:8077 unreachable: refused");
        let text = explain(&err, "http://127.0.0.1:8077");
        assert!(text.contains("findery doctor"), "{text}");
    }

    #[test]
    fn other_errors_are_passed_through_untouched() {
        let err = Error::NotFound("project `x`".into());
        assert_eq!(explain(&err, "http://x"), err.to_string());
    }
}
