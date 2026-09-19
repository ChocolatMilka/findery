//! Measures the brute-force cosine scan behind the `sqlite` vector backend.
//!
//! INTENT: "is a full scan fast enough?" is the one design question in this
//! repository that must never be answered from intuition. Run it yourself:
//!
//! ```text
//! cargo run -p findery-bench --release
//! ```
//!
//! Debug numbers are meaningless here — the kernel lives or dies on
//! autovectorization, which only the release profile enables.

use std::time::Instant;

/// The kernel actually shipped in `findery-store::sqlite_store`.
///
/// Eight independent accumulators break the dependency chain of a naive fold,
/// which is what lets the backend emit packed FMAs.
fn dot_unrolled(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0.0f32; 8];
    let blocks = a.len() / 8;
    for i in 0..blocks {
        let base = i * 8;
        for (j, slot) in acc.iter_mut().enumerate() {
            *slot += a[base + j] * b[base + j];
        }
    }
    let mut sum: f32 = acc.iter().sum();
    for i in blocks * 8..a.len() {
        sum += a[i] * b[i];
    }
    sum
}

/// The obvious version, kept as the baseline the unrolled one has to beat.
fn dot_naive(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Deterministic pseudo-random floats. A real RNG would be a dependency for
/// nothing: the distribution is irrelevant, only the memory traffic matters.
fn lcg(state: &mut u64) -> f32 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    ((*state >> 33) as f32 / (1u64 << 31) as f32) - 0.5
}

fn main() {
    if cfg!(debug_assertions) {
        eprintln!("warning: debug build — numbers below are 10-20x pessimistic.");
        eprintln!("         run with `cargo run -p findery-bench --release`.\n");
    }

    let dims: Vec<usize> = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .map(|d| vec![d])
        .unwrap_or_else(|| vec![1024, 2560]);
    let counts = [10_000usize, 50_000, 200_000];
    let mut seed = 42u64;

    println!(
        "{:>9} {:>7} {:>10} {:>12} {:>12}",
        "chunks", "dim", "RAM (MB)", "naive (ms)", "shipped (ms)"
    );
    for dim in dims {
        for &n in &counts {
            let mut data = vec![0.0f32; n * dim];
            for x in data.iter_mut() {
                *x = lcg(&mut seed);
            }
            let query: Vec<f32> = (0..dim).map(|_| lcg(&mut seed)).collect();

            let naive_ms = time_scan(&data, &query, dim, n, dot_naive);
            let shipped_ms = time_scan(&data, &query, dim, n, dot_unrolled);

            println!(
                "{:>9} {:>7} {:>10.0} {:>12.1} {:>12.1}",
                n,
                dim,
                (n * dim * 4) as f64 / 1e6,
                naive_ms,
                shipped_ms
            );
        }
    }
    println!(
        "\nRead this as: a query scans every chunk of ONE project. \
         Past a few hundred thousand chunks, or when RAM matters more than \
         exactness, switch FINDERY_STORE_BACKEND to qdrant."
    );
}

fn time_scan(
    data: &[f32],
    query: &[f32],
    dim: usize,
    n: usize,
    kernel: fn(&[f32], &[f32]) -> f32,
) -> f64 {
    let start = Instant::now();
    let mut best = f32::MIN;
    for i in 0..n {
        let score = kernel(&data[i * dim..(i + 1) * dim], query);
        if score > best {
            best = score;
        }
    }
    // Keep the loop from being optimized away entirely.
    std::hint::black_box(best);
    start.elapsed().as_secs_f64() * 1000.0
}
