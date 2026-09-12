//! Hot-path cost of the tool output boundary: head+tail bounding of a shell
//! log at the 16 KiB shell bound and at the 128 KiB ceiling, the no-op path
//! for text that already fits, and secret masking over clean text.
//!
//! Budget (per `docs/plans/tool-layer.md` T1): bounding must stay well under
//! the cost of the work it bounds. Targets on a quiet host: `fits_no_op`
//! ≤ 1 µs for 8 KiB, `shell_16k_from_128k` ≤ 100 µs, `mask_clean_128k`
//! ≤ 200 µs.

use std::{hint::black_box, time::Instant};

use qq_core::tool_output_bench as bench;

const DEFAULT_ITERATIONS: u64 = 2_000;

fn lines(count: usize, width: usize) -> String {
    let mut text = String::with_capacity(count * (width + 1));
    for index in 0..count {
        let line = format!("line-{index:06} ");
        text.push_str(&line);
        text.extend(std::iter::repeat_n('x', width.saturating_sub(line.len())));
        text.push('\n');
    }
    text
}

fn measure(name: &str, iterations: u64, mut run: impl FnMut()) {
    for _ in 0..20 {
        run();
    }
    let started = Instant::now();
    for _ in 0..iterations {
        run();
    }
    let nanos = started.elapsed().as_nanos() / u128::from(iterations);
    println!("{name}: {nanos} ns/iteration ({iterations} iterations)");
}

fn main() {
    let iterations = std::env::var("QQ_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);

    let small = lines(128, 63); // 8 KiB, fits every bound
    let shell_log = lines(2_048, 63); // 128 KiB, the shell capture cap
    let huge = lines(8_192, 127); // 1 MiB, an oversized MCP result
    let one_line = "s".repeat(128 * 1024);
    let clean = lines(2_048, 63);
    // Source-like text: every trigger byte appears at a natural frequency.
    let prose = include_str!("../src/tools/output.rs")
        .repeat(4)
        .chars()
        .take(128 * 1024)
        .collect::<String>();

    measure("fits_no_op_8k", iterations, || {
        black_box(bench::bound_default(black_box(small.clone())));
    });
    measure("shell_16k_from_128k", iterations, || {
        black_box(bench::bound_shell(black_box(shell_log.clone())));
    });
    measure("default_128k_from_1m", iterations / 4, || {
        black_box(bench::bound_default(black_box(huge.clone())));
    });
    measure("shell_16k_from_one_128k_line", iterations, || {
        black_box(bench::bound_shell(black_box(one_line.clone())));
    });
    measure("mask_x_filled_128k", iterations, || {
        black_box(bench::mask(black_box(clean.clone())));
    });
    measure("mask_source_128k", iterations, || {
        black_box(bench::mask(black_box(prose.clone())));
    });
    measure("clone_128k_control", iterations, || {
        black_box(black_box(clean.clone()));
    });
}
