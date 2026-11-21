//! What a watcher reads while a sync runs: denominators planned before the
//! first read, a file count that lands exactly on them, an embedding count
//! that moves with every batch, and a cancel that stops the whole pass.

use findery_core::api::{Progress, ProgressPhase};

use super::*;
use crate::ProgressHandle;

/// `files` one-function Rust files: one chunk each.
fn tree(files: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tree");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    for i in 0..files {
        write_item(dir.path(), i, i);
    }
    dir
}

fn write_item(root: &Path, file: usize, value: usize) {
    std::fs::write(
        root.join(format!("src/f{file}.rs")),
        format!("pub fn item_{file}() -> u32 {{\n    {value}\n}}\n"),
    )
    .expect("write");
}

/// Samples `progress` until `done` flips, so a test sees what a poller sees.
async fn sample_until_done(
    progress: ProgressHandle,
    done: Arc<std::sync::atomic::AtomicBool>,
) -> Vec<Progress> {
    let mut seen = Vec::new();
    while !done.load(std::sync::atomic::Ordering::SeqCst) {
        seen.push(progress.snapshot());
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    seen
}

#[tokio::test]
async fn files_done_never_passes_the_pending_count_and_lands_on_it_exactly() {
    let (data, proj) = (tempfile::tempdir().expect("data"), tree(30));
    let idx = boot_with(
        &config(data.path(), ""),
        slow_builder(Duration::from_millis(5)),
    )
    .await;
    let id = register(&idx, proj.path(), "p").await;
    sync(&idx, &id).await;

    // Three edits and one rewrite with the same bytes (gate 2): four pending.
    for i in 0..3 {
        write_item(proj.path(), i, 1000 + i);
    }
    let same = std::fs::read(proj.path().join("src/f9.rs")).expect("read");
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(proj.path().join("src/f9.rs"), same).expect("rewrite");

    let progress = ProgressHandle::detached();
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let poller = tokio::spawn(sample_until_done(progress.clone(), done.clone()));
    let report = idx
        .sync(&id, SyncMode::Incremental, &progress)
        .await
        .expect("sync");
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    let samples = poller.await.expect("poller");

    for p in samples
        .iter()
        .filter(|p| p.phase != ProgressPhase::Scanning)
    {
        assert_eq!(p.files_unchanged + p.files_pending, p.files_total, "{p:?}");
        assert!(p.files_done <= p.files_pending, "{p:?}");
    }
    let last = progress.snapshot();
    assert_eq!((last.files_total, last.files_unchanged), (30, 26));
    assert_eq!(last.files_pending, 4);
    assert_eq!(last.files_done, 4, "{last:?}");
    assert_eq!(last.files_seen, 30);
    assert_eq!(report.files_unchanged, 27, "gate 1 and gate 2 together");
    assert_eq!(report.files_updated, 3);
}

#[tokio::test]
async fn embeddings_computed_moves_with_every_sub_batch_of_a_flush() {
    let (data, proj) = (tempfile::tempdir().expect("data"), tree(10));
    let mut cfg = config(data.path(), "");
    cfg.embedder.batch_size = 4;
    cfg.embedder.concurrency = 1;
    let progress = ProgressHandle::detached();
    let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
    // Each batch writes down how many embeddings a poller could read the
    // moment it was asked for.
    let (handle, log) = (progress.clone(), seen.clone());
    let observing = wrapped_builder(Arc::new(move |_| {
        log.lock().push(handle.snapshot().embeddings_computed);
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Ok(())
        })
    }));
    let idx = boot_with(&cfg, observing).await;
    let id = register(&idx, proj.path(), "p").await;

    let report = idx
        .sync(&id, SyncMode::Full, &progress)
        .await
        .expect("sync");

    assert_eq!(report.embeddings_computed, 10);
    assert_eq!(
        *seen.lock(),
        vec![0, 4, 8],
        "one flush of ten misses, counted four at a time"
    );
    let last = progress.snapshot();
    assert_eq!(last.chunks_pending_embed, 10);
    assert_eq!(last.embeddings_computed, 10);
    assert_eq!(
        last.files_pending, last.files_total,
        "--full reads everything"
    );
    assert!(last.rate > 0.0, "{last:?}");
    assert_eq!(last.eta_ms, Some(0));
}

#[tokio::test]
async fn a_cancel_during_a_flush_keeps_that_flush_and_stops_both_halves() {
    let (data, proj) = (tempfile::tempdir().expect("data"), tree(400));
    let idx = boot_with(
        &config(data.path(), ""),
        slow_builder(Duration::from_millis(200)),
    )
    .await;
    let id = register(&idx, proj.path(), "big").await;

    let progress = ProgressHandle::detached();
    let pass = {
        let (idx, id, progress) = (idx.clone(), id.clone(), progress.clone());
        tokio::spawn(async move { idx.sync(&id, SyncMode::Incremental, &progress).await })
    };
    while progress.snapshot().chunks_pending_embed == 0 {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let request = idx.gate(&id).request_cancel("a test");

    let outcome = tokio::time::timeout(Duration::from_secs(5), pass)
        .await
        .expect("the pass stops within one flush")
        .expect("task");
    assert_eq!(error_of(outcome).code(), "cancelled");
    let last = progress.snapshot();
    assert_eq!(
        last.files_done, 96,
        "the flush in flight was written: {last:?}"
    );
    assert!(last.files_done < last.files_pending);

    drop(request);
    let written = idx.stats(&id).await.expect("stats").chunks;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        idx.stats(&id).await.expect("stats").chunks,
        written,
        "nothing writes after the pass returned"
    );
    assert_eq!(written, 96);
}

#[tokio::test]
async fn a_file_that_cannot_be_read_as_text_is_a_warning_and_the_pass_completes() {
    let (data, proj) = (tempfile::tempdir().expect("data"), tree(5));
    std::fs::write(proj.path().join("src/bad.rs"), [0xff, 0xfe, 0x00, 0x41]).expect("write");
    let idx = boot(&config(data.path(), "")).await;
    let id = register(&idx, proj.path(), "p").await;

    let progress = ProgressHandle::detached();
    let report = idx
        .sync(&id, SyncMode::Incremental, &progress)
        .await
        .expect("a warning does not fail the pass");

    assert_eq!(report.files_skipped, 1);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("src/bad.rs: not valid UTF-8")),
        "{:?}",
        report.warnings
    );
    assert_eq!(report.files_added, 5);
    let last = progress.snapshot();
    assert_eq!(
        last.files_done, last.files_pending,
        "a warned file is decided"
    );
}
