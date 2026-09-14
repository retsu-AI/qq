//! Cost of classifying one shell command: parse to a CST, collect simple
//! commands through wrappers and constructs, judge each against the rule
//! table. Runs on every shell call before policy, so it sits on the tool
//! hot path.
//!
//! Budget (per `docs/plans/tool-layer.md` T6): parse + classify ≤ 200 µs
//! for a 1 KiB command.

use std::{hint::black_box, time::Instant};

use qq_core::classify_bench::classify;

const DEFAULT_ITERATIONS: u64 = 2_000;

fn measure(name: &str, iterations: u64, command: &str) {
    for _ in 0..50 {
        black_box(classify(command));
    }
    let started = Instant::now();
    for _ in 0..iterations {
        black_box(classify(command));
    }
    let nanos = started.elapsed().as_nanos() / u128::from(iterations);
    println!(
        "{name}: {nanos} ns/iteration ({} bytes, {iterations} iterations) -> {}",
        command.len(),
        classify(command)
    );
}

fn main() {
    let iterations = std::env::var("QQ_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);
    measure("classify_simple", iterations, "cargo test -p qq-core");
    measure(
        "classify_pipeline",
        iterations,
        "cargo build 2>&1 | tail -n 40 && git status --short; echo done",
    );
    measure(
        "classify_forbidden_wrapped",
        iterations,
        "env -i nice -n 5 timeout 30 sudo rm -rf / ",
    );
    // ~1 KiB: a realistic long one-liner with a subshell, a loop, and quoting.
    let mut long = String::from("set -e; for f in src/*.rs crates/*/src/*.rs; do ");
    while long.len() < 900 {
        long.push_str("rg -n 'fn main' \"$f\" | head -n 3 && wc -l \"$f\" && ");
    }
    long.push_str("echo \"$f\"; done; (cd target && ls -la) | tail -5");
    measure("classify_1kib", iterations, &long);
    // 16 KiB ceiling: skips parsing.
    let huge = "echo x && ".repeat(1_700);
    measure("classify_over_limit", iterations, &huge);
}
