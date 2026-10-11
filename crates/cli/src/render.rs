//! Terminal rendering. Kept apart from dispatch so `main.rs` stays a table of
//! commands rather than a table of commands interleaved with `println!`.

use findery_core::Result;
use findery_core::api::{BucketInfo, ExportReport, HealthReport, Job};
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
        println!(
            "{here} {:<22} {:<14} {}",
            p.id.to_string(),
            p.bucket,
            p.host_root
        );
    }
    if current.is_none() {
        println!("\n(no project contains the current directory)");
    }
}

/// Lists the buckets the daemon has open, marking the default one.
///
/// INTENT: a bucket whose recipe disagrees with the daemon keeps working for
/// keyword search and can still be deleted, so the conflict is printed here as
/// the repair instruction it carries rather than as a failure.
pub fn buckets(list: &[BucketInfo]) {
    if list.is_empty() {
        println!("no bucket open — the daemon creates the default one at boot");
        return;
    }
    for c in list {
        println!(
            "{} {:<22} {:>5} project(s) {:>9} chunk(s) {:>9.1} MB",
            if c.is_default { "*" } else { " " },
            c.name,
            c.projects,
            c.chunks,
            c.disk_bytes as f64 / 1e6
        );
        println!("    {}", c.path);
        if let Some(recipe) = &c.recipe {
            println!("    built with {recipe}");
        }
        if let Some(conflict) = &c.conflict {
            println!("    {conflict}");
        }
    }
}

pub fn stats(s: &IndexStats) {
    println!(
        "{} [{}]: {} files · {} chunks · {} vectors · {} ({} dims) · {} · {:.1} MB",
        s.project,
        s.bucket,
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
        // The fused score is a rank, not a distance: it says nothing about which
        // leg found the hit, and "keyword only" is a different answer to act on
        // than "both agree".
        let legs = match (hit.vector_score, hit.keyword_score) {
            (Some(v), Some(k)) => format!("  (vec {v:.3} · bm25 {k:.3})"),
            (Some(v), None) => format!("  (vec {v:.3})"),
            (None, Some(k)) => format!("  (bm25 {k:.3})"),
            (None, None) => String::new(),
        };
        println!(
            "\n[{}] {:.3}{legs}  {}  ({} {})",
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
        // A job that is still running is the only one whose line can be acted
        // on, and its counters are the reason to look at it at all.
        if let Some(progress) = &job.progress {
            println!("    {}", progress.summary());
        }
        if let Some(error) = &job.error {
            println!("    {error}");
        }
    }
}

/// Turns a transport failure into something the reader can act on.
///
/// INTENT: "connection refused" tells a user nothing. The daemon starts itself,
/// so reaching this message means something stopped it from coming up — and
/// naming the one command that reports why is the whole point.
pub fn explain(err: &findery_core::Error, server: &str) -> String {
    let base = err.to_string();
    if base.contains("timed out") {
        return format!(
            "{base}\n\n  The daemon is answering, it just did not finish in time. \
             `findery doctor` says what it is doing."
        );
    }
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
    fn a_timeout_does_not_blame_a_stopped_daemon() {
        let err = Error::upstream("findery", "http://127.0.0.1:8077 timed out: deadline");
        let text = explain(&err, "http://127.0.0.1:8077");
        assert!(text.contains("did not finish in time"), "{text}");
        assert!(!text.contains("Nothing answers"), "{text}");
    }

    #[test]
    fn other_errors_are_passed_through_untouched() {
        let err = Error::NotFound("project `x`".into());
        assert_eq!(explain(&err, "http://x"), err.to_string());
    }
}
