//! Sync throughput against a runtime of fixed speed — the number behind the
//! claim that scanning and embedding overlap (CONV-035).
//!
//! Ignored by default: it measures, it does not assert. Reproduce with
//!
//! ```text
//! cargo test -p findery-index --release -- --ignored --nocapture a_first_sync_runs_at
//! ```
//!
//! `FINDERY_BENCH_FILES` (default 1500) sizes the generated tree,
//! `FINDERY_BENCH_US_PER_CHUNK` (default 400) the runtime's speed.

use std::time::Instant;

use super::*;
use crate::ProgressHandle;

/// A runtime that embeds one batch at a time, at a fixed cost per input — a
/// single GPU, which concurrency cannot make faster. Only overlapping the
/// scan with the embedding can shorten a sync against it.
///
/// Each call is booked on the lane until an absolute deadline and waited out
/// with the OS's high-resolution sleep: tokio's timer rounds every wait up to
/// the scheduler tick (15.6 ms on Windows), which across hundreds of small
/// batches would measure the timer instead of the sync.
fn fixed_speed_builder(per_chunk: Duration) -> BuildFn {
    let lane_free_at = Arc::new(parking_lot::Mutex::new(None::<Instant>));
    wrapped_builder(Arc::new(move |inputs| {
        let done_at = {
            let mut free_at = lane_free_at.lock();
            let start = free_at.map_or_else(Instant::now, |at| at.max(Instant::now()));
            let done_at = start + per_chunk * inputs as u32;
            *free_at = Some(done_at);
            done_at
        };
        Box::pin(async move {
            let wait = done_at.saturating_duration_since(Instant::now());
            if !wait.is_zero() {
                tokio::task::spawn_blocking(move || std::thread::sleep(wait))
                    .await
                    .map_err(|e| Error::Internal(format!("sleep: {e}")))?;
            }
            Ok(())
        })
    }))
}

/// `files` Rust files of six distinct functions each, so no chunk is a cache
/// hit for another.
fn generated_tree(files: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tree");
    for i in 0..files {
        let sub = dir.path().join(format!("src/m{}", i % 40));
        std::fs::create_dir_all(&sub).expect("mkdir");
        let body: String = (0..6)
            .map(|f| {
                format!(
                    "/// Computes step {f} of stage {i}.\npub fn stage_{i}_step_{f}(x: u64) -> u64 \
                     {{\n    let y = x.wrapping_mul({f}) ^ {i};\n    y.rotate_left({f})\n}}\n\n"
                )
            })
            .collect();
        std::fs::write(sub.join(format!("f{i}.rs")), body).expect("write");
    }
    dir
}

fn knob(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "a measurement, not an assertion: run it with --ignored --nocapture"]
async fn a_first_sync_runs_at_the_speed_of_the_runtime() {
    let files = knob("FINDERY_BENCH_FILES", 1500) as usize;
    let per_chunk = Duration::from_micros(knob("FINDERY_BENCH_US_PER_CHUNK", 400));
    let (data, tree) = (tempfile::tempdir().expect("data"), generated_tree(files));
    let idx = boot_with(&config(data.path(), ""), fixed_speed_builder(per_chunk)).await;
    let id = register(&idx, tree.path(), "bench").await;

    let started = Instant::now();
    let report = idx
        .sync(&id, SyncMode::Full, &ProgressHandle::detached())
        .await
        .expect("sync");
    let wall = started.elapsed();

    let embedding = per_chunk * report.embeddings_computed as u32;
    println!(
        "{files} files · {} chunks · wall {:.2}s · runtime busy {:.2}s · overhead {:.2}s · {:.0} \
         chunks/s",
        report.embeddings_computed,
        wall.as_secs_f64(),
        embedding.as_secs_f64(),
        wall.as_secs_f64() - embedding.as_secs_f64(),
        report.embeddings_computed as f64 / wall.as_secs_f64(),
    );
}
