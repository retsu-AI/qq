#![forbid(unsafe_code)]

#[path = "../tests/support/soak.rs"]
mod support;

use qq_protocol::RunOutcome;
use sha2::{Digest, Sha256};

fn main() {
    let goal_enabled = match std::env::var("QQ_BENCH_GOAL") {
        Ok(value) if value == "1" => true,
        Ok(value) if value == "0" => false,
        Err(std::env::VarError::NotPresent) => false,
        other => panic!("QQ_BENCH_GOAL must be 0 or 1: {other:?}"),
    };
    let mut fixture = Sha256::new();
    fixture.update(b"qq-turn-fixture-v1\0");
    for (path, source) in [
        (
            "crates/qq-core/benches/turn_overhead.rs",
            include_bytes!("turn_overhead.rs").as_slice(),
        ),
        (
            "crates/qq-core/tests/support/soak.rs",
            include_bytes!("../tests/support/soak.rs").as_slice(),
        ),
    ] {
        fixture.update((path.len() as u64).to_be_bytes());
        fixture.update(path.as_bytes());
        fixture.update((source.len() as u64).to_be_bytes());
        fixture.update(Sha256::digest(source));
    }
    let fixture_sha256 = format!("{:x}", fixture.finalize());
    let iterations = match std::env::var("QQ_BENCH_ITERATIONS") {
        Ok(value) => value
            .parse::<usize>()
            .expect("QQ_BENCH_ITERATIONS is an integer"),
        Err(std::env::VarError::NotPresent) => 3,
        Err(error) => panic!("QQ_BENCH_ITERATIONS: {error}"),
    };
    assert!(
        (1..=100).contains(&iterations),
        "iterations must be 1..=100"
    );
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("benchmark executor");
    let milestones = [10_usize, 100, 1_000];
    let mut samples = [Vec::new(), Vec::new(), Vec::new()];
    let mut compaction_counts = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let mut script = support::Script::tools(1_000, 256, Some(16 * 1_024));
        script.calls_per_turn = [1, 1];
        script.goal_enabled = goal_enabled;
        let report = tokio.block_on(support::run(script));
        assert_eq!(
            report.outcome,
            RunOutcome::Completed,
            "{:#?}",
            report.outcome
        );
        assert_eq!(report.executed_calls, 1_000);
        assert_eq!(report.durable_calls, 1_000);
        assert_eq!(report.duplicate_sequences, 0);
        assert_eq!(report.turn_gaps_ns.len(), 999);
        assert_eq!(
            report.work_requests_with_goal,
            if goal_enabled { 1_000 } else { 0 }
        );
        compaction_counts.push(report.compactions);
        for (index, milestone) in milestones.into_iter().enumerate() {
            let gaps = &report.turn_gaps_ns[milestone - 6..milestone - 1];
            let total = gaps.iter().map(|gap| u128::from(*gap)).sum::<u128>();
            samples[index].push(u64::try_from(total / 5).expect("sample fits u64"));
        }
    }
    for (milestone, mut values) in milestones.into_iter().zip(samples) {
        values.sort_unstable();
        println!(
            "{}",
            serde_json::json!({
                "metric":format!("turn_overhead_{milestone}_ns_per_turn"),
                "fixture_sha256":fixture_sha256,
                "goal_enabled":goal_enabled,
                "goal_work_requests_per_iteration":if goal_enabled {1_000} else {0},
                "compactions_per_iteration":compaction_counts,
                "unit":"ns/turn", "milestone":milestone, "iterations":iterations,
                "median":values[values.len() / 2], "samples":values,
                "boundary":"five completed work-turn gaps; fake-provider entry to next work entry, durable store included",
                "fixture_observer":"first block of first two messages in both modes; included in gaps",
                "h0_registered":false
            })
        );
    }
}
