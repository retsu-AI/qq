//! Cancellation latency of an in-flight external tool call.
//!
//! One embedded host serves a tool that never completes. Each iteration
//! starts a call, cancels the run after a short delay, and records how long
//! the call took to settle as `Cancelled` after `cancel()` returned. Before
//! H22.2 the host noticed cancellation on a 50 ms poll tick, so this measured
//! ~0-50 ms uniformly (median ~25 ms); after, it is the scheduler's wake
//! latency.
//!
//! Prints median / p95 / max over `QQ_BENCH_ITERATIONS` (default 200).

use std::{sync::Arc, time::Duration};

use qq_core::{
    EmbeddedToolFuture, EmbeddedToolHost, ExternalToolHost, HostCallError, RunCancellation,
    ToolHints,
};

const DEFAULT_ITERATIONS: usize = 200;

fn main() {
    let iterations = std::env::var("QQ_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);
    let host: Arc<dyn ExternalToolHost> = EmbeddedToolHost::builder("bench")
        .tool(
            "hang",
            "never completes",
            serde_json::json!({"type": "object"}),
            ToolHints::default(),
            Arc::new(|_arguments: String| -> EmbeddedToolFuture {
                Box::pin(std::future::pending())
            }),
        )
        .call_timeout(Duration::from_secs(30))
        .build()
        .expect("bench host builds");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let mut latencies: Vec<Duration> = runtime.block_on(async {
        let mut latencies = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let cancelled = RunCancellation::new();
            let call = host.call(
                "ext__bench__hang".to_owned(),
                "{}".to_owned(),
                cancelled.clone(),
            );
            let trip = {
                let cancelled = cancelled.clone();
                async move {
                    // Land at an arbitrary phase of any poll period.
                    tokio::time::sleep(Duration::from_micros(1_700)).await;
                    cancelled.cancel();
                    tokio::time::Instant::now()
                }
            };
            let (outcome, cancelled_at) = tokio::join!(call, trip);
            let settled_at = tokio::time::Instant::now();
            assert!(
                matches!(outcome, Err(HostCallError::Cancelled)),
                "{outcome:?}"
            );
            latencies.push(settled_at.saturating_duration_since(cancelled_at));
        }
        latencies
    });
    latencies.sort_unstable();
    let at = |q: f64| latencies[((latencies.len() - 1) as f64 * q) as usize];
    println!(
        "host_cancel_latency: {iterations} iterations; cancel -> settled median {:?}, p95 {:?}, max {:?}",
        at(0.5),
        at(0.95),
        latencies[latencies.len() - 1]
    );
}
