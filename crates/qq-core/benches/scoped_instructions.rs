use std::{hint::black_box, time::Instant};

use qq_core::tool_bench::ScopedInstructionsSession;

const DEFAULT_ITERATIONS: u64 = 1_000;
const ROOT_BODY: &str = "root scoped instructions\n";
const PARENT_BODY: &str = "parent scoped instructions\n";
const CHILD_BODY: &str = "child scoped instructions\n";

fn measure<T>(name: &str, iterations: u64, mut run: impl FnMut() -> T) -> T {
    let started = Instant::now();
    let mut last = None;
    for _ in 0..iterations {
        last = Some(black_box(run()));
    }
    let nanos = started.elapsed().as_nanos() / u128::from(iterations);
    println!("{name}: {nanos} ns/iteration ({iterations} iterations)");
    last.expect("at least one benchmark iteration must run")
}

fn main() {
    let iterations = std::env::var("QQ_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);
    assert!(iterations > 0, "QQ_BENCH_ITERATIONS must be positive");

    let empty_directory = tempfile::tempdir().expect("empty workspace must be created");
    let empty_targets = vec!["src/module/file.rs".to_owned()];
    let empty = ScopedInstructionsSession::open(empty_directory.path());
    let empty_result = measure("root_no_instructions_cold", iterations, || {
        empty
            .discover_cold(black_box(&empty_targets))
            .expect("empty lookup must succeed")
    });
    assert!(empty_result.is_none());

    println!("root_no_instructions_cold_body: 0 bytes/body");
    let nested_directory = tempfile::tempdir().expect("nested workspace must be created");
    let nested_root = nested_directory.path();
    std::fs::create_dir_all(nested_root.join("parent/child/depth3"))
        .expect("nested directories must be created");
    std::fs::write(nested_root.join("AGENTS.md"), ROOT_BODY)
        .expect("root instructions must be written");
    std::fs::write(nested_root.join("parent/AGENTS.md"), PARENT_BODY)
        .expect("parent instructions must be written");
    std::fs::write(nested_root.join("parent/child/AGENTS.md"), CHILD_BODY)
        .expect("child instructions must be written");
    let nested_targets = vec!["parent/child/depth3/file.rs".to_owned()];
    let mut nested = ScopedInstructionsSession::open(nested_root);

    let cold = measure("nested_depth3_cold_three_sources", iterations, || {
        nested
            .discover_cold(black_box(&nested_targets))
            .expect("cold nested lookup must succeed")
            .expect("cold nested lookup must disclose scoped sources")
    });
    assert!(!cold.contains("Instruction source: AGENTS.md"));
    assert!(cold.contains("Instruction source: parent/AGENTS.md"));
    assert!(cold.contains(PARENT_BODY));
    assert!(cold.contains("Instruction source: parent/child/AGENTS.md"));
    assert!(cold.contains(CHILD_BODY));
    assert!(cold.find(PARENT_BODY) < cold.find(CHILD_BODY));
    println!(
        "nested_depth3_cold_three_sources_body: {} bytes/body",
        cold.len()
    );

    let first = nested
        .discover_warm(&nested_targets)
        .expect("initial warm lookup must succeed")
        .expect("initial warm lookup must disclose scoped sources");
    assert_eq!(first, cold);
    let warm = measure("nested_depth3_warm_unchanged", iterations, || {
        nested
            .discover_warm(black_box(&nested_targets))
            .expect("warm nested lookup must succeed")
    });
    assert!(warm.is_none());
    println!("nested_depth3_warm_unchanged_body: 0 bytes/body");
}
