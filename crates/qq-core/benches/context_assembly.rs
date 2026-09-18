//! `context_assembly`: model-context assembly and `search_history` against a
//! session with a fixed retained context and a growing compacted archive.
//! Assembly cost must follow the retained context, not the archive
//! (harness-scale audit F06); an absent-term search must stay within its scan
//! budget. Reports wall time per assembly and per search at each archive
//! size.

use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use qq_core::context_assembly_bench as bench;

const RETAINED_RUNS: usize = 4;
const TURNS_PER_RUN: u32 = 4;
const RESULT_BYTES: usize = 2_048;
const DEFAULT_ITERATIONS: u32 = 20;

fn main() {
    let iterations = std::env::var("QQ_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);
    let archive_sizes = [10_usize, 1_000, 10_000];
    println!(
        "retained_runs={RETAINED_RUNS} turns_per_run={TURNS_PER_RUN} result_bytes={RESULT_BYTES} iterations={iterations}"
    );
    for archived in archive_sizes {
        let directory = tempfile::tempdir().expect("temp dir");
        // `QQ_BENCH_KEEP_DB=path` copies the largest store there for
        // `EXPLAIN QUERY PLAN` inspection.
        let (connection, session_id) = bench::seed_compacted_session(
            &directory.path().join("sessions.sqlite3"),
            archived,
            RETAINED_RUNS,
            TURNS_PER_RUN,
            RESULT_BYTES,
        );
        // Warm.
        black_box(bench::assemble(&connection, session_id));
        let assemble = time(iterations, || {
            black_box(bench::assemble(&connection, session_id));
        });
        let search_absent = time(iterations, || {
            black_box(bench::search(&connection, session_id, "no-such-term"));
        });
        let (_, absent_truncated) = bench::search(&connection, session_id, "no-such-term");
        let search_present = time(iterations, || {
            black_box(bench::search(&connection, session_id, "needle-1"));
        });
        println!(
            "archived_runs={archived:>6} assemble={:>9.3?} search_absent={:>9.3?} (truncated={absent_truncated}) search_present={:>9.3?}",
            assemble, search_absent, search_present
        );
        if let Ok(keep) = std::env::var("QQ_BENCH_KEEP_DB") {
            drop(connection);
            std::fs::copy(directory.path().join("sessions.sqlite3"), keep).expect("copy store");
        }
    }
}

fn time(iterations: u32, mut f: impl FnMut()) -> Duration {
    let started = Instant::now();
    for _ in 0..iterations {
        f();
    }
    started.elapsed() / iterations
}
