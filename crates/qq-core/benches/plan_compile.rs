use std::{hint::black_box, sync::Arc, time::Instant};

use futures_util::stream;
use qq_core::{
    Runtime,
    plan::{AgentProfile, CompiledAgentPlan},
};
use qq_provider::{ModelRequest, Provider, ProviderEvent, ProviderStream};

const DEFAULT_ITERATIONS: u64 = 2_000;

struct SilentProvider;

impl Provider for SilentProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        Box::pin(stream::iter([Ok(ProviderEvent::Completed { usage: None })]))
    }
}

fn main() {
    let iterations = std::env::var("QQ_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);
    let workspace = tempfile::tempdir().expect("temp workspace");
    let root = std::fs::canonicalize(workspace.path()).expect("canonical workspace");
    std::fs::write(
        root.join("AGENTS.md"),
        "# Project\n\nUse rustfmt. Prefer small commits. Run `cargo test` before pushing.\n"
            .repeat(20),
    )
    .expect("instructions");
    let runtime = Runtime::new(SilentProvider, "bench-model", 4_096)
        .expect("runtime")
        .with_context_window(Some(128_000));
    let profile = || {
        AgentProfile::embedded(&runtime, root.clone())
            .with_spawn_model_routes(vec![
                "bench/worker-a".to_owned(),
                "bench/worker-b".to_owned(),
            ])
            .with_provenance(vec![
                "compiled defaults".to_owned(),
                "/home/user/.config/qq/config.ron".to_owned(),
                format!("{}/.qq/config.ron", root.display()),
            ])
    };

    for _ in 0..100 {
        black_box(CompiledAgentPlan::compile_blocking(profile()).expect("plan compiles"));
    }
    let started = Instant::now();
    for _ in 0..iterations {
        black_box(CompiledAgentPlan::compile_blocking(profile()).expect("plan compiles"));
    }
    report("plan_compile", started, iterations);

    let plan = CompiledAgentPlan::compile_blocking(profile()).expect("plan compiles");
    let descriptor = Arc::clone(plan.descriptor());
    let digest_iterations = iterations * 10;
    for _ in 0..1_000 {
        black_box(descriptor.digest().expect("descriptor encodes"));
    }
    let started = Instant::now();
    for _ in 0..digest_iterations {
        black_box(descriptor.digest().expect("descriptor encodes"));
    }
    report("plan_descriptor_digest", started, digest_iterations);
    println!(
        "plan_estimated_bytes: {} bytes; descriptor_canonical_bytes: {} bytes",
        plan.estimated_bytes(),
        descriptor
            .canonical_bytes()
            .expect("descriptor encodes")
            .len()
    );
}

fn report(name: &str, started: Instant, iterations: u64) {
    let elapsed = started.elapsed();
    println!(
        "{name}: {}ns/iteration over {iterations} iterations",
        elapsed.as_nanos() / u128::from(iterations.max(1))
    );
}
