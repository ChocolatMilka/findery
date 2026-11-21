//! A sync as the model reads it: the report of one that finished, or where a
//! running one is and how to follow it.

use findery_core::api::{Job, JobState};
use findery_core::model::SyncReport;

/// Warnings a report lists before it summarizes the rest.
const SHOWN_WARNINGS: usize = 5;

/// What a finished sync did.
pub fn report(report: &SyncReport) -> String {
    let mut out = format!(
        "sync `{}` ({}) done in {} ms\n  files: {} scanned · {} unchanged · {} added · {} \
         updated · {} removed · {} skipped\n  chunks: +{} −{} · embeddings: {} computed, {} \
         from cache\n",
        report.project,
        report.mode,
        report.duration_ms,
        report.files_scanned,
        report.files_unchanged,
        report.files_added,
        report.files_updated,
        report.files_removed,
        report.files_skipped,
        report.chunks_written,
        report.chunks_deleted,
        report.embeddings_computed,
        report.embeddings_cached,
    );
    out.push_str(&warnings(&report.warnings));
    out
}

/// Where a job is; for one still going, how to follow it.
pub fn job(job: &Job, waited_secs: u64) -> String {
    if let Some(done) = &job.report {
        return report(done);
    }
    if let Some(failure) = job.failure() {
        return format!("sync `{}` failed: {failure}\n", job.project);
    }
    let state = match (&job.state, &job.progress) {
        (JobState::Queued, _) => "queued, waiting for a free slot".to_string(),
        (_, Some(progress)) => progress.summary(),
        (_, None) => "running".to_string(),
    };
    let waited = match waited_secs {
        0 => String::new(),
        secs => format!(" after {secs} s"),
    };
    format!(
        "sync `{}` ({}) still running{waited} — job `{}`\n  {state}\nFollow it with sync {{job: \
         \"{}\"}}; search keeps answering from the index meanwhile.\n",
        job.project, job.kind, job.id, job.id
    )
}

/// At most a few warnings, then how many more.
fn warnings(warnings: &[String]) -> String {
    if warnings.is_empty() {
        return String::new();
    }
    let mut out = String::from("  warnings:\n");
    for warning in warnings.iter().take(SHOWN_WARNINGS) {
        out.push_str(&format!("    {warning}\n"));
    }
    let rest = warnings.len().saturating_sub(SHOWN_WARNINGS);
    if rest > 0 {
        out.push_str(&format!("    … and {rest} more\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(state: JobState) -> Job {
        Job {
            id: "3f2a".into(),
            project: "axle".into(),
            kind: "incremental".into(),
            state,
            created_at: 0,
            finished_at: None,
            progress: None,
            report: None,
            error: None,
            error_code: None,
        }
    }

    #[test]
    fn a_job_that_outlived_the_wait_names_itself_and_how_to_follow_it() {
        let text = job(&running(JobState::Queued), 20);
        assert!(
            text.contains("still running after 20 s — job `3f2a`"),
            "{text}"
        );
        assert!(text.contains("queued, waiting for a free slot"), "{text}");
        assert!(text.contains("sync {job: \"3f2a\"}"), "{text}");
    }

    #[test]
    fn a_failed_job_reads_as_the_error_it_ended_with() {
        let mut failed = running(JobState::Failed);
        failed.error = Some("sync of `axle` stopped by `findery detach`".into());
        failed.error_code = Some("cancelled".into());
        let text = job(&failed, 0);
        assert!(text.starts_with("sync `axle` failed: cancelled"), "{text}");
    }

    #[test]
    fn warnings_are_summarized_not_dumped() {
        let many: Vec<String> = (0..9).map(|i| format!("file{i}: too big")).collect();
        let text = report(&SyncReport {
            project: "axle".into(),
            warnings: many,
            ..SyncReport::default()
        });
        assert!(text.contains("… and 4 more"), "{text}");
        assert!(!text.contains("file5"), "{text}");
    }
}
