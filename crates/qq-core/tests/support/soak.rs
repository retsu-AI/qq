//! Test-only public-API fixture shared by the AC0 soak and its benchmark.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use futures_util::{StreamExt, stream};
use qq_core::{
    ExternalToolHost, HostCallFuture, HostCatalog, HostReadiness, HostShutdownFuture, HostTool,
    HostToolResult, LoadedRuntime, OutputCeiling, RunCancellation, Runtime, RuntimeLoadError,
    RuntimeLoadFuture, RuntimeLoadRequest, RuntimeLoader, SessionEventStream, SessionRuntime,
    SessionRuntimeOptions, ToolHints, TurnRecoveryPolicy,
};
use qq_protocol::{
    ApprovalMode, CapabilitySupport, CommandId, CommandOutcome, Correlation,
    GenerationCapabilities, InputPart, ModelSelection, PromptCacheCapabilities, ResolvedModel,
    ResolvedModelVersion, RunFailureKind, RunId, RunLimits, RunOutcome, SessionCommand,
    SessionEvent, SessionId, SubscribeRequest,
};
use qq_provider::{
    IncompleteReason, ModelRequest, Provider, ProviderError, ProviderEvent, ProviderStream,
    ToolSpec,
};
use serde::Serialize;

const TOOL: &str = "ext__soak__step";

#[derive(Clone)]
pub struct Script {
    pub turns: usize,
    pub calls_per_turn: [usize; 2],
    pub result_bytes: usize,
    pub context_window: Option<u32>,
    pub truncate_at: Vec<usize>,
    pub fault_at: Option<usize>,
    pub fault_attempts: usize,
    pub summary_fails: bool,
    pub empty_checkpoint: bool,
    pub repeat_arguments: bool,
    pub stall_after: Option<usize>,
    pub journal: Option<PathBuf>,
    pub hold_result: bool,
}

impl Script {
    pub fn tools(turns: usize, result_bytes: usize, context_window: Option<u32>) -> Self {
        assert!(
            (1..=2_000).contains(&turns),
            "fixture turns must be 1..=2000"
        );
        assert!(result_bytes <= 8_192, "fixture results must be bounded");
        Self {
            turns,
            calls_per_turn: [2, 3],
            result_bytes,
            context_window,
            truncate_at: Vec::new(),
            fault_at: None,
            fault_attempts: 0,
            summary_fails: false,
            empty_checkpoint: false,
            repeat_arguments: false,
            stall_after: None,
            journal: None,
            hold_result: false,
        }
    }
}

#[derive(Default)]
pub struct Observations {
    pub work_turns: usize,
    pub provider_requests: usize,
    pub truncations: HashSet<usize>,
    pub faults: usize,
    pub calls: Vec<u64>,
    pub turn_gaps_ns: Vec<u64>,
    last_work_entry: Option<Instant>,
}

pub struct ScriptedProvider {
    pub script: Script,
    pub observed: Arc<Mutex<Observations>>,
}

impl Provider for ScriptedProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let mut observed = self.observed.lock().expect("fixture observations");
        observed.provider_requests += 1;
        // Summarizers declare no tools; ordinary completions retain the catalog.
        if request.tools().is_empty() {
            drop(observed);
            if self.script.summary_fails {
                return Box::pin(stream::iter([Err(ProviderError::Api {
                    status: 529,
                    message: "scripted summarizer outage".to_owned(),
                })]));
            }
            return text(
                "1. Intent: finish the scripted task\n2. Decisions and constraints: local fixture only\n3. Work state: earlier steps are durable\n4. Open problems: none\n5. Next step: finish the scripted task",
            );
        }
        // The checkpoint's report notice is the request's last message
        // (ADR-0054 § 2); earlier notices stay in history, so only the last
        // message identifies the checkpoint turn.
        if request.messages().last().is_some_and(|message| {
            message.content().iter().any(|block| {
                matches!(block, qq_provider::ContentBlock::Text { text }
                    if text.contains("This execution slice is at its safe tool-call boundary."))
            })
        }) {
            drop(observed);
            if self.script.empty_checkpoint {
                return Box::pin(stream::iter([Ok(ProviderEvent::Completed {
                    usage: Some(qq_provider::ProviderUsage {
                        input_tokens: 1,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                        output_tokens: 1,
                        reasoning_tokens: None,
                    }),
                })]));
            }
            return text("Checkpoint: continue with the next uniquely numbered step.");
        }
        let turn = observed.work_turns;
        if self.script.stall_after.is_some_and(|after| turn >= after) {
            return Box::pin(stream::pending());
        }
        if self.script.fault_at == Some(turn) && observed.faults < self.script.fault_attempts {
            observed.faults += 1;
            return Box::pin(stream::iter([Err(ProviderError::Api {
                status: 529,
                message: "scripted provider outage".to_owned(),
            })]));
        }
        if self.script.truncate_at.contains(&turn) && observed.truncations.insert(turn) {
            return Box::pin(stream::iter([Ok(ProviderEvent::Incomplete {
                usage: None,
                reason: IncompleteReason::OutputTokens,
            })]));
        }
        if turn == self.script.turns {
            drop(observed);
            return text("Scripted task complete.");
        }
        let now = Instant::now();
        if let Some(previous) = observed.last_work_entry.replace(now) {
            observed.turn_gaps_ns.push(
                u64::try_from(now.duration_since(previous).as_nanos()).expect("turn gap fits u64"),
            );
        }
        observed.work_turns += 1;
        drop(observed);
        let count = self.script.calls_per_turn[turn % 2];
        assert!((1..=3).contains(&count));
        let mut events = Vec::with_capacity(count * 3 + 1);
        for index in 0..count {
            let id = format!("soak-{turn}-{index}");
            let sequence = if self.script.repeat_arguments {
                0
            } else {
                turn * 3 + index
            };
            events.push(Ok(ProviderEvent::ToolCallStarted {
                id: id.clone(),
                name: TOOL.to_owned(),
            }));
            events.push(Ok(ProviderEvent::ToolCallArgumentsDelta {
                id: id.clone(),
                json: format!(r#"{{"sequence":{sequence}}}"#),
            }));
            events.push(Ok(ProviderEvent::ToolCallCompleted { id }));
        }
        events.push(Ok(ProviderEvent::Completed { usage: None }));
        Box::pin(stream::iter(events))
    }
}

fn text(value: &str) -> ProviderStream {
    Box::pin(stream::iter([
        Ok(ProviderEvent::OutputTextDelta {
            text: value.to_owned(),
        }),
        Ok(ProviderEvent::Completed { usage: None }),
    ]))
}

struct StepHost {
    script: Script,
    observed: Arc<Mutex<Observations>>,
}

impl ExternalToolHost for StepHost {
    fn name(&self) -> &str {
        "soak"
    }

    fn catalog_blocking(&self) -> HostCatalog {
        HostCatalog {
            generation: 1,
            tools: vec![HostTool {
                spec: ToolSpec::new(
                    TOOL,
                    "Execute one uniquely numbered local fixture step.",
                    serde_json::json!({"type":"object","properties":{"sequence":{"type":"integer"}},"required":["sequence"]}),
                ),
                hints: ToolHints::default(),
            }],
            readiness: HostReadiness::Ready,
        }
    }

    fn catalog_is_current(&self, generation: u64) -> bool {
        generation == 1
    }
    fn config_grants(&self) -> Vec<String> {
        vec![TOOL.to_owned()]
    }

    fn call(&self, _name: String, arguments: String, cancelled: RunCancellation) -> HostCallFuture {
        let sequence = serde_json::from_str::<serde_json::Value>(&arguments)
            .expect("scripted arguments")["sequence"]
            .as_u64()
            .expect("sequence");
        let mut observed = self.observed.lock().expect("fixture observations");
        assert!(observed.calls.len() < 6_000, "fixture call bound");
        observed.calls.push(sequence);
        drop(observed);
        let result_bytes = self.script.result_bytes;
        let journal = self.script.journal.clone();
        let hold = self.script.hold_result;
        Box::pin(async move {
            if let Some(path) = journal {
                tokio::task::spawn_blocking(move || {
                    use std::io::Write;
                    let mut file = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .expect("fixture journal");
                    writeln!(file, "{sequence}").expect("fixture side effect");
                    file.sync_all().expect("durable fixture barrier");
                    std::fs::write(path.with_extension("synced"), b"synced")
                        .expect("post-sync fixture barrier");
                })
                .await
                .expect("journal worker");
            }
            if hold {
                cancelled.cancelled().await;
                return Err(qq_core::HostCallError::Cancelled);
            }
            let content = format!("{}\n", "s".repeat(127)).repeat(result_bytes.div_ceil(128));
            Ok(HostToolResult {
                content: content[..result_bytes].to_owned(),
                is_error: false,
            })
        })
    }

    fn readiness(&self) -> HostReadiness {
        HostReadiness::Ready
    }
    fn shutdown(&self) -> HostShutdownFuture {
        Box::pin(std::future::ready(()))
    }
}

#[derive(Clone)]
pub struct Loader {
    pub script: Script,
    pub observed: Arc<Mutex<Observations>>,
}

impl RuntimeLoader for Loader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let script = self.script.clone();
        let observed = Arc::clone(&self.observed);
        Box::pin(async move {
            let compiled = tokio::task::spawn_blocking(move || {
                let host = Arc::new(StepHost {
                    script: script.clone(),
                    observed: Arc::clone(&observed),
                });
                let runtime = Runtime::new(
                    ScriptedProvider {
                        script: script.clone(),
                        observed,
                    },
                    "soak-model",
                    1_024,
                )
                .expect("fixture runtime")
                .with_context_window(script.context_window)
                .with_output_ceiling(Some(OutputCeiling {
                    tokens: 8_192,
                    policy_bound: false,
                }))
                .with_turn_recovery(TurnRecoveryPolicy::new(
                    Duration::from_millis(1),
                    Duration::from_millis(1),
                ))
                .with_tool_host(host);
                LoadedRuntime::compile_blocking(
                    &runtime,
                    ResolvedModel {
                        version: ResolvedModelVersion::new(1).expect("nonzero version"),
                        request_shape: None,
                        route: "soak/model".to_owned(),
                        provider_model: "soak-model".to_owned(),
                        organization: None,
                        credential_profile: None,
                        max_output_tokens: 1_024,
                        context_window: script.context_window,
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
            .await;
            match compiled {
                Ok(result) => result,
                Err(error) => Err(RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: format!("soak plan compilation worker: {error}"),
                }),
            }
        })
    }
}

pub struct Fixture {
    pub runtime: SessionRuntime,
    pub events: SessionEventStream,
    pub session_id: SessionId,
    pub observed: Arc<Mutex<Observations>>,
    pub database: PathBuf,
    pub requested_turns: usize,
    pub initial_resources: Resources,
}

impl Fixture {
    pub async fn open(directory: &Path, script: Script) -> Self {
        let database = directory.join("sessions.sqlite3");
        let requested_turns = script.turns;
        let observed = Arc::new(Mutex::new(Observations::default()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(database.clone()),
            Arc::new(Loader {
                script,
                observed: Arc::clone(&observed),
            }),
        )
        .await
        .expect("fixture store");
        let receipt = runtime
            .command(
                CommandId::generate().expect("command id"),
                SessionCommand::ResolveWorkspace {
                    path: directory
                        .to_str()
                        .expect("UTF-8 fixture workspace")
                        .to_owned(),
                },
            )
            .await
            .expect("workspace");
        let CommandOutcome::WorkspaceResolved { workspace_id } = receipt.outcome else {
            panic!("workspace receipt");
        };
        let receipt = runtime
            .command(
                CommandId::generate().expect("command id"),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model_is_fallback: false,
                        model: Some("soak/model".to_owned()),
                        max_output_tokens: Some(1_024),
                        organization: None,
                    },
                    approval_mode: ApprovalMode::Full,
                    profile: qq_protocol::AgentProfileId::default(),
                    reasoning_effort: None,
                    correlation: Correlation::default(),
                },
            )
            .await
            .expect("session");
        let CommandOutcome::SessionCreated { session_id } = receipt.outcome else {
            panic!("session receipt");
        };
        let events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: receipt.committed_through,
            })
            .expect("subscription");
        let initial_resources = resources(database.clone()).await;
        Self {
            runtime,
            events,
            session_id,
            observed,
            database,
            requested_turns,
            initial_resources,
        }
    }

    pub async fn submit(&self) -> RunId {
        let receipt = self
            .runtime
            .command(
                CommandId::generate().expect("command id"),
                SessionCommand::SubmitPrompt {
                    session_id: self.session_id,
                    input: vec![InputPart::text("finish the scripted task".to_owned())],
                    limits: RunLimits {
                        max_duration_ms: Some(300_000),
                        ..RunLimits::default()
                    },
                    correlation: Correlation::default(),
                    output: None,
                },
            )
            .await
            .expect("prompt receipt");
        let CommandOutcome::PromptQueued { run_id, .. } = receipt.outcome else {
            panic!("prompt receipt");
        };
        run_id
    }

    pub async fn finish(&mut self, run_id: RunId) -> Report {
        let mut compactions = 0;
        let mut durable_calls = 0;
        let mut persisted_bytes = 0;
        let baseline_rss = self.initial_resources.rss_bytes;
        let mut peak = self.initial_resources.clone();
        let mut samples = Vec::with_capacity(24);
        let outcome = tokio::time::timeout(Duration::from_secs(300), async {
            loop {
                let envelope = self
                    .events
                    .next()
                    .await
                    .expect("event stream")
                    .expect("durable event");
                persisted_bytes +=
                    serde_json::to_vec(&envelope).expect("event encoding").len() as u64;
                match envelope.event {
                    SessionEvent::SessionCompacted { .. } => compactions += 1,
                    SessionEvent::ToolCallFinished { .. } if envelope.run_id == Some(run_id) => {
                        durable_calls += 1
                    }
                    SessionEvent::ModelTurnCompleted { turn_ordinal, .. }
                        if turn_ordinal % 100 == 0 =>
                    {
                        let sample = resources(self.database.clone()).await;
                        peak.rss_bytes = peak.rss_bytes.max(sample.rss_bytes);
                        peak.store_bytes = peak.store_bytes.max(sample.store_bytes);
                        peak.wal_bytes = peak.wal_bytes.max(sample.wal_bytes);
                        samples.push(ResourceSample {
                            turn_ordinal,
                            observed_event_bytes: persisted_bytes,
                            resources: sample,
                        });
                    }
                    SessionEvent::RunFinished { outcome, .. }
                        if envelope.run_id == Some(run_id) =>
                    {
                        break outcome;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("bounded soak");
        let final_resources = resources(self.database.clone()).await;
        peak.rss_bytes = peak.rss_bytes.max(final_resources.rss_bytes);
        peak.store_bytes = peak.store_bytes.max(final_resources.store_bytes);
        peak.wal_bytes = peak.wal_bytes.max(final_resources.wal_bytes);
        self.runtime.close().await.expect("store closes");
        let closed_store_bytes = resources(self.database.clone()).await.store_bytes;
        let observed = self.observed.lock().expect("fixture observations");
        let unique = observed.calls.iter().copied().collect::<HashSet<_>>().len();
        Report {
            outcome,
            requested_turns: self.requested_turns,
            work_turns: observed.work_turns,
            provider_requests: observed.provider_requests,
            executed_calls: observed.calls.len(),
            duplicate_sequences: observed.calls.len() - unique,
            durable_calls,
            compactions,
            persisted_event_bytes: persisted_bytes,
            baseline_rss_bytes: baseline_rss,
            peak,
            resource_samples: samples,
            closed_store_bytes,
            turn_gaps_ns: observed.turn_gaps_ns.clone(),
        }
    }
}

#[derive(Clone, Default, Serialize)]
pub struct Resources {
    pub rss_bytes: Option<u64>,
    pub store_bytes: u64,
    pub wal_bytes: u64,
}

async fn resources(database: PathBuf) -> Resources {
    tokio::task::spawn_blocking(move || {
        let rss_bytes = if cfg!(target_os = "linux") {
            let status = std::fs::read_to_string("/proc/self/status").expect("Linux RSS sample");
            Some(
                status
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("VmRSS:")
                            .and_then(|value| value.split_whitespace().next())
                            .and_then(|value| value.parse::<u64>().ok())
                            .map(|kb| kb * 1_024)
                    })
                    .expect("Linux VmRSS"),
            )
        } else {
            None
        };
        let mut wal = database.as_os_str().to_owned();
        wal.push("-wal");
        let length = |path: &Path| match std::fs::metadata(path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => panic!("resource metadata: {error}"),
        };
        Resources {
            rss_bytes,
            store_bytes: length(&database),
            wal_bytes: length(Path::new(&wal)),
        }
    })
    .await
    .expect("resource sampler")
}

#[derive(Serialize)]
pub struct ResourceSample {
    pub turn_ordinal: u32,
    pub observed_event_bytes: u64,
    pub resources: Resources,
}

#[derive(Serialize)]
pub struct Report {
    pub outcome: RunOutcome,
    pub requested_turns: usize,
    pub work_turns: usize,
    pub provider_requests: usize,
    pub executed_calls: usize,
    pub duplicate_sequences: usize,
    pub durable_calls: usize,
    pub compactions: usize,
    pub persisted_event_bytes: u64,
    pub baseline_rss_bytes: Option<u64>,
    pub peak: Resources,
    pub resource_samples: Vec<ResourceSample>,
    pub closed_store_bytes: u64,
    pub turn_gaps_ns: Vec<u64>,
}

pub async fn run(script: Script) -> Report {
    let directory = tempfile::tempdir().expect("isolated soak workspace");
    let mut fixture = Fixture::open(directory.path(), script).await;
    let run_id = fixture.submit().await;
    fixture.finish(run_id).await
}
