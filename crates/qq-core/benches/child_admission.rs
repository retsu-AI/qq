//! Durable delegation through public runtime APIs, with compilation and store
//! setup outside the timer. The three latency cases add no provider delay.
//! A separate barrier case proves read-child overlap without timing sleeps.
//! Run with `--samples N` and optionally `--case CASE`; output is JSONL.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use qq_core::{
    LoadedRuntime, MAX_CONCURRENT_CHILDREN_PER_RUN, Runtime, RuntimeLoadError, RuntimeLoadFuture,
    RuntimeLoadRequest, RuntimeLoader, SessionRuntime, SessionRuntimeOptions,
};
use qq_protocol::{
    ApprovalMode, CapabilitySupport, CommandId, CommandOutcome, DelegationRoster,
    GenerationCapabilities, ModelPricing, ModelSelection, PromptCacheCapabilities, ResolvedModel,
    ResolvedModelVersion, RunFailureKind, RunLimits, RunOutcome, SessionCommand, SessionEvent,
    SubscribeRequest,
};
use qq_provider::{
    ContentBlock, ModelRequest, Provider, ProviderEvent, ProviderStream, ProviderUsage,
};
use serde::Serialize;
use tokio::sync::Barrier;

const ROUTES: [&str; 3] = ["bench/root", "bench/child", "bench/grandchild"];
const CHILDREN: usize = 4;
const USAGE: ProviderUsage = ProviderUsage {
    input_tokens: 2,
    cache_read_input_tokens: 0,
    cache_write_input_tokens: 0,
    output_tokens: 3,
    reasoning_tokens: None,
};

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Case {
    UnboundedRead,
    FiniteRead,
    DepthTwo,
    UnboundedReadOverlap,
}

impl Case {
    const ALL: [Self; 4] = [
        Self::UnboundedRead,
        Self::FiniteRead,
        Self::DepthTwo,
        Self::UnboundedReadOverlap,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::UnboundedRead => "unbounded-read",
            Self::FiniteRead => "finite-read",
            Self::DepthTwo => "depth-two",
            Self::UnboundedReadOverlap => "unbounded-read-overlap",
        }
    }

    fn limits(self) -> RunLimits {
        match self {
            Self::FiniteRead | Self::DepthTwo => RunLimits {
                max_total_tokens: Some(10_000),
                max_cost_usd_nanos: Some(10_000),
                ..RunLimits::default()
            },
            Self::UnboundedRead | Self::UnboundedReadOverlap => RunLimits::default(),
        }
    }
}

struct Activity {
    active: AtomicUsize,
    peak: AtomicUsize,
    child_active: AtomicUsize,
    child_peak: AtomicUsize,
    child_entries: AtomicUsize,
    overlap: Barrier,
}

impl Activity {
    fn new() -> Self {
        Self {
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            child_active: AtomicUsize::new(0),
            child_peak: AtomicUsize::new(0),
            child_entries: AtomicUsize::new(0),
            overlap: Barrier::new(usize::from(MAX_CONCURRENT_CHILDREN_PER_RUN)),
        }
    }
}

struct ActiveProvider {
    activity: Arc<Activity>,
    child: bool,
}

impl Drop for ActiveProvider {
    fn drop(&mut self) {
        self.activity.active.fetch_sub(1, Ordering::SeqCst);
        if self.child {
            self.activity.child_active.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

struct FakeProvider {
    case: Case,
    depth: usize,
    activity: Arc<Activity>,
}

impl Provider for FakeProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let has_results = request
            .messages()
            .iter()
            .flat_map(|message| message.content())
            .any(|block| matches!(block, ContentBlock::ToolResult { .. }));
        let case = self.case;
        let depth = self.depth;
        let activity = Arc::clone(&self.activity);
        Box::pin(async_stream::stream! {
            let active = activity.active.fetch_add(1, Ordering::SeqCst) + 1;
            activity.peak.fetch_max(active, Ordering::SeqCst);
            if depth > 0 {
                let active = activity.child_active.fetch_add(1, Ordering::SeqCst) + 1;
                activity.child_peak.fetch_max(active, Ordering::SeqCst);
            }
            let guard = ActiveProvider { activity: Arc::clone(&activity), child: depth > 0 };
            if depth == 1 {
                let entry = activity.child_entries.fetch_add(1, Ordering::SeqCst);
                if case == Case::UnboundedReadOverlap
                    && entry < usize::from(MAX_CONCURRENT_CHILDREN_PER_RUN)
                {
                    activity.overlap.wait().await;
                }
            }
            let children = if has_results {
                0
            } else if case == Case::DepthTwo && depth < 2 {
                1
            } else if depth == 0 {
                CHILDREN
            } else {
                0
            };
            for child in 0..children {
                let id = format!("child-{child}");
                yield Ok(ProviderEvent::ToolCallStarted {
                    id: id.clone(),
                    name: "spawn_agent".to_owned(),
                });
                yield Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: id.clone(),
                    json: serde_json::json!({
                        "task": format!("read child {child}"),
                        "model": ROUTES[depth + 1],
                        "authority": "read",
                    }).to_string(),
                });
                yield Ok(ProviderEvent::ToolCallCompleted { id });
            }
            if children == 0 {
                yield Ok(ProviderEvent::OutputTextDelta { text: "done".to_owned() });
            }
            // Core may retain a completed stream during tool execution.
            // Count actual streaming, not the lifetime of that container.
            drop(guard);
            yield Ok(ProviderEvent::Completed { usage: Some(USAGE) });
        })
    }
}

struct Loader {
    plans: Vec<LoadedRuntime>,
}

impl RuntimeLoader for Loader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let result = ROUTES
            .iter()
            .position(|route| request.model.model.as_deref() == Some(*route))
            .map(|index| self.plans[index].clone())
            .ok_or_else(|| RuntimeLoadError {
                kind: RunFailureKind::Configuration,
                message: "benchmark requested an unknown model".to_owned(),
            });
        Box::pin(std::future::ready(result))
    }
}

#[derive(Serialize)]
struct Sample {
    completion_ns: u64,
    peak_provider_concurrency: usize,
    peak_descendant_provider_concurrency: usize,
    peak_active_child_runs: usize,
    completed_children: usize,
    deepest_child: u16,
    root_inclusive_tokens: Option<u64>,
    root_inclusive_cost_usd_nanos: Option<u64>,
}

#[derive(Serialize)]
struct Report {
    fixture_version: u16,
    case: Case,
    samples: Vec<Sample>,
}

fn main() {
    let mut samples = 10;
    let mut selected = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--samples" => {
                samples = arguments
                    .next()
                    .expect("--samples needs a count")
                    .parse::<usize>()
                    .expect("sample count must be an integer");
                assert!(
                    (1..=1_000).contains(&samples),
                    "samples must be between 1 and 1000"
                );
            }
            "--case" => {
                let name = arguments.next().expect("--case needs a case name");
                selected = Some(Case::ALL.into_iter().find(|case| case.name() == name)
                    .expect("unknown case: use unbounded-read, finite-read, depth-two, or unbounded-read-overlap"));
            }
            "--bench" => {}
            _ => panic!("unknown benchmark argument: {argument}"),
        }
    }
    let executor = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("Tokio runtime");
    for case in Case::ALL {
        if selected.is_some_and(|selected| selected != case) {
            continue;
        }
        let mut measured = Vec::with_capacity(samples);
        for _ in 0..samples {
            measured.push(executor.block_on(run_sample(case)));
        }
        println!(
            "{}",
            serde_json::to_string(&Report {
                fixture_version: 1,
                case,
                samples: measured,
            })
            .expect("benchmark JSON")
        );
    }
}

async fn run_sample(case: Case) -> Sample {
    let activity = Arc::new(Activity::new());
    let compile_activity = Arc::clone(&activity);
    let (directory, loader) = tokio::task::spawn_blocking(move || {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let mut plans = Vec::with_capacity(ROUTES.len());
        for (depth, route) in ROUTES.into_iter().enumerate() {
            let runtime = Runtime::new(
                FakeProvider {
                    case,
                    depth,
                    activity: Arc::clone(&compile_activity),
                },
                "bench-model",
                256,
            )
            .expect("fixture runtime")
            .with_spawn_model_routes(
                ROUTES[1..]
                    .iter()
                    .map(|route| (*route).to_owned())
                    .collect(),
            )
            .with_delegation(DelegationRoster {
                max_depth: 2,
                ..DelegationRoster::default()
            });
            let model = ResolvedModel {
                version: ResolvedModelVersion::new(1).expect("version"),
                request_shape: None,
                route: route.to_owned(),
                provider_model: "bench-model".to_owned(),
                organization: None,
                credential_profile: None,
                max_output_tokens: 256,
                context_window: None,
                pricing: Some(ModelPricing {
                    input_usd_nanos_per_token: 1,
                    output_usd_nanos_per_token: 1,
                    cache_read_usd_nanos_per_token: Some(1),
                    cache_write_usd_nanos_per_token: Some(1),
                    context_tier: None,
                    provenance: "deterministic benchmark".to_owned(),
                }),
                output_token_control: CapabilitySupport::Native,
                generation: GenerationCapabilities {
                    reasoning_effort: CapabilitySupport::Unsupported,
                },
                prompt_cache: PromptCacheCapabilities {
                    control: CapabilitySupport::Unsupported,
                    cache_read_usage: false,
                    cache_write_usage: false,
                },
            };
            plans.push(
                LoadedRuntime::compile_blocking(&runtime, model, directory.path().to_owned())
                    .expect("compile fixture plan"),
            );
        }
        (directory, Arc::new(Loader { plans }))
    })
    .await
    .expect("compile worker");
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        loader,
    )
    .await
    .expect("open fixture store");
    let resolved = runtime
        .command(
            CommandId::generate().expect("command id"),
            SessionCommand::ResolveWorkspace {
                path: directory.path().to_str().expect("UTF-8 path").to_owned(),
            },
        )
        .await
        .expect("resolve workspace");
    let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.outcome else {
        panic!("unexpected workspace receipt");
    };
    let created = runtime
        .command(
            CommandId::generate().expect("command id"),
            SessionCommand::CreateSession {
                workspace_id,
                parent_id: None,
                model: ModelSelection {
                    model: Some(ROUTES[0].to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                approval_mode: ApprovalMode::ReadOnly,
                profile: Default::default(),
                correlation: Default::default(),
            },
        )
        .await
        .expect("create root session");
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected session receipt");
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .expect("subscribe");
    let started = Instant::now();
    let measured = tokio::time::timeout(Duration::from_secs(30), async {
        let submitted = runtime
            .command(
                CommandId::generate().expect("command id"),
                SessionCommand::SubmitPrompt {
                    session_id,
                    input: vec![qq_protocol::InputPart::text(
                        "complete the delegation fixture".to_owned(),
                    )],
                    limits: case.limits(),
                    correlation: Default::default(),
                },
            )
            .await
            .expect("submit root run");
        let CommandOutcome::PromptQueued { run_id, .. } = submitted.outcome else {
            panic!("unexpected prompt receipt");
        };
        let mut completed_children = 0;
        let mut deepest_child = 0;
        let mut active_children = 0;
        let mut peak_active_child_runs = 0;
        while let Some(event) = events.next().await {
            let event = event.expect("durable event");
            match event.event {
                SessionEvent::SessionCreated { session } => {
                    if let Some(spawned) = session.spawned_by {
                        deepest_child = deepest_child.max(spawned.depth);
                    }
                }
                SessionEvent::ToolCallFinished { tool_call } => {
                    assert!(
                        !tool_call.is_error,
                        "delegation failed: {:?}",
                        tool_call.result
                    );
                }
                SessionEvent::RunStarted { run_id: child, .. } if child != run_id => {
                    active_children += 1;
                    peak_active_child_runs = peak_active_child_runs.max(active_children);
                }
                SessionEvent::RunFinished {
                    run_id: finished,
                    outcome,
                    session,
                    ..
                } => {
                    assert!(
                        matches!(outcome, RunOutcome::Completed),
                        "fixture run failed: {outcome:?}"
                    );
                    if finished == run_id {
                        let accounting = session.accounting.map(|accounting| accounting.inclusive);
                        return Sample {
                            completion_ns: u64::try_from(started.elapsed().as_nanos())
                                .expect("duration fits u64"),
                            peak_provider_concurrency: activity.peak.load(Ordering::SeqCst),
                            peak_descendant_provider_concurrency: activity
                                .child_peak
                                .load(Ordering::SeqCst),
                            peak_active_child_runs,
                            completed_children,
                            deepest_child,
                            root_inclusive_tokens: accounting
                                .and_then(|accounting| accounting.usage)
                                .map(|usage| usage.input_tokens + usage.output_tokens),
                            root_inclusive_cost_usd_nanos: accounting
                                .and_then(|accounting| accounting.estimated_cost_usd_nanos),
                        };
                    }
                    active_children -= 1;
                    completed_children += 1;
                }
                _ => {}
            }
        }
        panic!("event stream ended before root completion");
    })
    .await;
    runtime.shutdown().await.expect("shutdown fixture");
    let measured = measured.expect("delegation fixture timed out");
    assert_eq!(
        measured.completed_children,
        if case == Case::DepthTwo { 2 } else { CHILDREN }
    );
    assert_eq!(
        measured.deepest_child,
        if case == Case::DepthTwo { 2 } else { 1 }
    );
    let expected_spend = if case == Case::DepthTwo { 25 } else { 30 };
    assert_eq!(measured.root_inclusive_tokens, Some(expected_spend));
    assert_eq!(measured.root_inclusive_cost_usd_nanos, Some(expected_spend));
    assert_eq!(activity.active.load(Ordering::SeqCst), 0, "provider leaked");
    if case == Case::UnboundedReadOverlap {
        assert_eq!(
            measured.peak_descendant_provider_concurrency,
            usize::from(MAX_CONCURRENT_CHILDREN_PER_RUN)
        );
    }
    measured
}
