//! `store_output_batch`: eight concurrent runs each streaming small text
//! deltas through the durable session runtime, so the store's output lane
//! sees interleaved `append_text` jobs from eight streams. Reports the batch
//! wall time and the derived per-delta cost. Exercises the group-commit path
//! end to end through the public runtime API; the store itself is private.

use std::{
    hint::black_box,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use futures_util::{StreamExt, stream};
use qq_core::{
    LoadedRuntime, Runtime, RuntimeLoadError, RuntimeLoadFuture, RuntimeLoadRequest, RuntimeLoader,
    SessionRuntime, SessionRuntimeOptions,
};
use qq_protocol::{
    ApprovalMode, CapabilitySupport, CommandId, CommandOutcome, GenerationCapabilities,
    ModelSelection, PromptCacheCapabilities, ResolvedModel, ResolvedModelVersion, RunFailureKind,
    RunId, SessionCommand, SessionEvent, SubscribeRequest,
};
use qq_provider::{ModelRequest, Provider, ProviderEvent, ProviderStream};

const STREAMS: usize = 8;
const DELTAS_PER_STREAM: usize = 256;
const DELTA_BYTES: usize = 64;
const DEFAULT_ITERATIONS: u64 = 5;

/// Emits `DELTAS_PER_STREAM` deltas of `DELTA_BYTES`, yielding between each
/// so the eight streams interleave at the store instead of arriving as eight
/// contiguous bursts.
struct DeltaProvider;

impl Provider for DeltaProvider {
    fn stream(&self, _: ModelRequest) -> ProviderStream {
        Box::pin(
            stream::iter((0..DELTAS_PER_STREAM).map(|_| {
                Ok(ProviderEvent::OutputTextDelta {
                    text: "x".repeat(DELTA_BYTES),
                })
            }))
            .then(|event| async move {
                tokio::task::yield_now().await;
                event
            })
            .chain(stream::iter([Ok(ProviderEvent::Completed { usage: None })])),
        )
    }
}

struct DeltaLoader;

impl RuntimeLoader for DeltaLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        Box::pin(async move {
            let runtime = Runtime::new(DeltaProvider, "bench-model", 4096).map_err(|error| {
                RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                }
            })?;
            LoadedRuntime::compile_blocking(
                &runtime,
                ResolvedModel {
                    version: ResolvedModelVersion::new(1).expect("non-zero"),
                    request_shape: None,
                    route: "bench/model".to_owned(),
                    provider_model: "bench-model".to_owned(),
                    organization: None,
                    credential_profile: None,
                    max_output_tokens: 4096,
                    context_window: None,
                    pricing: None,
                    output_token_control: CapabilitySupport::Native,
                    generation: GenerationCapabilities {
                        reasoning_effort: CapabilitySupport::Unsupported,
                    },
                    prompt_cache: PromptCacheCapabilities {
                        control: CapabilitySupport::Unsupported,
                        cache_read_usage: false,
                        cache_write_usage: false,
                    },
                },
                PathBuf::from(request.workspace),
            )
            .map_err(|error| RuntimeLoadError {
                kind: RunFailureKind::Configuration,
                message: error.to_string(),
            })
        })
    }
}

fn main() {
    let iterations = std::env::var("QQ_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("Tokio runtime must initialize");
    let samples: Vec<Duration> = tokio.block_on(async {
        let mut samples = Vec::with_capacity(iterations as usize);
        for _ in 0..iterations {
            samples.push(run_batch().await);
        }
        samples
    });
    let total: Duration = samples.iter().sum();
    let per_batch = total / u32::try_from(iterations).expect("iterations fit u32");
    let deltas = (STREAMS * DELTAS_PER_STREAM) as u128;
    println!(
        "store_output_batch: {} ms/batch ({STREAMS} streams x {DELTAS_PER_STREAM} x {DELTA_BYTES} B; {} us/delta; {iterations} iterations)",
        per_batch.as_millis(),
        per_batch.as_micros() / deltas
    );
}

async fn run_batch() -> Duration {
    let directory = tempfile::tempdir().expect("temporary directory");
    let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
    options.max_active_runs = STREAMS;
    let runtime = SessionRuntime::open(options, Arc::new(DeltaLoader))
        .await
        .expect("runtime opens");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let outcome = runtime
        .command(
            CommandId::generate().expect("id"),
            SessionCommand::ResolveWorkspace {
                path: workspace.to_str().expect("utf-8").to_owned(),
            },
        )
        .await
        .expect("resolve")
        .outcome;
    let CommandOutcome::WorkspaceResolved { workspace_id } = outcome else {
        panic!("unexpected receipt: {outcome:?}");
    };
    let mut sessions = Vec::with_capacity(STREAMS);
    let mut cursor = None;
    for _ in 0..STREAMS {
        let receipt = runtime
            .command(
                CommandId::generate().expect("id"),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model: Some("bench/model".to_owned()),
                        max_output_tokens: Some(4096),
                        organization: None,
                    },
                    approval_mode: ApprovalMode::ReadOnly,
                    profile: Default::default(),
                    correlation: Default::default(),
                },
            )
            .await
            .expect("create");
        let CommandOutcome::SessionCreated { session_id } = receipt.outcome else {
            panic!("unexpected receipt");
        };
        sessions.push(session_id);
        cursor = Some(receipt.committed_through);
    }
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: cursor.expect("at least one session"),
        })
        .expect("subscribe");

    let started = Instant::now();
    let pending: Arc<Mutex<Vec<RunId>>> = Arc::new(Mutex::new(Vec::with_capacity(STREAMS)));
    for session_id in sessions {
        let receipt = runtime
            .command(
                CommandId::generate().expect("id"),
                SessionCommand::SubmitPrompt {
                    session_id,
                    input: vec![qq_protocol::InputPart::text("stream".to_owned())],
                    limits: Default::default(),
                    correlation: Default::default(),
                    output: None,
                },
            )
            .await
            .expect("submit");
        let CommandOutcome::PromptQueued { run_id, .. } = receipt.outcome else {
            panic!("unexpected receipt");
        };
        pending.lock().expect("lock").push(run_id);
    }
    let mut finished = 0;
    while finished < STREAMS {
        let envelope = events.next().await.expect("stream open").expect("event");
        if matches!(envelope.event, SessionEvent::RunFinished { .. }) {
            finished += 1;
        }
        black_box(&envelope);
    }
    let elapsed = started.elapsed();
    runtime.shutdown().await.expect("shutdown");
    elapsed
}
