//! Tool catalog compilation and progressive selection cost.
//!
//! `catalog_compile_512` compiles a catalog from the static tools plus two
//! hosts contributing 512 external declarations with realistic schemas: the
//! validation, deduplication, ordering, digest, exposure decision, and index
//! rendering a plan pays once per generation. `select_tools_rank` is one
//! ranking query against that catalog, the per-call cost of progressive
//! disclosure. `plan_compile_with_host` is a full plan compile with a 64-tool
//! host attached, for comparison with the host-less `plan_compile` bench.

use std::{hint::black_box, sync::Arc, time::Instant};

use futures_util::stream;
use qq_core::{
    ExternalToolHost, HostCallFuture, HostCatalog, HostReadiness, HostShutdownFuture, HostTool,
    HostToolResult, RunCancellation, Runtime, ToolHints,
    catalog::bench_support,
    plan::{AgentProfile, CompiledAgentPlan, HostSnapshot},
};
use qq_provider::{ModelRequest, Provider, ProviderEvent, ProviderStream, ToolSpec};

const DEFAULT_ITERATIONS: u64 = 500;

struct SilentProvider;

impl Provider for SilentProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        Box::pin(stream::iter([Ok(ProviderEvent::Completed { usage: None })]))
    }
}

fn external_tools(host: &str, count: usize) -> Vec<HostTool> {
    (0..count)
        .map(|i| HostTool {
            spec: ToolSpec::new(
                format!("mcp__{host}__operation_{i:03}"),
                format!(
                    "Operation {i} of the {host} service: inspects, transforms, or publishes \
                     records in the {host} store with bounded output."
                ),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "Record identifier" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 100 },
                        "filter": {
                            "type": "object",
                            "properties": {
                                "status": { "type": "string", "enum": ["open", "closed"] },
                                "since": { "type": "string", "format": "date-time" }
                            }
                        }
                    },
                    "required": ["id"],
                    "additionalProperties": false
                }),
            ),
            hints: ToolHints {
                read_only: i % 3 == 0,
                ..ToolHints::default()
            },
        })
        .collect()
}

struct FixedHost {
    name: &'static str,
    tools: Vec<HostTool>,
}

impl ExternalToolHost for FixedHost {
    fn name(&self) -> &str {
        self.name
    }

    fn catalog_blocking(&self) -> HostCatalog {
        HostCatalog {
            generation: 1,
            tools: self.tools.clone(),
            readiness: HostReadiness::Ready,
        }
    }

    fn catalog_is_current(&self, generation: u64) -> bool {
        generation == 1
    }

    fn config_grants(&self) -> Vec<String> {
        Vec::new()
    }

    fn call(
        &self,
        _name: String,
        _arguments: String,
        _cancelled: RunCancellation,
    ) -> HostCallFuture {
        Box::pin(std::future::ready(Ok(HostToolResult {
            content: String::new(),
            is_error: false,
        })))
    }

    fn readiness(&self) -> HostReadiness {
        HostReadiness::Ready
    }

    fn shutdown(&self) -> HostShutdownFuture {
        Box::pin(std::future::ready(()))
    }
}

fn main() {
    let iterations = std::env::var("QQ_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);

    let hosts = || {
        vec![
            ("alpha".to_owned(), external_tools("alpha", 256)),
            ("beta".to_owned(), external_tools("beta", 256)),
        ]
    };
    for _ in 0..20 {
        black_box(bench_support::compile_default_catalog(hosts()));
    }
    let started = Instant::now();
    for _ in 0..iterations {
        black_box(bench_support::compile_default_catalog(hosts()));
    }
    let per_iteration = started.elapsed().as_nanos() / u128::from(iterations);
    println!("catalog_compile_512: {per_iteration}ns/iteration over {iterations} iterations");

    let catalog = bench_support::compile_default_catalog(hosts());
    println!(
        "catalog_entries: {}; exposure: {:?}; external_schema_bytes: {}; index_bytes: {}",
        catalog.len(),
        catalog.exposure(),
        catalog.external_schema_bytes(),
        bench_support::index_len(&catalog),
    );
    let rank_iterations = iterations * 20;
    for _ in 0..100 {
        black_box(bench_support::rank(
            &catalog,
            "publish records beta store",
            8,
        ));
    }
    let started = Instant::now();
    for _ in 0..rank_iterations {
        black_box(bench_support::rank(
            &catalog,
            "publish records beta store",
            8,
        ));
    }
    let per_iteration = started.elapsed().as_nanos() / u128::from(rank_iterations);
    println!("select_tools_rank: {per_iteration}ns/iteration over {rank_iterations} iterations");

    let workspace = tempfile::tempdir().expect("temp workspace");
    let root = std::fs::canonicalize(workspace.path()).expect("canonical workspace");
    std::fs::write(root.join("AGENTS.md"), "Use rustfmt.\n".repeat(20)).expect("instructions");
    let host: Arc<dyn ExternalToolHost> = Arc::new(FixedHost {
        name: "alpha",
        tools: external_tools("alpha", 64),
    });
    let runtime = Runtime::new(SilentProvider, "bench-model", 4_096).expect("runtime");
    let profile = || {
        AgentProfile::embedded(&runtime, root.clone())
            .with_host(HostSnapshot::capture_blocking(Arc::clone(&host)))
    };
    for _ in 0..50 {
        black_box(CompiledAgentPlan::compile_blocking(profile()).expect("plan compiles"));
    }
    let started = Instant::now();
    for _ in 0..iterations {
        black_box(CompiledAgentPlan::compile_blocking(profile()).expect("plan compiles"));
    }
    let per_iteration = started.elapsed().as_nanos() / u128::from(iterations);
    println!("plan_compile_with_host_64: {per_iteration}ns/iteration over {iterations} iterations");
    let plan = CompiledAgentPlan::compile_blocking(profile()).expect("plan compiles");
    println!(
        "plan_estimated_bytes_with_host: {}; descriptor_bytes: {}",
        plan.estimated_bytes(),
        plan.descriptor_json().len()
    );
}
