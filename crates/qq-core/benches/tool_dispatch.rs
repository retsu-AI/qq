use std::{hint::black_box, time::Instant};

use futures_util::StreamExt;
use qq_core::Runtime;
use qq_protocol::RunCommand;

#[path = "../tests/support/read_tool.rs"]
mod support;

use support::ReadToolProvider;

const DEFAULT_ITERATIONS: u64 = 1_000;

fn main() {
    let iterations = std::env::var("QQ_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);
    let directory = tempfile::tempdir().expect("temporary workspace must be created");
    std::fs::write(directory.path().join("input.txt"), "benchmark\n")
        .expect("benchmark input must be written");
    let runtime =
        Runtime::new(ReadToolProvider, "benchmark-model", 64).expect("runtime must be configured");
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Tokio runtime must initialize");

    for _ in 0..10 {
        tokio.block_on(run_once(&runtime, directory.path().to_owned()));
    }
    let started = Instant::now();
    for _ in 0..iterations {
        tokio.block_on(run_once(&runtime, directory.path().to_owned()));
    }
    let nanos_per_iteration = started.elapsed().as_nanos() / u128::from(iterations);
    println!("read_tool_loop: {nanos_per_iteration} ns/iteration ({iterations} iterations)");
}

async fn run_once(runtime: &Runtime, workspace: std::path::PathBuf) {
    let events = runtime
        .run_in_workspace(RunCommand::new("read the input"), workspace)
        .collect::<Vec<_>>()
        .await;
    black_box(events);
}
