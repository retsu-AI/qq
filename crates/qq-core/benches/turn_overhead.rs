//! AC0: fake-provider entry-to-entry wall time near turns 10/100/1000.
//! Includes tool dispatch, durable settlement, planning, and checkpoint /
//! compaction work; excludes startup and live-provider network latency.

#![forbid(unsafe_code)]

#[path = "../tests/support/soak.rs"]
mod support;

use qq_protocol::RunOutcome;

fn main() {
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
    for _ in 0..iterations {
        let mut script = support::Script::tools(1_000, 256, Some(16 * 1_024));
        script.calls_per_turn = [1, 1];
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
        for (index, milestone) in milestones.into_iter().enumerate() {
            // Five adjacent completed work-turn gaps ending at this milestone.
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
                "unit":"ns/turn", "milestone":milestone, "iterations":iterations,
                "median":values[values.len() / 2], "samples":values,
                "boundary":"five completed work-turn gaps; fake-provider entry to next work entry, durable store included",
                "h0_registered":false
            })
        );
    }
}
