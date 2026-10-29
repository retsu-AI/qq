use super::loop_guard::{CallKey, LoopGuard};
use crate::*;

use sha2::Digest as _;
use tracing::Instrument as _;

static NEXT_DIAGNOSTIC_RUN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub(crate) fn next_diagnostic_run() -> u64 {
    NEXT_DIAGNOSTIC_RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

struct RunScope {
    handled_interrupt: u64,
    max_output_tokens: u32,
    budget: BudgetMeter,
    audit_triggers: runtime::AuditTriggers,
    audit_actions: Vec<runtime::AuditedAction>,
    audit_revisions: u16,
    output_repairs: u8,
    goal_handoff_requested: bool,
    goal_final_slot_announced: bool,
    live_turns: Vec<u32>,
    summarizer_rejected_turns: u8,
    inherited_compaction_attempted: bool,
    summary_fold: runtime::SummaryFold,
    summary_fold_text: String,
    stall: runtime::StallScope,
    loop_guard: LoopGuard,
    guard_eligible: Vec<(ToolCallId, CallKey)>,
    slices: u32,
    pins: catalog::PinSet,
    tool_specs: Arc<[ToolSpec]>,
    tool_schema: runtime::ToolSchemaMeasurement,
    prompt_identity: Option<Arc<RunPromptIdentity>>,
    call_effects: HashMap<String, catalog::EffectClass>,
    argument_errors: Vec<[u8; 32]>,
}

const MAX_ARGUMENT_ERRORS_SEEN: usize = 64;

fn is_argument_error(text: &str) -> bool {
    let text = text
        .strip_prefix("edit ")
        .and_then(|rest| rest.split_once(": "))
        .filter(|(index, _)| index.bytes().all(|byte| byte.is_ascii_digit()))
        .map_or(text, |(_, rest)| rest);
    text.starts_with("invalid arguments") || text.starts_with("invalid_")
}

impl RunScope {
    fn hint_repeated_argument_error(&mut self, name: &str, result: &mut tools::ToolOutput) {
        if !result.is_error
            || result.disposition != tools::ResultDisposition::Refused
            || !is_argument_error(&result.model_text)
        {
            return;
        }
        let Some(example) = tools::minimal_call(name) else {
            return;
        };
        let mut hash = sha2::Sha256::new();
        hash.update(name.as_bytes());
        hash.update([0]);
        hash.update(result.model_text.as_bytes());
        let digest: [u8; 32] = hash.finalize().into();
        if self.argument_errors.contains(&digest) {
            result.model_text.push_str(&format!(
                "\nthe same error as before; a minimal valid call: {name} {example}"
            ));
            return;
        }
        if self.argument_errors.len() == MAX_ARGUMENT_ERRORS_SEEN {
            self.argument_errors.remove(0);
        }
        self.argument_errors.push(digest);
    }

    fn admission_key(&self, id: ToolCallId) -> Option<CallKey> {
        self.guard_eligible
            .iter()
            .find_map(|(candidate, key)| (*candidate == id).then_some(*key))
    }

    fn observe_tool(
        &mut self,
        call: &RuntimeToolCall,
        result: &RetainedResult,
        network: &NetworkPolicy,
        entry: Option<&catalog::ToolEntry>,
    ) {
        let Some(key) = self.admission_key(call.id) else {
            return;
        };
        match result.disposition {
            tools::ResultDisposition::Observed => {
                self.loop_guard
                    .executed_digest(key, result.result_digest, result.is_error);
                let mutation =
                    match approval::classify(call.effect, &call.name, &call.arguments, network) {
                        approval::ToolClass::Mutating => true,
                        approval::ToolClass::External => {
                            !entry.is_some_and(|entry| entry.hints.read_only)
                        }
                        approval::ToolClass::Shell { .. } => result.process_work,
                        approval::ToolClass::ReadOnly
                        | approval::ToolClass::Interactive { .. }
                        | approval::ToolClass::Network { .. } => false,
                    };
                if !result.is_error && mutation {
                    self.loop_guard.mutation();
                }
            }
            tools::ResultDisposition::Refused => {
                self.loop_guard.unexecuted();
            }
            tools::ResultDisposition::Unobserved => {
                self.loop_guard.unobserved();
            }
            tools::ResultDisposition::Interrupted => {}
        }
        if result.human_input {
            self.loop_guard.mutation();
        }
    }
}

struct WindowScope {
    reducible_message_bytes: u64,
    irreducible_message_bytes: u64,
    compatible_request: Option<(u64, u64, u64, u64)>,
    model_text_bytes: usize,
}

struct SliceScope {
    slice_tool_calls: usize,
    continuing_slice: bool,
    placed_report: Option<runtime::TurnNotice>,
    pending_notice: Option<runtime::TurnNotice>,
    pending_goal_notices: Vec<runtime::OrderedTurnNotice>,
    pending_deliveries: usize,
    pending_reserved_goal_notices: usize,
}

struct TurnScope {
    turn_retries: u16,
    output_continuations: u16,
    empty_output_retries: u16,
    reasoning_only_truncations: u16,
    provider_overflowed: bool,
    wait_delivered_for: Option<u32>,
}

enum TurnStep<T> {
    Emit(RuntimeEvent),
    Ready(T),
    NextTurn,
    Stop,
}

struct RunState {
    env: RunEnvironment,
    messages: Arc<Vec<Message>>,
    steering: Option<runtime::SteeringReceiver>,
    run: RunScope,
    window: WindowScope,
    slice: SliceScope,
    recovery: TurnScope,
}
fn may_change_files(
    call: &RuntimeToolCall,
    network: &NetworkPolicy,
    entry: Option<&catalog::ToolEntry>,
) -> bool {
    match approval::classify(call.effect, &call.name, &call.arguments, network) {
        approval::ToolClass::Mutating => true,
        approval::ToolClass::External => !entry.is_some_and(|entry| entry.hints.read_only),
        approval::ToolClass::Shell { read_only, .. } => !read_only,
        approval::ToolClass::ReadOnly
        | approval::ToolClass::Interactive { .. }
        | approval::ToolClass::Network { .. } => false,
    }
}

struct RunEnvironment {
    plan: Arc<plan::CompiledAgentPlan>,
    cancelled: RunCancellation,
    gate: Arc<dyn ToolGate>,
    file_state: Arc<workspace::FileState>,
    spawner: Option<Arc<dyn SubagentSpawner>>,
    history: Option<Arc<dyn HistorySearcher>>,
    goal: Option<sessions::GoalRunInput>,
    goal_spend_tracking: bool,
    goal_paid: Option<sessions::GoalPaidInput>,
    spills: Option<Arc<dyn runtime::SpillReader>>,
    compactor: Option<Arc<dyn runtime::InRunCompactor>>,
    audit_hook: Option<Arc<dyn runtime::AuditHook>>,
    tool_tasks: tools::ToolTasks,
    output: Option<Arc<output::CompiledOutputSchema>>,
    summarizer: Option<plan::PromptPrefixKey>,
    inherited_effects: Vec<Option<catalog::EffectClass>>,
    limits: RunLimits,
    deadline: Option<runtime::RunDeadline>,
    system: Arc<str>,
    system_prompt_hash: ContentHash,
    base_specs: Arc<[ToolSpec]>,
    static_filter: Option<catalog::StaticFilter>,
    reducible_messages: usize,
    audit_prompt: String,
    allow_tools: bool,
    model_max_output_tokens: u32,
    output_ceiling: u32,
    ceiling_by_policy: bool,
    cache_key: Option<Arc<str>>,
}
struct PreparedTurn {
    turn_ordinal: u32,
    budget_final_turn: bool,
    final_answer_turn: bool,
    checkpoint_turn: bool,
    slice_report: bool,
    request_system: Arc<str>,
    request_has_tools: bool,
    request_system_hash: ContentHash,
    system_bytes: u64,
    tool_schema_bytes: u64,
}
struct CompactedTurn {
    prepared: PreparedTurn,
    message_bytes: u64,
    compatible_input_tokens: Option<u64>,
}
struct AttemptScope {
    rejected_before_work: bool,
    activity: RunActivity,
    pending_calls: Vec<PendingToolCall>,
    calls_by_provider_id: HashMap<String, usize>,
    replay: Option<Arc<str>>,
    blocks: Vec<TurnBlock>,
    terminal_usage: Option<qq_protocol::TokenUsage>,
    completed: bool,
    reasoning_bytes: usize,
    reasoning_observed: bool,
    open_reasoning: Option<qq_provider::ReasoningKind>,
    interrupted_turn: bool,
    truncated_turn: bool,
    truncation_reason: qq_provider::IncompleteReason,
    streamed_visible_output: bool,
    turn_fault: Option<(RunFailureKind, String)>,
    retry_after: Option<std::time::Duration>,
}
struct StreamedTurn {
    prepared: PreparedTurn,
    message_bytes: u64,
    attempt: AttemptScope,
}
struct SettledTurn {
    turn_ordinal: u32,
    final_answer_turn: bool,
    assistant: Message,
    calls: Vec<RuntimeToolCall>,
}
enum TurnRoute {
    Candidate {
        turn_ordinal: u32,
        assistant: Message,
    },
    Tools {
        turn_ordinal: u32,
        final_answer_turn: bool,
        calls: Vec<RuntimeToolCall>,
    },
}
struct WaitScope {
    delivery_retry: Duration,
}

struct ToolScope {
    interrupted: bool,
}
struct ExecutionScope {
    output_closed: bool,
}
struct ResultScope {
    budget: tools::TurnOutputBudget,
}

struct ToolOutcomes {
    turn_ordinal: u32,
    final_answer_turn: bool,
    calls: Vec<RuntimeToolCall>,
    results: Vec<Option<RetainedResult>>,
}
impl plan::CompiledAgentPlan {
    pub(crate) fn execute(
        self: &Arc<Self>,
        messages: Vec<Message>,
        cancelled: RunCancellation,
        gate: Arc<dyn ToolGate>,
        file_state: Arc<workspace::FileState>,
        mut capabilities: RunCapabilities,
    ) -> RuntimeStream {
        let started = capabilities
            .execution_started
            .unwrap_or_else(tokio::time::Instant::now);
        let deadline = runtime::RunDeadline::new(capabilities.limits, started);
        let deadline_resources = deadline.map(|_| {
            let tools = capabilities.tool_tasks.get_or_insert_with(Default::default);
            (
                cancelled.clone(),
                tools.clone(),
                capabilities.spawner.clone(),
                capabilities.audit_hook.clone(),
            )
        });
        let plan = Arc::clone(self);
        let mut run_span = None;
        let events: RuntimeStream = Box::pin(stream! {
            let run_parent = tracing::Span::current();

            let _cancel_on_drop = CancelOnDrop(cancelled.clone());
            let mut state = {
                let initialization = plan.initialize(messages, cancelled, gate, file_state, capabilities, (started, deadline));
                tokio::pin!(initialization);
                loop {
                    match initialization.next().await {
                        Some(TurnStep::Emit(event)) => yield event,
                        Some(TurnStep::Ready(state)) => break state,
                        Some(TurnStep::Stop) => return,
                        Some(TurnStep::NextTurn) | None => unreachable!("initialization ended without state"),
                    }
                }
            };
            'turns: for turn_ordinal in 1..=u32::MAX {

                let turn_span = tracing::info_span!(target: "qq::diagnostic", parent: &run_parent, "turn", turn_ordinal);
                let turn = {
                    let phase = state.prepare_turn(turn_ordinal);
                    tokio::pin!(phase);
                    loop {
                        match phase.next().instrument(turn_span.clone()).await {
                            Some(TurnStep::Emit(event)) => yield event,
                            Some(TurnStep::Ready(value)) => break value,
                            Some(TurnStep::NextTurn) => continue 'turns,
                            Some(TurnStep::Stop) => return,
                            None => unreachable!("phase ended without a typed outcome"),
                        }
                    }
                };
                let turn = {
                    let phase = state.compact_turn(turn);
                    tokio::pin!(phase);
                    loop {
                        match phase.next().instrument(turn_span.clone()).await {
                            Some(TurnStep::Emit(event)) => yield event,
                            Some(TurnStep::Ready(value)) => break value,
                            Some(TurnStep::NextTurn) => continue 'turns,
                            Some(TurnStep::Stop) => return,
                            None => unreachable!("phase ended without a typed outcome"),
                        }
                    }
                };
                let turn = {
                    let phase = state.stream_turn(turn);
                    tokio::pin!(phase);
                    loop {
                        match phase.next().instrument(turn_span.clone()).await {
                            Some(TurnStep::Emit(event)) => yield event,
                            Some(TurnStep::Ready(value)) => break value,
                            Some(TurnStep::NextTurn) => continue 'turns,
                            Some(TurnStep::Stop) => return,
                            None => unreachable!("phase ended without a typed outcome"),
                        }
                    }
                };
                let turn = {
                    let phase = state.settle_turn(turn);
                    tokio::pin!(phase);
                    loop {
                        match phase.next().instrument(turn_span.clone()).await {
                            Some(TurnStep::Emit(event)) => yield event,
                            Some(TurnStep::Ready(value)) => break value,
                            Some(TurnStep::NextTurn) => continue 'turns,
                            Some(TurnStep::Stop) => return,
                            None => unreachable!("phase ended without a typed outcome"),
                        }
                    }
                };
                let res = {
                    let phase = state.route_turn(turn);
                    tokio::pin!(phase);
                    loop {
                        match phase.next().instrument(turn_span.clone()).await {
                            Some(TurnStep::Emit(event)) => yield event,
                            Some(TurnStep::Ready(value)) => break value,
                            Some(TurnStep::NextTurn) => continue 'turns,
                            Some(TurnStep::Stop) => return,
                            None => unreachable!("phase ended without a typed outcome"),
                        }
                    }
                }; match res {
                    TurnRoute::Candidate { turn_ordinal, assistant } => {
                        {
                    let phase = state.finish_candidate(turn_ordinal, assistant);
                    tokio::pin!(phase);
                    loop {
                        match phase.next().instrument(turn_span.clone()).await {
                            Some(TurnStep::Emit(event)) => yield event,
                            Some(TurnStep::Ready(value)) => break value,
                            Some(TurnStep::NextTurn) => continue 'turns,
                            Some(TurnStep::Stop) => return,
                            None => unreachable!("phase ended without a typed outcome"),
                        }
                    }
                };
                    }
                    TurnRoute::Tools { turn_ordinal, final_answer_turn, calls } => {
                        let outcomes = {
                    let phase = state.settle_tools(turn_ordinal, final_answer_turn, calls);
                    tokio::pin!(phase);
                    loop {
                        match phase.next().instrument(turn_span.clone()).await {
                            Some(TurnStep::Emit(event)) => yield event,
                            Some(TurnStep::Ready(value)) => break value,
                            Some(TurnStep::NextTurn) => continue 'turns,
                            Some(TurnStep::Stop) => return,
                            None => unreachable!("phase ended without a typed outcome"),
                        }
                    }
                };
                        {
                    let phase = state.close_tool_turn(outcomes);
                    tokio::pin!(phase);
                    loop {
                        match phase.next().instrument(turn_span.clone()).await {
                            Some(TurnStep::Emit(event)) => yield event,
                            Some(TurnStep::Ready(value)) => break value,
                            Some(TurnStep::NextTurn) => continue 'turns,
                            Some(TurnStep::Stop) => return,
                            None => unreachable!("phase ended without a typed outcome"),
                        }
                    }
                };
                    }
                }
            }
            yield RuntimeEvent::Failed {
                kind: RunFailureKind::Policy,
                message: "run exhausted the durable u32 model-turn ordinal space".to_owned(),
            };
        });
        let events = match (deadline, deadline_resources) {
            (Some(deadline), Some((cancelled, tools, spawner, audit_hook))) => {
                deadline.enforce(events, cancelled, tools, spawner, audit_hook)
            }
            _ => events,
        };
        let mut events = Some(events);
        Box::pin(futures_util::stream::poll_fn(move |cx| {
            let Some(events_stream) = events.as_mut() else {
                return std::task::Poll::Ready(None);
            };
            let span = run_span.get_or_insert_with(|| {
                let current = tracing::Span::current();
                if current.metadata().is_some_and(|metadata| metadata.target() == "qq::diagnostic" && metadata.name() == "run") {
                    current
                } else {
                    tracing::info_span!(target: "qq::diagnostic", "run", run_ordinal = next_diagnostic_run())
                }
            });
            let result = span.in_scope(|| events_stream.as_mut().poll_next(cx));
            if matches!(
                &result,
                std::task::Poll::Ready(
                    None | Some(
                        RuntimeEvent::Completed { .. }
                            | RuntimeEvent::Failed { .. }
                            | RuntimeEvent::BudgetExhausted { .. }
                    )
                )
            ) {
                events.take();
                run_span.take();
            }
            result
        }))
    }
}
impl plan::CompiledAgentPlan {
    fn initialize(
        self: Arc<Self>,
        messages: Vec<Message>,
        cancelled: RunCancellation,
        gate: Arc<dyn ToolGate>,
        file_state: Arc<workspace::FileState>,
        capabilities: RunCapabilities,
        timing: (tokio::time::Instant, Option<runtime::RunDeadline>),
    ) -> impl futures_util::Stream<Item = TurnStep<RunState>> + Send {
        let (started, deadline) = timing;
        let plan = Arc::clone(&self);
        let mut messages = Arc::new(messages);
        let model_max_output_tokens = plan.runtime.max_output_tokens;
        let ceiling_by_policy = plan
            .runtime
            .output_ceiling
            .is_some_and(|ceiling| ceiling.policy_bound);
        let output_ceiling = plan
            .runtime
            .output_ceiling
            .map_or(model_max_output_tokens, |ceiling| {
                ceiling.tokens.max(model_max_output_tokens)
            });
        let catalog = Arc::clone(&plan.catalog);
        let skills = Arc::clone(&plan.skills);
        let pack_roots = Arc::clone(&plan.pack_roots);
        let context_sources = Arc::clone(&plan.runtime.context_sources);
        let context_cache = Arc::clone(&plan.runtime.context_cache);
        let profile_name = plan.descriptor().profile.as_str().to_owned();
        stream! {
                   let RunCapabilities {
                       spawner,
                       allow_guidance,
                       slash_is_literal,
                       allow_tools,
                       read_only,
                       unattended,
                       max_output_tokens,
                       limits,
                       execution_started: _,
                       pricing,
                       history,
                       goal,
                       goal_spend_tracking,
                       goal_paid,
                       spills,
                       steering,
                       compactor,
                       audit_hook,
                       tool_tasks,
                       output,
                       subagent,
                       stall_exempt,
                       summarizer,
                       inherited_effects,
                       cache_key,
                       mode,
                       snapshots,
                   } = capabilities;
                   let tool_tasks = tool_tasks.unwrap_or_default();
                   let steering = steering;
                   let handled_interrupt = steering
                       .as_ref()
                       .map_or(0, |steering| *steering.interrupts.borrow());
                   let max_output_tokens = max_output_tokens
                       .unwrap_or(model_max_output_tokens)
                       .min(model_max_output_tokens);
                   let empty_output_retries = 0_u16;
                   let reasoning_only_truncations = 0_u16;
                   let budget = BudgetMeter::new(limits, pricing, started);
                   yield TurnStep::Emit( RuntimeEvent::Started);
                   messages = Arc::new(usable_conversation(Arc::unwrap_or_clone(messages)));
                   if messages.is_empty() {
                       yield TurnStep::Emit( RuntimeEvent::Failed {
                           kind: RunFailureKind::InvalidCommand,
                           message: "conversation messages must not be empty".to_owned(),
                       });
                       yield TurnStep::Stop; return;
                   }

                   let parsed_invocation = match if allow_guidance && !slash_is_literal {
                       workspace::parse_invocation(Arc::make_mut(&mut messages).as_mut_slice())
                   } else {
                       Ok(workspace::ParsedInvocation {
                           guidance: None,
                       })
                   } {
                       Ok(request) => request,
                       Err(error) => {
                           yield TurnStep::Emit( RuntimeEvent::Failed {
                               kind: RunFailureKind::InvalidCommand,
                               message: error.to_string(),
                           });
                           yield TurnStep::Stop; return;
                       }
                   };

                   let workspace = plan.workspace.clone();
                   let workspace_instructions = &plan.instructions;
                   let selected_guidance = match parsed_invocation.guidance {
                       None => None,
                       Some(request) => match workspace::prepare_guidance(
                           workspace.clone(),
                           Arc::clone(&pack_roots),
                           Arc::clone(&skills),
                           cancelled.clone(),
                           request,
                           &tool_tasks,
                       )
                       .await
                       {
                           Ok(guidance) => Some(guidance),
                           Err(workspace::WorkspacePreparationError::Guidance(error)) => {
                               yield TurnStep::Emit( RuntimeEvent::Failed {
                                   kind: RunFailureKind::InvalidCommand,
                                   message: error.to_string(),
                               });
                               yield TurnStep::Stop; return;
                           }
                           Err(error) => {
                               yield TurnStep::Emit( RuntimeEvent::Failed {
                                   kind: RunFailureKind::Configuration,
                                   message: error.to_string(),
                               });
                               yield TurnStep::Stop; return;
                           }
                       },
                   };
                   let mut context_blocks = String::new();
                   let mut context_records = Vec::new();
                   if !context_sources.is_empty() && summarizer.is_none() {
                       let latest_user_text = messages
                           .last()
                           .filter(|message| message.role() == Role::User)
                           .map(|message| {
                               message
                                   .content()
                                   .iter()
                                   .filter_map(|block| match block {
                                       ContentBlock::Text { text } => Some(text.as_str()),
                                       ContentBlock::ToolCall { .. } | ContentBlock::ToolResult { .. } => None,
                                   })
                                   .collect::<Vec<_>>()
                                   .join("\n")
                           })
                           .unwrap_or_default();
                       let request = context_source::ContextRequest {
                           profile: profile_name.clone(),
                           workspace: workspace.path().display().to_string(),
                           latest_user_text,
                           budget: context_source::ContextBudget::default(),
                       };
                       match context_source::fetch_all(
                           &context_sources,
                           &context_cache,
                           request,
                           cancelled.clone(),
                       )
                       .await
                       {
                           Ok(rendered) => {
                               for context in rendered {
                                   context_blocks.push_str(&context.text);
                                   context_records.push(context.record);
                               }
                           }
                           Err((message, record)) => {
                               context_records.push(record);
                               yield TurnStep::Emit( RuntimeEvent::Failed {
                                   kind: RunFailureKind::ContextSource,
                                   message,
                               });
                               yield TurnStep::Stop; return;
                           }
                       }
                   }
                   let prefix_key = match summarizer {
                       Some(key) => key,
                       None => plan::PromptPrefixKey {
                           tools: allow_tools.then_some(catalog::StaticFilter {
                               spawn_agent: spawner.is_some(),
                               search_history: history.is_some(),
                               update_goal: goal.is_some(),
                               read_tool_result: spills.is_some(),
                               load_skill: allow_guidance,
                               read_only,
                               unattended,
                           }),
                           guidance: allow_guidance,
                           subagent,
                           mode,
                           snapshots,
                       },
                   };
                   let static_filter = prefix_key.tools;
                   let allow_tools = static_filter.is_some();
                   let base_specs: Arc<[ToolSpec]> = match &static_filter {
                       Some(filter) => catalog.base_specs(filter),
                       None => Arc::from([]),
                   };
                   let mut pins = catalog::PinSet::default();
                   if allow_tools && catalog.exposure() == catalog::Exposure::Progressive {
                       recover_pins(&messages, &catalog, &mut pins, static_filter.as_ref().expect("tools have a filter"));
                   }
                   let tool_specs: Arc<[ToolSpec]> = if pins.is_empty() {
                       Arc::clone(&base_specs)
                   } else {
                       catalog.specs_with_pins(&base_specs, &pins, static_filter.as_ref().expect("pins require tools"))
                   };
                   let prompt_prefix = plan.prompt_prefix(prefix_key, &base_specs);
                   let (system, system_prompt_hash) = {
                       let mut suffix = String::new();
                       if let Some(guidance) = &selected_guidance {
                           guidance.append_to_prompt(&mut suffix);
                       }
                       suffix.push_str(&context_blocks);
                       if let Some(output) = &output {
                           suffix.push_str("\n\n");
                           suffix.push_str(output::OUTPUT_CONTRACT_SYSTEM_NOTICE);
                           suffix.push_str(output.schema_json());
                           suffix.push_str("\n```\n");
                       }
                       prompt_prefix.complete(&suffix)
                   };
                   let tool_schema = catalog.schema_measurement(&tool_specs);
                   let prompt_identity = Some(Arc::new(RunPromptIdentity {
                           version: AGENT_PROMPT_VERSION,
                           instruction_hash: workspace_instructions.hash(),
                           system_prompt_hash: Some(system_prompt_hash),
                           tool_schema_hash: Some(tool_schema.hash),
                           selected_guidance: selected_guidance
                               .as_ref()
                               .map(|guidance| Box::new(guidance.identity())),
                           catalog_digest: Some(catalog.digest()),
                           exposure: Some(match catalog.exposure() {
                               catalog::Exposure::Full => qq_protocol::ToolExposure::Full,
                               catalog::Exposure::Progressive => qq_protocol::ToolExposure::Progressive,
                           }),
                           context_sources: context_records,
                       }));
                   if let Some(goal) = &goal {
                       let notice_index = messages.len().saturating_sub(1);
                       Arc::make_mut(&mut messages).insert(notice_index, Message::user(goal.initial_notice()));
                   }
                   let reducible_messages = messages.len().saturating_sub(1 + usize::from(goal.is_some()));
                   let reducible_message_bytes = measure_messages(&messages[..reducible_messages]);
                   let irreducible_message_bytes =
                       measure_messages(&messages[reducible_messages..]);
                   let compatible_request: Option<(u64, u64, u64, u64)> = None;
                   let call_effects = HashMap::<String, catalog::EffectClass>::new();

                   let slice_tool_calls = 0_usize;
                   let stall = runtime::StallScope::new(if stall_exempt || !allow_tools || summarizer.is_some() {
                       runtime::StallPolicy::Exempt
                   } else if subagent.is_some() {
                       runtime::StallPolicy::Subagent
                   } else {
                       runtime::StallPolicy::Root
                   });
                   let output_continuations = 0_u16;
                   let turn_retries = 0_u16;
                   let audit_triggers = runtime::AuditTriggers::default();
                   let audit_actions: Vec<runtime::AuditedAction> = Vec::new();
                   let audit_revisions = 0_u16;
                   let output_repairs = 0_u8;
                   let audit_prompt = messages
                       .iter()
                       .rev()
                       .find(|message| message.role() == Role::User)
                       .and_then(|message| {
                           message.content().iter().find_map(|block| match block {
                               ContentBlock::Text { text } => Some(text.clone()),
                               _ => None,
                           })
                       })
                       .unwrap_or_default();
                   let model_text_bytes = 0_usize;
                   let continuing_slice = false;
                   let placed_report: Option<runtime::TurnNotice> = None;
                   let pending_notice: Option<runtime::TurnNotice> = None;
                   let live_turns: Vec<u32> = Vec::new();
                   let provider_overflowed = false;
                   let summarizer_rejected_turns = 0_u8;
                   let wait_delivered_for: Option<u32> = None;
                   let run = RunScope {
                       handled_interrupt,
                       max_output_tokens,
                       budget,
                       audit_triggers,
                       audit_actions,
                       audit_revisions,
                       output_repairs,
                       goal_handoff_requested: false,
                       goal_final_slot_announced: false,
                       live_turns,
                       summarizer_rejected_turns,
                       inherited_compaction_attempted: false,
                       summary_fold: runtime::SummaryFold::new(messages.len().saturating_sub(1)),
                       summary_fold_text: String::new(),
                       stall,
                       loop_guard: LoopGuard::default(),
                       guard_eligible: Vec::new(),
                       slices: 1,
                       pins,
                       tool_specs,
                       tool_schema,
                       prompt_identity,
                       call_effects,
                       argument_errors: Vec::new(),
                   };
                   let window = WindowScope {
                       reducible_message_bytes,
                       irreducible_message_bytes,
                       compatible_request,
                       model_text_bytes,
                   };
                   let slice = SliceScope {
                       slice_tool_calls,
                       continuing_slice,
                       placed_report,
                       pending_notice,
                       pending_goal_notices: Vec::new(),
                       pending_deliveries: 0,
                       pending_reserved_goal_notices: 0,
                   };
                   let recovery = TurnScope {
                       turn_retries,
                       output_continuations,
                       empty_output_retries,
                       reasoning_only_truncations,
                       provider_overflowed,
                       wait_delivered_for,
                   };

        yield TurnStep::Ready(RunState {
           messages, steering, run, window, slice, recovery,
           env: RunEnvironment { plan, cancelled, gate, file_state, spawner, history, goal_spend_tracking: goal_spend_tracking || goal.is_some(), goal_paid, goal, spills, compactor, audit_hook, tool_tasks, output, summarizer, inherited_effects, limits, deadline, system, system_prompt_hash, base_specs, static_filter, reducible_messages, audit_prompt, allow_tools, model_max_output_tokens, output_ceiling, ceiling_by_policy, cache_key },
        });
               }
    }
}
impl RunState {
    fn prepare_turn(
        &mut self,
        turn_ordinal: u32,
    ) -> impl futures_util::Stream<Item = TurnStep<PreparedTurn>> + Send + '_ {
        stream! {
                    let Self { env, messages, run, window, slice, recovery, .. } = self;
                    let RunEnvironment { spawner, system, system_prompt_hash, allow_tools, goal, cancelled, deadline, .. } = env;
                    let allow_tools = *allow_tools;
                    let system_prompt_hash = *system_prompt_hash;

                        if let Some(goal) = goal {
                            let refreshed = tokio::select! {
                                biased;
                                () = cancelled.cancelled() => { yield TurnStep::Stop; return; },
                                () = runtime::RunDeadline::wait(*deadline) => {
                                    yield TurnStep::Emit(RuntimeEvent::BudgetExhausted { exhaustion: deadline.expect("only a finite deadline wakes").exhaustion() });
                                    yield TurnStep::Stop; return;
                                },
                                refreshed = goal.refresh_notice() => refreshed,
                            };
                            match refreshed {
                                Ok(Some(text)) => {
                                    let notice = Message::user(text.clone());
                                    window.irreducible_message_bytes = window.irreducible_message_bytes.saturating_add(measure_message(&notice));
                                    Arc::make_mut(messages).push(notice);
                                    slice.pending_goal_notices.push(runtime::OrderedTurnNotice { text, preceding_deliveries: slice.pending_deliveries });
                                }
                                Ok(None) => {},
                                Err(error) => {
                                    yield TurnStep::Emit(RuntimeEvent::Failed { kind: RunFailureKind::Server, message: format!("refresh goal notice: {error}") });
                                    yield TurnStep::Stop; return;
                                }
                            }
                        }

                        if !run.goal_final_slot_announced && goal.as_ref().is_some_and(sessions::GoalRunInput::final_slot) {
                            run.goal_final_slot_announced = true;
                            let text = sessions::GoalRunInput::FINAL_RUN_NOTICE.to_owned();
                            let notice = Message::user(text.clone());
                            window.irreducible_message_bytes = window.irreducible_message_bytes.saturating_add(measure_message(&notice));
                            Arc::make_mut(messages).push(notice);
                            slice.pending_goal_notices.push(runtime::OrderedTurnNotice { text, preceding_deliveries: slice.pending_deliveries });
                        }
                        let delivered_by_wait = std::mem::take(&mut recovery.wait_delivered_for) == Some(turn_ordinal);
                        if let Some(spawner) = &spawner
                            && !delivered_by_wait
                            && spawner.outstanding_detached() > 0
                        {
                            match deliver_children(
                                spawner,
                                Boundary { turn_ordinal, reports: runtime::ReportDelivery::Always },
                                Arc::make_mut(messages),
                                &mut window.irreducible_message_bytes,
                                &mut run.budget,
                                &mut run.stall,
                            )
                            .await {
                                Ok(delivered) => { slice.pending_deliveries += delivered.answers + delivered.reports; },
                                Err(error) => {
                                    yield TurnStep::Emit( RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() });
                                    yield TurnStep::Stop; return;
                                }
                            }
                        }
                        if let Some(cap) = goal.as_ref().and_then(sessions::GoalRunInput::final_slot_turn_cap) {
                            run.budget.constrain_model_turns(cap);
                        }
                        let decision=if goal.as_ref().is_some_and(sessions::GoalRunInput::audit) {
                            run.budget.before_goal_audit(tokio::time::Instant::now())
                        } else {run.budget.before_turn(
                            tokio::time::Instant::now(),
                            if allow_tools { MAX_TOOL_CALLS_PER_TURN } else { 0 },
                        )};
                        let budget_final_turn = match decision {
                            BudgetDecision::Continue => false,
                            BudgetDecision::FinalResponse(_) => true,
                            BudgetDecision::Exhausted(exhaustion) => {
                                yield TurnStep::Emit( RuntimeEvent::BudgetExhausted { exhaustion });
                                yield TurnStep::Stop; return;
                            }
                        };
                        if budget_final_turn && !run.goal_handoff_requested
                            && goal.as_ref().is_some_and(sessions::GoalRunInput::final_slot)
                        {
                            run.goal_handoff_requested = true;
                            run.goal_final_slot_announced = true;
                            let text = sessions::GoalRunInput::HANDOFF_NOTICE.to_owned();
                            let notice = Message::user(text.clone());
                            window.irreducible_message_bytes = window.irreducible_message_bytes.saturating_add(measure_message(&notice));
                            Arc::make_mut(messages).push(notice);
                            slice.pending_goal_notices.push(runtime::OrderedTurnNotice { text, preceding_deliveries: slice.pending_deliveries });
                        }
                        let slice_checkpoint = !budget_final_turn && !run.goal_handoff_requested
                            && slice.slice_tool_calls
                                .saturating_add(MAX_TOOL_CALLS_PER_TURN)
                                > MAX_TOOL_CALLS_PER_SLICE;
                        let report_due = match (budget_final_turn || run.goal_handoff_requested, slice.placed_report) {
                            (true, _) => runtime::ReportDue::None,
                            (false, Some(runtime::TurnNotice::FinalAnswer)) => runtime::ReportDue::FinalAnswer,
                            (false, Some(runtime::TurnNotice::Report | runtime::TurnNotice::StallReport | runtime::TurnNotice::RootStallReport)) => {
                                runtime::ReportDue::Report
                            }
                            (false, Some(runtime::TurnNotice::Continuation) | None) => {
                                run.stall.due(slice_checkpoint)
                            }
                        };
                        let final_answer_turn = report_due == runtime::ReportDue::FinalAnswer;
                        let checkpoint_turn = report_due == runtime::ReportDue::Report;
                        let slice_report = match slice.placed_report {
                            Some(placed) => placed == runtime::TurnNotice::Report,
                            None => slice_checkpoint,
                        };
                        let continuation_turn = std::mem::take(&mut slice.continuing_slice);
                        debug_assert!(!(checkpoint_turn && continuation_turn));
                        let notice = if (checkpoint_turn || final_answer_turn) && slice.placed_report.is_none() {
                            let notice = if final_answer_turn {
                                runtime::TurnNotice::FinalAnswer
                            } else if slice_report {
                                runtime::TurnNotice::Report
                            } else {
                                run.stall.stall_report_notice()
                            };
                            slice.placed_report = Some(notice);
                            Some(notice)
                        } else if continuation_turn && !budget_final_turn && !final_answer_turn && !run.goal_handoff_requested {
                            Some(runtime::TurnNotice::Continuation)
                        } else {
                            None
                        };
                        if let Some(notice) = notice {
                            let message = Message::user(notice.text());
                            window.irreducible_message_bytes =
                                window.irreducible_message_bytes.saturating_add(measure_message(&message));
                            Arc::make_mut(messages).push(message);
                            if goal.is_some() {
                                slice.pending_goal_notices.push(runtime::OrderedTurnNotice {
                                    text: notice.text().to_owned(), preceding_deliveries: slice.pending_deliveries,
                                });
                            }
                            slice.pending_notice = Some(notice);
                        }
                        if budget_final_turn {
                            let text = BUDGET_FINAL_RESPONSE_NOTICE.to_owned();
                            let message = Message::user(text.clone());
                            window.irreducible_message_bytes =
                                window.irreducible_message_bytes.saturating_add(measure_message(&message));
                            Arc::make_mut(messages).push(message);
                            slice.pending_goal_notices.push(runtime::OrderedTurnNotice {
                                text, preceding_deliveries: slice.pending_deliveries,
                            });
                        }
                        let request_system: Arc<str> = Arc::clone(system);
                        let request_has_tools = allow_tools;
                        let request_system_hash = system_prompt_hash;
                        let system_bytes = u64::try_from(request_system.len())
                            .unwrap_or(u64::MAX)
                            .saturating_add(CONTEXT_BLOCK_FRAMING_BYTES);
                        let tool_schema_bytes = if request_has_tools {
                            run.tool_schema.bytes
                        } else {
                            0
                        };

        yield TurnStep::Ready(PreparedTurn { turn_ordinal, budget_final_turn, final_answer_turn, checkpoint_turn, slice_report, request_system, request_has_tools, request_system_hash, system_bytes, tool_schema_bytes });
                }
    }
    fn compact_turn(
        &mut self,
        prepared: PreparedTurn,
    ) -> impl futures_util::Stream<Item = TurnStep<CompactedTurn>> + Send + '_ {
        stream! {
                    let Self { env, messages, run, window, slice, recovery, .. } = self;
                    let RunEnvironment { plan, compactor, goal, system, reducible_messages, model_max_output_tokens, inherited_effects, cancelled, deadline, .. } = env;
                    let reducible_messages = *reducible_messages;
                    let model_max_output_tokens = *model_max_output_tokens;
                    let deadline = *deadline;
                    let turn_recovery = plan.runtime.turn_recovery;
                    let PreparedTurn { turn_ordinal, budget_final_turn, final_answer_turn, checkpoint_turn, slice_report, request_system, request_has_tools, request_system_hash, system_bytes, tool_schema_bytes } = prepared;
                        let estimate_input_tokens = |chain: Option<(u64, u64, u64, u64)>, reducible: u64, irreducible: u64| -> u64 {
                            let message_bytes = reducible.saturating_add(irreducible);
                            match chain {
                                Some((previous_system, previous_tools, previous_messages, measured)) => {
                                    let ratio = sessions::context::calibrated_bytes_per_token(
                                        measured,
                                        previous_system
                                            .saturating_add(previous_tools)
                                            .saturating_add(previous_messages),
                                    );
                                    let tokens = sessions::context::adjust_measured_tokens_at(
                                        measured,
                                        previous_system,
                                        system_bytes,
                                        ratio,
                                    );
                                    let tokens = sessions::context::adjust_measured_tokens_at(
                                        tokens,
                                        previous_tools,
                                        tool_schema_bytes,
                                        ratio,
                                    );
                                    sessions::context::adjust_measured_tokens_at(
                                        tokens,
                                        previous_messages,
                                        message_bytes,
                                        ratio,
                                    )
                                }
                                None => sessions::context::estimate_tokens(
                                    system_bytes
                                        .saturating_add(tool_schema_bytes)
                                        .saturating_add(message_bytes),
                                ),
                            }
                        };
                        let over_window = |estimated_input_tokens: u64| -> bool {
                            turn_ordinal > 1
                                && plan.runtime.context_window.is_some_and(|window| {
                                    estimated_input_tokens
                                        .saturating_add(u64::from(run.max_output_tokens))
                                        > u64::from(window)
                                })
                        };
                        let would_overflow = recovery.provider_overflowed
                            || over_window(estimate_input_tokens(
                                window.compatible_request,
                                window.reducible_message_bytes,
                                window.irreducible_message_bytes,
                            ));
                        if would_overflow && {
                            let mut effects = HashMap::new();
                            let mut inherited = 0_usize;
                            for (message_index, message) in messages.iter().enumerate() {
                                for (block_index, block) in message.content().iter().enumerate() {
                                    let ContentBlock::ToolResult { call_id, .. } = block else {
                                        continue;
                                    };
                                    let effect = if message_index < reducible_messages {
                                        inherited += 1;
                                        inherited_effects
                                            .get(inherited - 1)
                                            .copied()
                                            .unwrap_or_else(|| run.call_effects.get(call_id).copied())
                                    } else {
                                        run.call_effects.get(call_id).copied()
                                    };
                                    if let Some(effect) = effect {
                                        effects.insert((message_index, block_index), effect);
                                    }
                                }
                            }
                            sessions::prune_stale_tool_results(
                                Arc::make_mut(messages).as_mut_slice(),
                                &effects,
                            )
                        } {
                            window.reducible_message_bytes = measure_messages(&messages[..reducible_messages]);
                            window.irreducible_message_bytes = measure_messages(&messages[reducible_messages..]);
                            yield TurnStep::Emit(RuntimeEvent::ContextPruned { turn_ordinal });
                        }
                        let provider_rejected_window = recovery.provider_overflowed;
                        let still_overflows = std::mem::take(&mut recovery.provider_overflowed)
                            || over_window(estimate_input_tokens(
                                window.compatible_request,
                                window.reducible_message_bytes,
                                window.irreducible_message_bytes,
                            ));
                        if still_overflows && let Some(compactor) = compactor.as_ref() {
                            let run_start = reducible_messages.saturating_add(1 + usize::from(goal.is_some()));
                            let inherited_prefix = provider_rejected_window && reducible_messages > 0 && !run.inherited_compaction_attempted
                                && sessions::in_run_compaction_boundary(&messages[run_start..], sessions::CONTEXT_PRUNE_KEEP_TURNS).is_none();
                            let boundary = if inherited_prefix {
                                Some((0, 0))
                            } else {
                                sessions::in_run_compaction_boundary(
                                    &messages[run_start..], sessions::CONTEXT_PRUNE_KEEP_TURNS,
                                )
                            };
                            if let Some((replace_through, replaced_turns)) = boundary {
                                let replaced_turns = usize::try_from(replaced_turns).expect("turn count fits usize");
                                let live_assistant_messages = messages[run_start..]
                                    .iter()
                                    .filter(|message| message.role() == Role::Assistant)
                                    .count();
                                let turn_cutoff = if inherited_prefix { 0 } else { match run.live_turns.get(replaced_turns.wrapping_sub(1)) {
                                    Some(ordinal) if run.live_turns.len() == live_assistant_messages => *ordinal,
                                    Some(_) | None => {
                                        yield TurnStep::Emit(RuntimeEvent::Failed {
                                            kind: RunFailureKind::Server,
                                            message: format!(
                                                "in-run compaction found {} recorded turns for {live_assistant_messages} live assistant messages",
                                                run.live_turns.len()
                                            ),
                                        });
                                        yield TurnStep::Stop; return;
                                    }
                                }};
                                if inherited_prefix { run.inherited_compaction_attempted = true; }
                                let cut = run_start + replace_through;
                                let summarizer_fits = !provider_rejected_window
                                    && plan.runtime.context_window.is_none_or(|context_window| {
                                        estimate_input_tokens(
                                            window.compatible_request,
                                            window.reducible_message_bytes,
                                            measure_messages(&messages[reducible_messages..cut]),
                                        )
                                        .saturating_add(u64::from(
                                            sessions::context::summarizer_output_tokens(
                                                model_max_output_tokens,
                                                Some(context_window),
                                            ),
                                        ))
                                            <= u64::from(context_window)
                                    });
                                let transcript = if inherited_prefix {
                                    &messages[..reducible_messages]
                                } else if summarizer_fits {
                                    &messages[..cut]
                                } else {
                                    &messages[reducible_messages..cut]
                                };
                                let goal_notice = (!inherited_prefix).then(|| goal.as_ref().and_then(|goal| goal.post_summary_notice(run.goal_handoff_requested)).map(Arc::<str>::from)).flatten();
                                let post_summary_notice = goal_notice.as_ref().map(|text| Message::user(text.as_ref()));
                                let retained_bytes = system_bytes
                                    .saturating_add(tool_schema_bytes)
                                    .saturating_add(window.reducible_message_bytes)
                                    .saturating_add(measure_messages(&messages[reducible_messages..run_start]))
                                    .saturating_add(measure_messages(&messages[cut..]))
                                    .saturating_add(post_summary_notice.as_ref().map_or(0, measure_message));
                                let retained_bytes = if inherited_prefix {
                                    system_bytes.saturating_add(tool_schema_bytes)
                                        .saturating_add(measure_messages(&messages[reducible_messages..]))
                                } else { retained_bytes };
                                yield TurnStep::Emit(RuntimeEvent::ActivityChanged {
                                    activity: RunActivity::Compacting,
                                });
                                let mut summarizer_retries: u16 = 0;
                                let compacted = loop {
                                    let allowance = match run.budget.child_budget(tokio::time::Instant::now()) {
                                        Ok(allowance) => allowance,
                                        Err(kind) => {
                                            yield TurnStep::Emit(RuntimeEvent::BudgetExhausted {
                                                exhaustion: run.budget.exhaustion(kind, false, tokio::time::Instant::now()),
                                            });
                                            yield TurnStep::Stop; return;
                                        }
                                    };
                                    let attempt = compactor
                                        .compact(runtime::InRunCompactionRequest {
                                            transcript: transcript.to_vec(),
                                            turn_cutoff,
                                            inherited_prefix,
                                            system: Arc::clone(system),
                                            tools: Arc::clone(&run.tool_specs),
                                            retained_bytes,
                                            goal_notice: goal_notice.clone(),
                                            budget: allowance,
                                        })
                                        .await;
                                    let (kind, message, retry_after) = match attempt {
                                        Err(runtime::InRunCompactionError::SummarizerUnavailable { kind, message, retry_after, usage, estimated_cost_usd_nanos }) => {
                                            run.budget.charge_child(usage, estimated_cost_usd_nanos);
                                            if let Err(limit) = run.budget.remaining(tokio::time::Instant::now()) {
                                                yield TurnStep::Emit(RuntimeEvent::BudgetExhausted {
                                                    exhaustion: run.budget.exhaustion(limit, false, tokio::time::Instant::now()),
                                                });
                                                yield TurnStep::Stop; return;
                                            }
                                            (kind, message, retry_after)
                                        }
                                        Err(runtime::InRunCompactionError::BudgetExhausted { kind, usage, estimated_cost_usd_nanos }) => {
                                            run.budget.charge_child(usage, estimated_cost_usd_nanos);
                                            yield TurnStep::Emit(RuntimeEvent::BudgetExhausted {
                                                exhaustion: run.budget.exhaustion(kind, false, tokio::time::Instant::now()),
                                            });
                                            yield TurnStep::Stop; return;
                                        }
                                        settled => break settled,
                                    };
                                    if summarizer_retries >= MAX_TURN_RETRIES {
                                        yield TurnStep::Emit(RuntimeEvent::Paused {
                                            pause: Box::new(qq_protocol::RunPause::ProviderRetry {
                                                kind,
                                                message: format!("in-run compaction summarizer: {message}"),
                                                turn_ordinal,
                                                attempts: summarizer_retries,
                                            }),
                                        });
                                        yield TurnStep::Stop; return;
                                    }
                                    summarizer_retries += 1;
                                    let delay = turn_recovery.delay_after(summarizer_retries,retry_after);
                                    yield TurnStep::Emit(RuntimeEvent::TurnRetrying {
                                        turn_ordinal,
                                        attempt: summarizer_retries,
                                        delay,
                                        kind,
                                        message: format!("in-run compaction summarizer: {message}"),
                                    });
                                    let sleep = tokio::time::sleep(delay);
                                    tokio::pin!(sleep);
                                    tokio::select! {
                                        biased;
                                        () = cancelled.cancelled() => { yield TurnStep::Stop; return; },
                                        () = runtime::RunDeadline::wait(deadline) => {
                                            yield TurnStep::Emit(RuntimeEvent::BudgetExhausted {
                                                exhaustion: deadline.expect("only a finite deadline wakes").exhaustion(),
                                            });
                                            yield TurnStep::Stop; return;
                                        }
                                        () = &mut sleep => {}
                                    }
                                };
                                match compacted
                                {
                                    Ok(summary) => {
                                        run.budget.charge_child(summary.usage, summary.estimated_cost_usd_nanos);
                                        let summary = Message::user(format!(
                                            "{}\n\n{}",
                                            if inherited_prefix { sessions::COMPACTION_SUMMARY_PREAMBLE } else { sessions::IN_RUN_COMPACTION_PREAMBLE }, summary.summary
                                        ));
                                        let live = Arc::make_mut(messages);
                                        if inherited_prefix {
                                            live.splice(..reducible_messages, [summary]);
                                            env.reducible_messages = 1;
                                            inherited_effects.clear();
                                            window.reducible_message_bytes = measure_messages(&messages[..1]);
                                        } else { match post_summary_notice {
                                            Some(notice) => {
                                                live.splice(run_start..run_start + replace_through, [summary, notice]);
                                            }
                                            None => {
                                                live.splice(run_start..run_start + replace_through, [summary]);
                                            }
                                        }}
                                        run.live_turns.drain(..replaced_turns);
                                        window.compatible_request = None;
                                        window.model_text_bytes = 0;
                                        window.irreducible_message_bytes = measure_messages(&messages[if inherited_prefix { 1 } else { reducible_messages }..]);
                                        slice.pending_reserved_goal_notices = slice.pending_goal_notices.len();
                                        yield TurnStep::Emit( RuntimeEvent::InRunCompacted { turn_ordinal, turn_cutoff });
                                        if let Err(kind) = run.budget.remaining(tokio::time::Instant::now()) {
                                            yield TurnStep::Emit(RuntimeEvent::BudgetExhausted {
                                                exhaustion: run.budget.exhaustion(kind, false, tokio::time::Instant::now()),
                                            });
                                            yield TurnStep::Stop; return;
                                        }
                                    }
                                    Err(error) => {
                                        yield TurnStep::Emit(match error {
                                            runtime::InRunCompactionError::Persistence(_) => RuntimeEvent::Failed {
                                                kind: RunFailureKind::Server,
                                                message: format!("in-run compaction failed: {error}"),
                                            },
                                            runtime::InRunCompactionError::SummarizerFailed(_)
                                            | runtime::InRunCompactionError::SummarizerUnavailable { .. }
                                            | runtime::InRunCompactionError::BudgetExhausted { .. }
                                            | runtime::InRunCompactionError::Unavailable(_) => RuntimeEvent::Failed {
                                                kind: if inherited_prefix { RunFailureKind::ProviderContextExceeded } else { RunFailureKind::Policy },
                                                message: format!(
                                                    "automatic context recovery could not produce a smaller usable request: {error}; all session history is preserved"
                                                ),
                                            },
                                        });
                                        yield TurnStep::Stop; return;
                                    }
                                }
                            }
                        }
                        let message_bytes = window.reducible_message_bytes.saturating_add(window.irreducible_message_bytes);
                        let compatible_input_tokens = window.compatible_request.map(|chain| {
                            estimate_input_tokens(Some(chain), window.reducible_message_bytes, window.irreducible_message_bytes)
                        });

        yield TurnStep::Ready(CompactedTurn { compatible_input_tokens, prepared: PreparedTurn { turn_ordinal, budget_final_turn, final_answer_turn, checkpoint_turn, slice_report, request_system, request_has_tools, request_system_hash, system_bytes, tool_schema_bytes }, message_bytes });
                }
    }
    fn stream_turn(
        &mut self,
        compacted: CompactedTurn,
    ) -> impl futures_util::Stream<Item = TurnStep<StreamedTurn>> + Send + '_ {
        stream! {
                    let Self { env, messages, steering, run, window, slice, recovery, .. } = self;
                    let RunEnvironment { plan, compactor, summarizer, reducible_messages, goal_paid, goal, cache_key, .. } = env;
                    let reducible_messages = *reducible_messages;
                    let provider = &plan.runtime.provider;
                    let model = &plan.runtime.model;
                    let reasoning_effort = plan.runtime.reasoning_effort;

        let CompactedTurn { prepared, message_bytes, compatible_input_tokens } = compacted;
                    let PreparedTurn { turn_ordinal, budget_final_turn, final_answer_turn, checkpoint_turn, slice_report, request_system, request_has_tools, request_system_hash, system_bytes, tool_schema_bytes } = prepared;
                        if turn_ordinal == 1 {
                            slice.pending_reserved_goal_notices = slice.pending_goal_notices.len();
                        }
                        let input_tokens = if goal_paid.is_some() {
                            window.reducible_message_bytes.saturating_add(window.irreducible_message_bytes)
                                .saturating_add(system_bytes).saturating_add(tool_schema_bytes)
                        } else { 0 };
                        let mut request_cap = run.max_output_tokens;
                        if let Some(paid) = goal_paid.as_ref() {
                            let caller = if turn_ordinal == 1 {
                                runtime::ChildBudget::default()
                            } else {
                                match run.budget.child_budget(tokio::time::Instant::now()) {
                                    Ok(caller) => caller,
                                    Err(kind) => {
                                        yield TurnStep::Emit(RuntimeEvent::BudgetExhausted {exhaustion:run.budget.exhaustion(kind,false,tokio::time::Instant::now())});
                                        yield TurnStep::Stop; return;
                                    }
                                }
                            };
                            match paid.preview_request(request_cap, input_tokens, caller).await {
                                Ok(sessions::PaidRequestAdmission::Reprepare { max_output_tokens }) => request_cap = max_output_tokens,
                                Ok(sessions::PaidRequestAdmission::Cancelled) => {
                                    yield TurnStep::Emit(RuntimeEvent::GoalPaidCancelled);
                                    yield TurnStep::Stop; return;
                                }
                                Ok(sessions::PaidRequestAdmission::Exhausted(exhaustion)) => {
                                    yield TurnStep::Emit(RuntimeEvent::BudgetExhausted { exhaustion });
                                    yield TurnStep::Stop; return;
                                }
                                Ok(sessions::PaidRequestAdmission::Allowed { .. }) => unreachable!("preview cannot admit provider work"),
                                Err(error) => {
                                    yield TurnStep::Emit(RuntimeEvent::Failed { kind: RunFailureKind::Server, message: format!("failed to preview goal request allowance: {error}") });
                                    yield TurnStep::Stop; return;
                                }
                            }
                        }
                        loop {
                        yield TurnStep::Emit( RuntimeEvent::Prepared {
                            turn_ordinal,
                            identity: run.prompt_identity.take(),
                            static_prefix: PreparedStaticPrefix::new(
                                request_system_hash,
                                request_has_tools.then_some(run.tool_schema.hash),
                            ),
                            weight: PreparedRequestWeight {
                                max_output_tokens: request_cap,
                                system_bytes,
                                tool_schema_bytes,
                                reducible_message_bytes: window.reducible_message_bytes,
                                irreducible_message_bytes: window.irreducible_message_bytes,
                                compatible_input_tokens,
                            },
                        });
                            let Some(paid) = goal_paid.as_ref() else { break; };
                            let caller=match run.budget.child_budget(tokio::time::Instant::now()) {
                                Ok(caller)=>caller,
                                Err(kind)=>{
                                    yield TurnStep::Emit(RuntimeEvent::BudgetExhausted {exhaustion:run.budget.exhaustion(kind,false,tokio::time::Instant::now())});
                                    yield TurnStep::Stop; return;
                                }
                            };
                            match paid.admit_request(request_cap, input_tokens, caller).await {
                                Ok(sessions::PaidRequestAdmission::Allowed { cursor }) => {
                                    yield TurnStep::Emit(RuntimeEvent::GoalRequestAdmitted { cursor });
                                    break;
                                }
                                Ok(sessions::PaidRequestAdmission::Reprepare { max_output_tokens }) => request_cap = max_output_tokens,
                                Ok(sessions::PaidRequestAdmission::Cancelled) => {
                                    yield TurnStep::Emit(RuntimeEvent::GoalPaidCancelled);
                                    yield TurnStep::Stop; return;
                                }
                                Ok(sessions::PaidRequestAdmission::Exhausted(exhaustion)) => {
                                    yield TurnStep::Emit(RuntimeEvent::BudgetExhausted { exhaustion });
                                    yield TurnStep::Stop; return;
                                }
                                Err(error) => {
                                    yield TurnStep::Emit(RuntimeEvent::Failed { kind: RunFailureKind::Server, message: format!("failed to admit goal provider request: {error}") });
                                    yield TurnStep::Stop; return;
                                }
                            }
                        }
                        let request = ModelRequest::new(
                            Arc::clone(model),
                            Arc::clone(messages),
                            request_cap,
                        );
                        let request = match cache_key {
                            Some(key) => request.with_cache_key(Arc::clone(key)),
                            None => request,
                        };
                        let request = match reasoning_effort {
                            Some(effort) => request.with_reasoning_effort(effort),
                            None => request,
                        };
                        let request = if request_has_tools {
                            let request = request
                                .with_tools(Arc::clone(&run.tool_specs))
                                .with_system(Arc::clone(&request_system));
                            if budget_final_turn || final_answer_turn {
                                request.with_tool_choice(qq_provider::ToolChoice::None)
                            } else {
                                request
                            }
                        } else {
                            request.with_system(Arc::clone(&request_system))
                        };
                        let request = if run.goal_handoff_requested || goal.as_ref().is_some_and(sessions::GoalRunInput::audit) {
                            request.with_tool_choice(qq_provider::ToolChoice::None)
                        } else {
                            request
                        };
                        let activity = RunActivity::WaitingForProvider;
                        if goal_paid.is_none() {
                            yield TurnStep::Emit(RuntimeEvent::ActivityChanged { activity });
                        }
                        let mut provider_events = provider.stream(request);

                        let mut provider_span = Some(tracing::info_span!(target: "qq::diagnostic", "provider"));
                    let mut attempt = AttemptScope {
                        rejected_before_work: false,
                        activity,
                        pending_calls: Vec::<PendingToolCall>::new(),
                        calls_by_provider_id: HashMap::<String, usize>::new(),
                        replay: None,
                        blocks: Vec::<TurnBlock>::new(),
                        terminal_usage: None,
                        completed: false,
                        reasoning_bytes: 0_usize,
                        reasoning_observed: false,
                        open_reasoning: None,
                        interrupted_turn: false,
                        truncated_turn: false,
                        truncation_reason: qq_provider::IncompleteReason::OutputTokens,
                        streamed_visible_output: false,
                        turn_fault: None,
                        retry_after: None,
                    };
                        loop {
                            let interrupt = async {
                                match steering {
                                    Some(steering) => loop {
                                        if *steering.interrupts.borrow() > run.handled_interrupt {
                                            break;
                                        }
                                        if steering.interrupts.changed().await.is_err() {
                                            std::future::pending::<()>().await;
                                        }
                                    },
                                    None => std::future::pending().await,
                                }
                            };
                            let event = tokio::select! {
                                biased;
                                () = interrupt => StreamStep::Interrupted,
                                event = provider_events.next().instrument(provider_span.as_ref().expect("provider attempt is active").clone()) => StreamStep::Event(event),
                            };
                            if matches!(&event, StreamStep::Interrupted | StreamStep::Event(None | Some(Err(_) | Ok(ProviderEvent::Completed { .. } | ProviderEvent::Incomplete { .. })))) {
                                provider_span.take();
                            }
                            let event = match event {
                                StreamStep::Interrupted => {
                                    attempt.interrupted_turn = true;
                                    attempt.completed = true;
                                    break;
                                }
                                StreamStep::Event(Some(event)) => event,
                                StreamStep::Event(None) => break,
                            };
                            match event {
                                Ok(ProviderEvent::Replay { data }) => {
                                    if attempt.replay.is_some() || data.len() > 16 * 1024 * 1024 {
                                        yield TurnStep::Emit( RuntimeEvent::Failed { kind: RunFailureKind::ProviderProtocol, message: "invalid or oversized provider continuation".to_owned() });
                                        yield TurnStep::Stop; return;
                                    }
                                    attempt.replay = Some(data);
                                }
                                Ok(ProviderEvent::ReasoningStarted { kind }) => {
                                    attempt.reasoning_observed = true;
                                    if attempt.open_reasoning.is_some() {
                                        yield TurnStep::Emit( RuntimeEvent::Failed {
                                            kind: RunFailureKind::ProviderProtocol,
                                            message: "provider started a reasoning block before completing the previous block".to_owned(),
                                        });
                                        yield TurnStep::Stop; return;
                                    }
                                    attempt.open_reasoning = Some(kind);
                                    if attempt.activity != RunActivity::Reasoning {
                                        attempt.activity = RunActivity::Reasoning;
                                        yield TurnStep::Emit( RuntimeEvent::ActivityChanged { activity: attempt.activity });
                                    }
                                    yield TurnStep::Emit( RuntimeEvent::ReasoningStarted { kind });
                                }
                                Ok(ProviderEvent::ReasoningDelta { kind, text }) => {
                                    if attempt.open_reasoning != Some(kind) {
                                        yield TurnStep::Emit( RuntimeEvent::Failed {
                                            kind: RunFailureKind::ProviderProtocol,
                                            message: "provider streamed reasoning outside its matching block".to_owned(),
                                        });
                                        yield TurnStep::Stop; return;
                                    }
                                    if attempt.reasoning_bytes.saturating_add(text.len()) > MAX_RUN_REASONING_BYTES {
                                        yield TurnStep::Emit( RuntimeEvent::Failed {
                                            kind: RunFailureKind::Policy,
                                            message: "displayable reasoning exceeded the 1 MiB per-run limit".to_owned(),
                                        });
                                        yield TurnStep::Stop; return;
                                    }
                                    attempt.reasoning_bytes += text.len();
                                    if !text.is_empty() {
                                        yield TurnStep::Emit( RuntimeEvent::ReasoningDelta { kind, text });
                                    }
                                }
                                Ok(ProviderEvent::ReasoningCompleted { kind }) => {
                                    if attempt.open_reasoning != Some(kind) {
                                        yield TurnStep::Emit( RuntimeEvent::Failed {
                                            kind: RunFailureKind::ProviderProtocol,
                                            message: "provider completed an unknown reasoning block".to_owned(),
                                        });
                                        yield TurnStep::Stop; return;
                                    }
                                    attempt.open_reasoning = None;
                                    yield TurnStep::Emit( RuntimeEvent::ReasoningCompleted { kind });
                                }
                                Ok(ProviderEvent::OutputTextDelta { text }) => {
                                    if attempt.activity != RunActivity::GeneratingResponse {
                                        attempt.activity = RunActivity::GeneratingResponse;
                                        yield TurnStep::Emit( RuntimeEvent::ActivityChanged { activity: attempt.activity });
                                    }
                                    if window.model_text_bytes.saturating_add(text.len()) > MAX_RUN_MODEL_TEXT_BYTES {
                                        yield TurnStep::Emit( RuntimeEvent::Failed {
                                            kind: RunFailureKind::Policy,
                                            message: "model text exceeded the 16 MiB per-window limit".to_owned(),
                                        });
                                        yield TurnStep::Stop; return;
                                    }
                                    window.model_text_bytes += text.len();
                                    append_turn_text(&mut attempt.blocks, &text);
                                    yield TurnStep::Emit( RuntimeEvent::OutputTextDelta { text });
                                }
                                Ok(ProviderEvent::RefusalDelta { text }) => {
                                    if attempt.activity != RunActivity::GeneratingResponse {
                                        attempt.activity = RunActivity::GeneratingResponse;
                                        yield TurnStep::Emit( RuntimeEvent::ActivityChanged { activity: attempt.activity });
                                    }
                                    if window.model_text_bytes.saturating_add(text.len()) > MAX_RUN_MODEL_TEXT_BYTES {
                                        yield TurnStep::Emit( RuntimeEvent::Failed {
                                            kind: RunFailureKind::Policy,
                                            message: "model text exceeded the 16 MiB per-window limit".to_owned(),
                                        });
                                        yield TurnStep::Stop; return;
                                    }
                                    window.model_text_bytes += text.len();
                                    append_turn_text(&mut attempt.blocks, &text);
                                    yield TurnStep::Emit( RuntimeEvent::RefusalDelta { text });
                                }
                                Ok(ProviderEvent::ToolCallStarted { id, name }) => {
                                    if budget_final_turn && !run.goal_handoff_requested {
                                        let BudgetDecision::Exhausted(mut exhaustion) = run.budget
                                            .before_turn(tokio::time::Instant::now(), 0)
                                        else {
                                            unreachable!("a requested final response always settles the run")
                                        };
                                        exhaustion.final_response = false;
                                        yield TurnStep::Emit( RuntimeEvent::BudgetExhausted { exhaustion });
                                        yield TurnStep::Stop; return;
                                    }
                                    if !request_has_tools {
                                        yield TurnStep::Emit( RuntimeEvent::Failed {
                                            kind: RunFailureKind::ProviderProtocol,
                                            message: "provider requested a tool after the request declared no tools".to_owned(),
                                        });
                                        yield TurnStep::Stop; return;
                                    }
                                    if attempt.activity != RunActivity::PreparingToolCall {
                                        attempt.activity = RunActivity::PreparingToolCall;
                                        yield TurnStep::Emit( RuntimeEvent::ActivityChanged { activity: attempt.activity });
                                    }
                                    if id.is_empty() || id.len() > MAX_TOOL_CALL_ID_BYTES {
                                        yield TurnStep::Emit( RuntimeEvent::Failed {
                                            kind: RunFailureKind::ProviderProtocol,
                                            message: "provider tool call ID is empty or exceeds 1 KiB".to_owned(),
                                        });
                                        yield TurnStep::Stop; return;
                                    }
                                    if name.is_empty() || name.len() > MAX_TOOL_NAME_BYTES {
                                        yield TurnStep::Emit( RuntimeEvent::Failed {
                                            kind: RunFailureKind::ProviderProtocol,
                                            message: "provider tool name is empty or exceeds 128 bytes".to_owned(),
                                        });
                                        yield TurnStep::Stop; return;
                                    }
                                    if attempt.pending_calls.len() >= MAX_ADMITTED_TOOL_CALLS_PER_TURN {
                                        yield TurnStep::Emit( RuntimeEvent::Failed {
                                            kind: RunFailureKind::ProviderProtocol,
                                            message: format!("model requested more than {MAX_ADMITTED_TOOL_CALLS_PER_TURN} tools in one turn"),
                                        });
                                        yield TurnStep::Stop; return;
                                    }
                                    let over_cap = attempt.pending_calls.len() >= MAX_TOOL_CALLS_PER_TURN;
                                    let rejection = if summarizer.is_some() {
                                        Some(SUMMARIZER_TOOL_REJECTION.to_owned())
                                    } else if run.goal_handoff_requested {
                                        Some("not executed: the final goal handoff is tool-free".to_owned())
                                    } else if final_answer_turn {
                                        Some(SUBAGENT_FINAL_ANSWER_REJECTION.to_owned())
                                    } else if checkpoint_turn && slice_report {
                                        Some(SLICE_CHECKPOINT_REJECTION.to_owned())
                                    } else if checkpoint_turn {
                                        Some(STALL_REPORT_REJECTION.to_owned())
                                    } else if over_cap {
                                        Some(format!(
                                            "not executed: this turn requested more than \
                                             {MAX_TOOL_CALLS_PER_TURN} tool calls and only the first \
                                             {MAX_TOOL_CALLS_PER_TURN} ran; call this again next turn"
                                        ))
                                    } else {
                                        None
                                    };
                                    if attempt.calls_by_provider_id.contains_key(&id) {
                                        yield TurnStep::Emit( RuntimeEvent::Failed {
                                            kind: RunFailureKind::ProviderProtocol,
                                            message: format!("provider reused tool call ID {id:?} in one turn"),
                                        });
                                        yield TurnStep::Stop; return;
                                    }
                                    let index = attempt.pending_calls.len();
                                    attempt.calls_by_provider_id.insert(id.clone(), index);
                                    attempt.pending_calls.push(PendingToolCall {
                                        provider_call_id: id,
                                        name,
                                        arguments: String::new(),
                                        completed: false,
                                        rejection,
                                        oversized: false,
                                    });
                                    if !over_cap && !checkpoint_turn && !final_answer_turn {
                                        slice.slice_tool_calls += 1;
                                    }
                                    attempt.blocks.push(TurnBlock::ToolCall(index));
                                }
                                Ok(ProviderEvent::ToolCallArgumentsDelta { id, json }) => {
                                    let Some(index) = attempt.calls_by_provider_id.get(&id).copied() else {
                                        yield TurnStep::Emit( RuntimeEvent::Failed {
                                            kind: RunFailureKind::ProviderProtocol,
                                            message: format!("provider streamed arguments for unknown tool call {id:?}"),
                                        });
                                        yield TurnStep::Stop; return;
                                    };
                                    let call = &mut attempt.pending_calls[index];
                                    if call.completed {
                                        yield TurnStep::Emit( RuntimeEvent::Failed {
                                            kind: RunFailureKind::ProviderProtocol,
                                            message: format!("provider streamed arguments after completing tool call {id:?}"),
                                        });
                                        yield TurnStep::Stop; return;
                                    }
                                    if call.oversized {
                                        continue;
                                    }
                                    if call.arguments.len().saturating_add(json.len()) > MAX_TOOL_ARGUMENT_BYTES {
                                        call.oversized = true;
                                        call.arguments = String::new();
                                        call.rejection.get_or_insert_with(|| format!(
                                            "not executed: this call's arguments exceed the {} KiB limit; split the content across several smaller calls",
                                            MAX_TOOL_ARGUMENT_BYTES / 1024
                                        ));
                                        continue;
                                    }
                                    call.arguments.push_str(&json);
                                }
                                Ok(ProviderEvent::ToolCallCompleted { id }) => {
                                    let Some(index) = attempt.calls_by_provider_id.get(&id).copied() else {
                                        yield TurnStep::Emit( RuntimeEvent::Failed {
                                            kind: RunFailureKind::ProviderProtocol,
                                            message: format!("provider completed unknown tool call {id:?}"),
                                        });
                                        yield TurnStep::Stop; return;
                                    };
                                    let call = &mut attempt.pending_calls[index];
                                    if call.completed {
                                        yield TurnStep::Emit( RuntimeEvent::Failed {
                                            kind: RunFailureKind::ProviderProtocol,
                                            message: format!("provider completed tool call {id:?} twice"),
                                        });
                                        yield TurnStep::Stop; return;
                                    }
                                    let arguments = if call.arguments.trim().is_empty() {
                                        "{}"
                                    } else {
                                        &call.arguments
                                    };
                                    let parsed: serde_json::Value = match serde_json::from_str(arguments)
                                    {
                                        Ok(mut arguments) => {
                                            if call.name == "edit_file" {
                                                tools::normalize_edits(&mut arguments);
                                            }
                                            arguments
                                        }
                                        Err(error) => {
                                            if call.rejection.is_none() {
                                                call.rejection = Some(format!(
                                                    "tool call arguments were not valid JSON: {error}"
                                                ));
                                            }
                                            serde_json::Value::Object(serde_json::Map::new())
                                        }
                                    };
                                    call.arguments = serde_json::to_string(&parsed)
                                        .expect("a parsed JSON value must serialize");
                                    call.completed = true;
                                }
                                Ok(ProviderEvent::Completed { usage }) => {
                                    if attempt.open_reasoning.is_some() {
                                        yield TurnStep::Emit( RuntimeEvent::Failed {
                                            kind: RunFailureKind::ProviderProtocol,
                                            message: "provider completed the turn with an unfinished reasoning block".to_owned(),
                                        });
                                        yield TurnStep::Stop; return;
                                    }
                                    if let Some(call) = attempt.pending_calls.iter().find(|call| !call.completed) {
                                        yield TurnStep::Emit( RuntimeEvent::Failed {
                                            kind: RunFailureKind::ProviderProtocol,
                                            message: format!(
                                                "provider completed the turn before tool call {:?}",
                                                call.provider_call_id
                                            ),
                                        });
                                        yield TurnStep::Stop; return;
                                    }
                                    attempt.terminal_usage = usage.map(provider_usage);
                                    attempt.completed = true;
                                    break;
                                }
                                Ok(ProviderEvent::Incomplete { usage, reason }) => {
                                    if attempt.open_reasoning.is_some() {
                                        yield TurnStep::Emit( RuntimeEvent::ReasoningCompleted {
                                            kind: attempt.open_reasoning.take().expect("checked"),
                                        });
                                    }
                                    attempt.terminal_usage = usage.map(provider_usage);
                                    attempt.truncated_turn = true;
                                    attempt.truncation_reason = reason;
                                    attempt.completed = true;
                                    break;
                                }
                                Err(error) => {
                                    attempt.rejected_before_work = matches!(&error,
                                        qq_provider::ProviderError::Api { .. } | qq_provider::ProviderError::RetryAfter { .. }
                                        | qq_provider::ProviderError::ResponseFailed { kind: qq_provider::ProviderErrorKind::ContextExceeded, .. }
                                        | qq_provider::ProviderError::Configuration(_)
                                        | qq_provider::ProviderError::CredentialsUnavailable(_))
                                        && attempt.blocks.is_empty() && attempt.pending_calls.is_empty()
                                        && attempt.reasoning_bytes == 0 && !attempt.reasoning_observed
                                        && attempt.replay.is_none();
                                    if attempt.rejected_before_work && env.goal_spend_tracking {
                                        yield TurnStep::Emit(RuntimeEvent::GoalRequestRejected);
                                    }
                                    let kind = run_failure_kind(error.kind());
                                    if summarizer.is_some()
                                        && attempt.rejected_before_work
                                        && kind == RunFailureKind::ProviderContextExceeded
                                        && run.summary_fold.split(Arc::make_mut(messages))
                                    {
                                        run.summary_fold_text.clear();
                                        recovery.turn_retries = 0;
                                        recovery.output_continuations = 0;
                                        recovery.reasoning_only_truncations = 0;
                                        recovery.empty_output_retries = 0;
                                        yield TurnStep::Emit(RuntimeEvent::AssistantTurnCompleted {
                                            model_work: runtime::ModelWorkDisposition::RejectedBeforeWork,
                                            turn_ordinal,
                                            message: Message::assistant(String::new()),
                                            usage: Some(TokenUsage::default()),
                                            calls: Vec::new(),
                                            truncated: false,
                                            notice: None,
                                            goal_notices: Vec::new(),
                                            reserved_goal_notices: 0,
                                        });
                                        env.reducible_messages = messages.len().saturating_sub(1);
                                        env.inherited_effects.clear();
                                        window.reducible_message_bytes = measure_messages(&messages[..env.reducible_messages]);
                                        window.irreducible_message_bytes = measure_messages(&messages[env.reducible_messages..]);
                                        window.compatible_request = None;
                                        yield TurnStep::Emit(RuntimeEvent::ProviderOverflow {
                                            turn_ordinal,
                                            message: error.to_string(),
                                        });
                                        yield TurnStep::NextTurn; return;
                                    }
                                    let reactive_compaction = kind
                                        == RunFailureKind::ProviderContextExceeded
                                        && compactor.is_some()
                                        && attempt.blocks.is_empty()
                                        && attempt.pending_calls.is_empty()
                                        && !attempt.reasoning_observed
                                        && attempt.replay.is_none()
                                        && (reducible_messages > 0 && !run.inherited_compaction_attempted || sessions::in_run_compaction_boundary(
                                            &messages[reducible_messages.saturating_add(1)..],
                                            sessions::CONTEXT_PRUNE_KEEP_TURNS,
                                        ).is_some());
                                    if reactive_compaction {
                                        recovery.provider_overflowed = true;
                                        yield TurnStep::Emit( RuntimeEvent::ProviderOverflow {
                                            turn_ordinal,
                                            message: error.to_string(),
                                        });
                                        yield TurnStep::NextTurn; return;
                                    }
                                    if !recoverable_turn_fault(kind) {
                                        yield TurnStep::Emit( RuntimeEvent::Failed {
                                            kind,
                                            message: error.to_string(),
                                        });
                                        yield TurnStep::Stop; return;
                                    }
                                    if let Some(kind) = attempt.open_reasoning.take() {
                                        yield TurnStep::Emit( RuntimeEvent::ReasoningCompleted { kind });
                                    }
                                    attempt.retry_after = error.retry_after();
                                    attempt.turn_fault = Some((kind, error.to_string()));
                                    attempt.completed = true;
                                    break;
                                }
                            }
                        }

                        drop(provider_events);

                        if !attempt.completed {
                            if let Some(kind) = attempt.open_reasoning.take() {
                                yield TurnStep::Emit( RuntimeEvent::ReasoningCompleted { kind });
                            }
                            attempt.turn_fault = Some((
                                RunFailureKind::ProviderTransport,
                                "provider stream ended without a terminal event".to_owned(),
                            ));
                        } else if attempt.turn_fault.is_none()
                            && !attempt.interrupted_turn
                            && !attempt.truncated_turn
                            && attempt.terminal_usage.is_none()
                            && attempt.pending_calls.is_empty()
                            && attempt.blocks.iter().all(|block| matches!(block, TurnBlock::Text(text) if text.trim().is_empty()))
                            && attempt.replay.is_none()
                            && messages
                                .iter()
                                .rev()
                                .take_while(|message| message.role() == Role::User)
                                .any(|message| {
                                    message.content().iter().any(|block| matches!(block, ContentBlock::ToolResult { .. }))
                                })
                        {
                            attempt.turn_fault = Some((
                                RunFailureKind::ProviderTransport,
                                "provider completed the turn after tool results with no content and no usage".to_owned(),
                            ));
                        }

                        if attempt.interrupted_turn || attempt.truncated_turn || attempt.turn_fault.is_some() {
                            if attempt.interrupted_turn {
                                run.handled_interrupt = steering
                                    .as_ref()
                                    .map_or(run.handled_interrupt, |steering| *steering.interrupts.borrow());
                            }
                            attempt.streamed_visible_output = attempt.pending_calls.iter().any(|call| call.completed);
                            attempt.blocks.retain(|block| matches!(block, TurnBlock::Text(_)));
                            attempt.pending_calls.clear();
                        }

        yield TurnStep::Ready(StreamedTurn { prepared: PreparedTurn { turn_ordinal, budget_final_turn, final_answer_turn, checkpoint_turn, slice_report, request_system, request_has_tools, request_system_hash, system_bytes, tool_schema_bytes }, message_bytes, attempt });
                }
    }
    fn settle_turn(
        &mut self,
        streamed: StreamedTurn,
    ) -> impl futures_util::Stream<Item = TurnStep<SettledTurn>> + Send + '_ {
        stream! {
                    let Self { env, messages, steering, run, window, slice, recovery, .. } = self;
                    let RunEnvironment { plan, cancelled, file_state, summarizer, deadline, output_ceiling, ceiling_by_policy, .. } = env;
                    let output_ceiling = *output_ceiling;
                    let ceiling_by_policy = *ceiling_by_policy;
                    let deadline = *deadline;
                    let workspace = &plan.workspace;
                    let catalog = &plan.catalog;
                    let turn_recovery = plan.runtime.turn_recovery;

                    let StreamedTurn { prepared, message_bytes, attempt } = streamed;
                    let PreparedTurn { turn_ordinal, budget_final_turn, final_answer_turn, checkpoint_turn, slice_report, request_system: _, request_has_tools: _, request_system_hash: _, system_bytes, tool_schema_bytes } = prepared;
                    let AttemptScope { rejected_before_work, reasoning_bytes, reasoning_observed, pending_calls, replay, blocks, terminal_usage, completed, interrupted_turn, truncated_turn, truncation_reason, streamed_visible_output, turn_fault, retry_after, .. } = attempt;
                        window.compatible_request = terminal_usage.map(|usage| {
                            (
                                system_bytes,
                                tool_schema_bytes,
                                message_bytes,
                                usage
                                    .input_tokens
                                    .saturating_add(usage.cache_read_input_tokens)
                                    .saturating_add(usage.cache_write_input_tokens),
                            )
                        });

                        let model_work = if rejected_before_work {
                            runtime::ModelWorkDisposition::RejectedBeforeWork
                        } else if (completed && turn_fault.is_none() && !interrupted_turn)
                            || !blocks.is_empty() || !pending_calls.is_empty() || reasoning_bytes > 0 || reasoning_observed || replay.is_some() || terminal_usage.is_some() {
                            runtime::ModelWorkDisposition::Committed
                        } else {
                            runtime::ModelWorkDisposition::Uncertain
                        };
                        let assistant_content = blocks
                            .into_iter()
                            .filter_map(|block| match block {
                                TurnBlock::Text(text) if text.is_empty() => None,
                                TurnBlock::Text(text) => Some(ContentBlock::Text { text }),
                                TurnBlock::ToolCall(index) => {
                                    let call = &pending_calls[index];
                                    Some(ContentBlock::ToolCall {
                                        id: call.provider_call_id.clone(),
                                        name: call.name.clone(),
                                        arguments: serde_json::value::RawValue::from_string(
                                            call.arguments.clone(),
                                        )
                                        .expect("completed calls hold canonical JSON arguments"),
                                    })
                                }
                            })
                            .collect::<Vec<_>>();
                        let mut assistant = Message::new(Role::Assistant, assistant_content);
                        if !checkpoint_turn && !final_answer_turn && !budget_final_turn && !run.goal_handoff_requested && summarizer.is_none()
                            && assistant.content().iter().any(|block| matches!(block, ContentBlock::Text { text } if !text.trim().is_empty())) {
                            run.loop_guard.spontaneous_text();
                        }
                        run.guard_eligible.clear();
                        if completed && !interrupted_turn && !truncated_turn && turn_fault.is_none()
                            && let Some(data) = replay {
                            assistant = assistant.with_replay(data);
                        }
                        let mut calls = Vec::with_capacity(pending_calls.len());
                        let mut id_generation_failed = None;
                        for (index, pending) in pending_calls.into_iter().enumerate() {
                            let id = match ToolCallId::generate() {
                                Ok(id) => id,
                                Err(error) => {
                                    id_generation_failed = Some(error.to_string());
                                    break;
                                }
                            };
                            let known = catalog.lookup(&pending.name).map(|entry| entry.effect);
                            #[cfg(test)]
                            let known = known.or_else(|| tools::test_tool_effect(&pending.name));
                            let (effect, rejection) = match known {
                                Some(effect) => (effect, pending.rejection.map(runtime::CallRejection::Runtime)),
                                None => (
                                    catalog::EffectClass::ReadOnly,
                                    Some(runtime::CallRejection::Runtime(format!("unknown tool {:?}", pending.name))),
                                ),
                            };
                            if rejection.is_none() {
                                run.call_effects.insert(pending.provider_call_id.clone(), effect);
                            }
                            if !checkpoint_turn && !final_answer_turn && summarizer.is_none() && index < MAX_TOOL_CALLS_PER_TURN {
                                run.guard_eligible.push((id, CallKey::new(&pending.name, &pending.arguments)));
                            }
                            calls.push(RuntimeToolCall {
                                id,
                                turn_ordinal,
                                call_ordinal: u16::try_from(index + 1)
                                    .expect("the per-turn tool bound fits u16"),
                                provider_call_id: pending.provider_call_id,
                                name: pending.name,
                                arguments: pending.arguments,
                                effect,
                                rejection,
                            });
                        }
                        if let Some(message) = id_generation_failed {
                            yield TurnStep::Emit( RuntimeEvent::Failed {
                                kind: RunFailureKind::Server,
                                message,
                            });
                            yield TurnStep::Stop; return;
                        }
                        let mut search_space = tools::output::MAX_TURN_TOOL_OUTPUT_BYTES.saturating_sub(
                            calls.iter().map(|call| tools::output::result_reserve_bytes(&call.name)).sum::<usize>());
                        let page_reserve = tools::output::SEARCH_RESERVATION_BYTES;
                        for call in &mut calls {
                            if call.name == "search" && call.rejection.is_none() {
                                if search_space >= page_reserve {
                                    search_space -= page_reserve;
                                } else {
                                    run.guard_eligible.retain(|(id, _)| *id != call.id);
                                    call.rejection = Some(runtime::CallRejection::OutputBudget);
                                    slice.slice_tool_calls -= 1;
                                }
                            }
                        }
                        slice.pending_deliveries = 0;
                        yield TurnStep::Emit( RuntimeEvent::AssistantTurnCompleted {
                            model_work,                            turn_ordinal,
                            message: assistant.clone(),
                            usage: terminal_usage,
                            calls: calls.clone(),
                            truncated: truncated_turn,
                            notice: slice.pending_notice.take(),
                            goal_notices: std::mem::take(&mut slice.pending_goal_notices),
                            reserved_goal_notices: std::mem::take(&mut slice.pending_reserved_goal_notices),
                        });
                        if !env.goal_spend_tracking || model_work != runtime::ModelWorkDisposition::RejectedBeforeWork {
                            run.budget.charge_turn(terminal_usage);
                        }
                        run.budget.charge_tool_calls(calls.iter().filter(|call| call.rejection.is_none()).count());
                        if summarizer.is_some() {
                            if calls.is_empty() {
                                for block in assistant.content() {
                                    if let ContentBlock::Text { text } = block {
                                        run.summary_fold_text.push_str(text);
                                    }
                                }
                            } else {
                                run.summary_fold_text.clear();
                            }
                        }
                        if summarizer.is_some() && !calls.is_empty() {
                            run.summarizer_rejected_turns += 1;
                            if run.summarizer_rejected_turns > 1 {
                                yield TurnStep::Emit( RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: "the compaction summarizer called a tool on two turns; tools are unavailable during compaction".to_owned(),
                                });
                                yield TurnStep::Stop; return;
                            }
                        }

                        if let Some((kind, message)) = turn_fault {
                            if recovery.turn_retries >= MAX_TURN_RETRIES {
                                yield TurnStep::Emit( RuntimeEvent::Paused {
                                    pause: Box::new(qq_protocol::RunPause::ProviderRetry {
                                        kind,
                                        message,
                                        turn_ordinal,
                                        attempts: recovery.turn_retries,
                                    }),
                                });
                                yield TurnStep::Stop; return;
                            }
                            recovery.turn_retries += 1;
                            let delay = turn_recovery.delay_after(recovery.turn_retries,retry_after);
                            yield TurnStep::Emit( RuntimeEvent::TurnRetrying {
                                turn_ordinal,
                                attempt: recovery.turn_retries,
                                delay,
                                kind,
                                message,
                            });
                            if assistant.has_content() {
                                window.irreducible_message_bytes = window.irreducible_message_bytes
                                    .saturating_add(measure_message(&assistant));
                                Arc::make_mut(messages).push(assistant);
                                run.live_turns.push(turn_ordinal);
                            }
                            if messages.last().is_some_and(|message| message.role() == Role::Assistant) {
                                Arc::make_mut(messages).push(Message::user(TURN_RETRY_CONTINUE_NOTICE));
                                window.irreducible_message_bytes = window.irreducible_message_bytes
                                    .saturating_add(measure_message(messages.last().expect("just pushed")));
                            }
                            let sleep = tokio::time::sleep(delay);
                            tokio::pin!(sleep);
                            tokio::select! {
                                biased;
                                () = cancelled.cancelled() => { yield TurnStep::Stop; return; },
                                () = runtime::RunDeadline::wait(deadline) => {
                                    yield TurnStep::Emit( RuntimeEvent::BudgetExhausted {
                                        exhaustion: deadline.expect("only a finite deadline wakes").exhaustion(),
                                    });
                                    yield TurnStep::Stop; return;
                                }
                                () = &mut sleep => {}
                            }
                            yield TurnStep::NextTurn; return;
                        }
                        recovery.turn_retries = 0;
                        if truncated_turn {
                            if !budget_final_turn {
                                if !assistant.has_content()
                                    && truncation_reason == qq_provider::IncompleteReason::OutputTokens
                                {
                                    let can_raise = recovery.empty_output_retries < MAX_EMPTY_OUTPUT_RETRIES
                                        && run.max_output_tokens < output_ceiling;
                                    if streamed_visible_output {
                                        recovery.reasoning_only_truncations = 0;
                                    } else {
                                        recovery.reasoning_only_truncations =
                                            recovery.reasoning_only_truncations.saturating_add(1);
                                    }
                                    if !can_raise && !streamed_visible_output && summarizer.is_none()
                                        && recovery.output_continuations < MAX_OUTPUT_CONTINUATIONS
                                    {
                                        recovery.output_continuations += 1;
                                        yield TurnStep::Emit( RuntimeEvent::OutputTruncated {
                                            turn_ordinal,
                                            continuation: recovery.output_continuations,
                                        });
                                        let text = REASONING_TRUNCATED_NOTICE.to_owned();
                                        let notice = Message::user(text.clone());
                                        window.irreducible_message_bytes = window.irreducible_message_bytes
                                            .saturating_add(measure_message(&notice));
                                        Arc::make_mut(messages).push(notice);
                                        slice.pending_goal_notices.push(runtime::OrderedTurnNotice {
                                            text, preceding_deliveries: slice.pending_deliveries,
                                        });
                                        yield TurnStep::NextTurn; return;
                                    }
                                    if !can_raise && !streamed_visible_output {
                                        yield TurnStep::Emit( RuntimeEvent::Failed {
                                            kind: RunFailureKind::ProviderOutputTruncated,
                                            message: format!(
                                                "the provider stopped at its output token limit ({max_output_tokens} tokens) \
                                                 without producing any visible output on {} consecutive turn{}; the \
                                                 limit was spent on reasoning. {}",
                                                recovery.reasoning_only_truncations,
                                                if recovery.reasoning_only_truncations == 1 { "" } else { "s" },
                                                if ceiling_by_policy {
                                                    format!(
                                                        "Managed policy caps output at {output_ceiling} tokens, so raising \
                                                         `max_output_tokens` cannot help: lower `reasoning_effort` or ask the \
                                                         administrator to raise `policy.max_output_tokens`"
                                                    )
                                                } else {
                                                    format!(
                                                        "Raise `max_output_tokens` (model ceiling {output_ceiling}) or lower \
                                                         `reasoning_effort`"
                                                    )
                                                },
                                                max_output_tokens = run.max_output_tokens
                                            ),
                                        });
                                        yield TurnStep::Stop; return;
                                    }
                                }
                                if !assistant.has_content()
                                    && truncation_reason == qq_provider::IncompleteReason::OutputTokens
                                    && recovery.empty_output_retries < MAX_EMPTY_OUTPUT_RETRIES
                                    && run.max_output_tokens < output_ceiling
                                {
                                    if recovery.output_continuations >= MAX_OUTPUT_CONTINUATIONS {
                                        yield TurnStep::Emit( RuntimeEvent::Failed {
                                            kind: RunFailureKind::ProviderOutputTruncated,
                                            message: format!(
                                                "the provider stopped at its output token limit ({max_output_tokens} tokens) on \
                                                 {} consecutive turns; the partial answer is in the transcript",
                                                u32::from(MAX_OUTPUT_CONTINUATIONS) + 1,
                                                max_output_tokens = run.max_output_tokens
                                            ),
                                        });
                                        yield TurnStep::Stop; return;
                                    }
                                    recovery.empty_output_retries += 1;
                                    run.max_output_tokens = run.max_output_tokens
                                        .saturating_mul(2)
                                        .min(output_ceiling);
                                    recovery.output_continuations += 1;
                                    yield TurnStep::Emit( RuntimeEvent::OutputTruncated {
                                        turn_ordinal,
                                        continuation: recovery.output_continuations,
                                    });
                                    yield TurnStep::NextTurn; return;
                                }
                                if recovery.output_continuations >= MAX_OUTPUT_CONTINUATIONS {
                                    yield TurnStep::Emit( RuntimeEvent::Failed {
                                        kind: RunFailureKind::ProviderOutputTruncated,
                                        message: format!(
                                            "the provider stopped at its output token limit ({max_output_tokens} tokens) on \
                                             {} consecutive turns; the partial answer is in the transcript",
                                            u32::from(MAX_OUTPUT_CONTINUATIONS) + 1,
                                                max_output_tokens = run.max_output_tokens
                                        ),
                                    });
                                    yield TurnStep::Stop; return;
                                }
                                recovery.output_continuations += 1;
                                yield TurnStep::Emit( RuntimeEvent::OutputTruncated {
                                    turn_ordinal,
                                    continuation: recovery.output_continuations,
                                });
                                if assistant.has_content() {
                                    recovery.reasoning_only_truncations = 0;
                                    window.irreducible_message_bytes = window.irreducible_message_bytes
                                        .saturating_add(measure_message(&assistant));
                                    Arc::make_mut(messages).push(assistant);
                                    run.live_turns.push(turn_ordinal);
                                }
                                if messages.last().is_some_and(|message| message.role() == Role::Assistant) {
                                    Arc::make_mut(messages).push(Message::user(OUTPUT_TRUNCATED_CONTINUE_NOTICE));
                                    window.irreducible_message_bytes = window.irreducible_message_bytes
                                        .saturating_add(measure_message(messages.last().expect("just pushed")));
                                }
                                yield TurnStep::NextTurn; return;
                            }
                        } else {
                            recovery.output_continuations = 0;
                            recovery.reasoning_only_truncations = 0;
                            recovery.empty_output_retries = 0;
                        }

                        if interrupted_turn {
                            yield TurnStep::Emit( RuntimeEvent::Interrupted { turn_ordinal });
                            if assistant.has_content() {
                                window.irreducible_message_bytes = window.irreducible_message_bytes
                                    .saturating_add(measure_message(&assistant));
                                Arc::make_mut(messages).push(assistant);
                                run.live_turns.push(turn_ordinal);
                            }
                            if let Some(applied) = apply_steering(&mut run.stall, steering, Arc::make_mut(messages), &mut window.irreducible_message_bytes, workspace, file_state).await {
                        run.loop_guard.mutation();
                                for steer in applied {
                                    yield TurnStep::Emit( RuntimeEvent::SteeringApplied {
                                        message_id: steer.message_id,
                                        turn_ordinal: turn_ordinal.saturating_add(1),
                                        attachments: steer.attachments,
                                    });
                                }
                            }
                            if messages.last().is_some_and(|message| message.role() == Role::Assistant) {
                                Arc::make_mut(messages).push(Message::user(INTERRUPT_CONTINUE_NOTICE));
                                window.irreducible_message_bytes = window.irreducible_message_bytes
                                    .saturating_add(measure_message(messages.last().expect("just pushed")));
                            }
                            yield TurnStep::NextTurn; return;
                        }

                        if budget_final_turn {
                            let BudgetDecision::Exhausted(exhaustion) = run.budget.before_turn(
                                tokio::time::Instant::now(),
                                0,
                            ) else {
                                unreachable!("a requested final response always settles the run")
                            };
                            yield TurnStep::Emit( RuntimeEvent::BudgetExhausted { exhaustion });
                            yield TurnStep::Stop; return;
                        }
                        if calls.is_empty()
                            && let Some(kind) = run.budget.exceeded(tokio::time::Instant::now())
                            && matches!(
                                kind,
                                BudgetLimitKind::Cost
                                    | BudgetLimitKind::CostUnknown
                                    | BudgetLimitKind::TotalTokens
                            )
                        {
                            let exhaustion = run.budget.exhaustion(kind, false, tokio::time::Instant::now());
                            yield TurnStep::Emit( RuntimeEvent::BudgetExhausted { exhaustion });
                            yield TurnStep::Stop; return;
                        }

                        if final_answer_turn && calls.is_empty() {
                            yield TurnStep::Emit( RuntimeEvent::Completed { final_output: None });
                            yield TurnStep::Stop; return;
                        }
                        if checkpoint_turn {
                            if slice_report {
                                slice.slice_tool_calls = 0;
                                run.slices = run.slices.saturating_add(1);
                            }
                            run.stall.reported();
                            slice.placed_report = None;
                            slice.continuing_slice = true;
                            if calls.is_empty() {
                                let assistant = if assistant.has_content() {
                                    assistant
                                } else {
                                    Message::assistant(EMPTY_TURN_PLACEHOLDER)
                                };
                                window.irreducible_message_bytes = window.irreducible_message_bytes
                                    .saturating_add(measure_message(&assistant));
                                Arc::make_mut(messages).push(assistant);
                                run.live_turns.push(turn_ordinal);
                                if let Some(applied) = apply_steering(&mut run.stall, steering, Arc::make_mut(messages), &mut window.irreducible_message_bytes, workspace, file_state).await {
                        run.loop_guard.mutation();
                                    for steer in applied {
                                        yield TurnStep::Emit( RuntimeEvent::SteeringApplied {
                                            message_id: steer.message_id,
                                            turn_ordinal: turn_ordinal.saturating_add(1),
                                            attachments: steer.attachments,
                                        });
                                    }
                                }
                                yield TurnStep::NextTurn; return;
                            }
                        }

        yield TurnStep::Ready(SettledTurn { turn_ordinal, final_answer_turn, assistant, calls });
                }
    }
    fn route_turn(
        &mut self,
        settled: SettledTurn,
    ) -> impl futures_util::Stream<Item = TurnStep<TurnRoute>> + Send + '_ {
        stream! {
                    let Self { env, messages, steering, run, window, recovery, slice, .. } = self;
                    let RunEnvironment { plan, file_state, spawner, .. } = env;
                    let workspace = &plan.workspace;

        let SettledTurn { turn_ordinal, final_answer_turn, assistant, calls } = settled;
                        if calls.is_empty() {
                            if let Some(applied) = apply_steering(&mut run.stall, steering, Arc::make_mut(messages), &mut window.irreducible_message_bytes, workspace, file_state).await {
                        run.loop_guard.mutation();
                                let assistant = if assistant.has_content() {
                                    assistant
                                } else {
                                    Message::assistant(EMPTY_TURN_PLACEHOLDER)
                                };
                                window.irreducible_message_bytes = window.irreducible_message_bytes
                                    .saturating_add(measure_message(&assistant));
                                let keep = messages.len() - applied.len();
                                let steering_messages = Arc::make_mut(messages).split_off(keep);
                                Arc::make_mut(messages).push(assistant);
                                run.live_turns.push(turn_ordinal);
                                Arc::make_mut(messages).extend(steering_messages);
                                for steer in applied {
                                    yield TurnStep::Emit( RuntimeEvent::SteeringApplied {
                                        message_id: steer.message_id,
                                        turn_ordinal: turn_ordinal.saturating_add(1),
                                        attachments: steer.attachments,
                                    });
                                }
                                yield TurnStep::NextTurn; return;
                            }
                            if let Some(spawner) = &spawner
                                && spawner.outstanding_detached() > 0
                            {
                                let assistant = if assistant.has_content() {
                                    assistant
                                } else {
                                    Message::assistant(EMPTY_TURN_PLACEHOLDER)
                                };
                                window.irreducible_message_bytes = window.irreducible_message_bytes
                                    .saturating_add(measure_message(&assistant));
                                Arc::make_mut(messages).push(assistant);
                                run.live_turns.push(turn_ordinal);
                                #[cfg(test)]
                                spawner.waiting_for_test();
                                let mut wait_scope = WaitScope { delivery_retry: SUBAGENT_DELIVERY_RETRY };
                                loop {
                                    tokio::select! {
                                        biased;
                                        () = steering_arrived(steering, run.handled_interrupt) => {}
                                        () = spawner.child_settled() => {}
                                    }
                                    run.handled_interrupt = steering
                                        .as_ref()
                                        .map_or(run.handled_interrupt, |steering| *steering.interrupts.borrow());
                                    if let Some(applied) = apply_steering(&mut run.stall, steering, Arc::make_mut(messages), &mut window.irreducible_message_bytes, workspace, file_state).await {
                        run.loop_guard.mutation();
                                        for steer in applied {
                                            yield TurnStep::Emit( RuntimeEvent::SteeringApplied {
                                                message_id: steer.message_id,
                                                turn_ordinal: turn_ordinal.saturating_add(1),
                                                attachments: steer.attachments,
                                            });
                                        }
                                        break;
                                    }
                                    match deliver_children(
                                        spawner,
                                        Boundary { turn_ordinal: turn_ordinal.saturating_add(1), reports: runtime::ReportDelivery::WithAnswers },
                                        Arc::make_mut(messages),
                                        &mut window.irreducible_message_bytes,
                                        &mut run.budget,
                                        &mut run.stall,
                                    )
                                    .await
                                    {
                                        Ok(delivered) if delivered.answers > 0 => {
                                            slice.pending_deliveries += delivered.answers + delivered.reports;
                                            recovery.wait_delivered_for = Some(turn_ordinal.saturating_add(1));
                                            break;
                                        }
                                        Ok(delivered) => {
                                            debug_assert_eq!(delivered.reports, 0);
                                            if spawner.settled_detached() {
                                                tokio::time::sleep(wait_scope.delivery_retry).await;
                                                wait_scope.delivery_retry = (wait_scope.delivery_retry * 2).min(SUBAGENT_DELIVERY_RETRY_MAX);
                                            }
                                        }
                                        Err(error) => {
                                            yield TurnStep::Emit( RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() });
                                            yield TurnStep::Stop; return;
                                        }
                                    }
                                }
                                yield TurnStep::NextTurn; return;
                            }

         yield TurnStep::Ready(TurnRoute::Candidate { turn_ordinal, assistant });
         return;
        }
                        window.irreducible_message_bytes = window.irreducible_message_bytes
                            .saturating_add(measure_message(&assistant));
                        Arc::make_mut(messages).push(assistant);
                        run.live_turns.push(turn_ordinal);

         yield TurnStep::Ready(TurnRoute::Tools { turn_ordinal, final_answer_turn, calls });

                }
    }
    fn finish_candidate(
        &mut self,
        turn_ordinal: u32,
        assistant: Message,
    ) -> impl futures_util::Stream<Item = TurnStep<()>> + Send + '_ {
        stream! {
            let Self { env, messages, steering, run, window, slice, .. } = self;
            let RunEnvironment { plan, file_state, audit_hook, output, limits, deadline, audit_prompt, goal, summarizer, .. } = env;
            let refreshed_goal_notice = if !run.goal_handoff_requested && let Some(goal) = goal {
                let refreshed = tokio::select! {
                    biased;
                    () = env.cancelled.cancelled() => { yield TurnStep::Stop; return; },
                    () = runtime::RunDeadline::wait(*deadline) => {
                        yield TurnStep::Emit(RuntimeEvent::BudgetExhausted { exhaustion: deadline.expect("only a finite deadline wakes").exhaustion() });
                        yield TurnStep::Stop; return;
                    },
                    refreshed = goal.refresh_handoff_notice() => refreshed,
                };
                match refreshed {
                    Ok(notice) => notice,
                    Err(error) => {
                        yield TurnStep::Emit(RuntimeEvent::Failed { kind: RunFailureKind::Server, message: format!("refresh final goal handoff: {error}") });
                        yield TurnStep::Stop; return;
                    }
                }
            } else { None };
            if !run.goal_handoff_requested && goal.as_ref().is_some_and(sessions::GoalRunInput::final_slot) {
                run.goal_handoff_requested = true;
                run.goal_final_slot_announced = true;
                let assistant = if assistant.has_content() { assistant } else { Message::assistant(EMPTY_TURN_PLACEHOLDER) };
                window.irreducible_message_bytes = window.irreducible_message_bytes.saturating_add(measure_message(&assistant));
                Arc::make_mut(messages).push(assistant);
                run.live_turns.push(turn_ordinal);
                if let Some(text) = refreshed_goal_notice {
                    let notice = Message::user(text.clone());
                    window.irreducible_message_bytes = window.irreducible_message_bytes.saturating_add(measure_message(&notice));
                    Arc::make_mut(messages).push(notice);
                    slice.pending_goal_notices.push(runtime::OrderedTurnNotice { text, preceding_deliveries: slice.pending_deliveries });
                }
                let text = sessions::GoalRunInput::HANDOFF_NOTICE.to_owned();
                let notice = Message::user(text.clone());
                window.irreducible_message_bytes = window.irreducible_message_bytes.saturating_add(measure_message(&notice));
                Arc::make_mut(messages).push(notice);
                slice.pending_goal_notices.push(runtime::OrderedTurnNotice { text, preceding_deliveries: slice.pending_deliveries });
                yield TurnStep::NextTurn; return;
            }
            let _limits = *limits;
            let _deadline = *deadline;
            let workspace = &plan.workspace;


                    if let Some(hook) = &audit_hook
                        && (run.audit_revisions == 0
                            || run.audit_revisions < plan.runtime.audit.max_revisions)
                        && run.audit_triggers.fires(plan.runtime.audit.mode)
                        && let Ok(mut child_limits) = run.budget.child_budget(tokio::time::Instant::now())
                    {
                        child_limits.limits.max_model_turns = Some(
                            child_limits
                                .limits
                                .max_model_turns
                                .map_or(runtime::MAX_AUDIT_CHILD_TURNS, |turns| {
                                    turns.min(runtime::MAX_AUDIT_CHILD_TURNS)
                                }),
                        );
                        let audit_deadline = tokio::time::Instant::now()
                            + std::time::Duration::from_millis(runtime::MAX_AUDIT_CHILD_DURATION_MS);
                        child_limits.deadline = Some(
                            child_limits
                                .deadline
                                .map_or(audit_deadline, |deadline| deadline.min(audit_deadline)),
                        );
                        let answer = assistant
                            .content()
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        let mut auditing = hook
                            .audit(runtime::AuditRequest {
                                prompt: audit_prompt.clone(),
                                answer: bounded_text(&answer, runtime::MAX_AUDIT_ANSWER_BYTES),
                                actions: run.audit_actions.clone(),
                                role: plan.runtime.audit.role,
                                revision: run.audit_revisions,
                            }, child_limits);
                        let verdict = tokio::select! {
                            biased;
                            () = interrupt_requested(steering, run.handled_interrupt) => None,
                            verdict = &mut auditing => Some(verdict),
                        };
                        drop(auditing);
                        let audit_interrupted = verdict.is_none();
                        let verdict = match verdict {
                            Some(verdict) => verdict,
                            None => {
                                let spends = match hook.drain().await {
                                    Ok(spends) => spends,
                                    Err(error) => {
                                        yield TurnStep::Emit( RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() });
                                        yield TurnStep::Stop; return;
                                    }
                                };
                                let spend = match spends.as_slice() {
                                    [] => SpawnAgentSpend::NONE,
                                    [spend] => *spend,
                                    _ => {
                                        yield TurnStep::Emit( RuntimeEvent::Failed { kind: RunFailureKind::Server, message: "audit cleanup found multiple uncharged children".to_owned() });
                                        yield TurnStep::Stop; return;
                                    }
                                };
                                runtime::AuditVerdict {
                                    usage: spend.usage, cost_usd_nanos: spend.cost_usd_nanos,
                                    ..runtime::AuditVerdict::unavailable()
                                }
                            }
                        };
                        run.budget.charge_child(verdict.usage, verdict.cost_usd_nanos);
                        hook.acknowledge();
                        let revise = verdict.outcome == qq_protocol::AuditOutcome::Revised
                            && run.audit_revisions < plan.runtime.audit.max_revisions;
                        yield TurnStep::Emit( RuntimeEvent::Audited {
                            outcome: verdict.outcome,
                            findings: verdict.findings.clone(),
                            revisions: run.audit_revisions,
                            usage: verdict.usage,
                            cost_usd_nanos: verdict.cost_usd_nanos,
                            audit_session: verdict.audit_session,
                        });
                        if audit_interrupted {
                            run.handled_interrupt = steering.as_ref().map_or(run.handled_interrupt, |steering| *steering.interrupts.borrow());
                            window.irreducible_message_bytes = window.irreducible_message_bytes.saturating_add(measure_message(&assistant));
                            Arc::make_mut(messages).push(assistant);
                            run.live_turns.push(turn_ordinal);
                            yield TurnStep::Emit( RuntimeEvent::Interrupted { turn_ordinal });
                            if let Some(applied) = apply_steering(&mut run.stall, steering, Arc::make_mut(messages), &mut window.irreducible_message_bytes, workspace, file_state).await {
                        run.loop_guard.mutation();
                                for steer in applied {
                                    yield TurnStep::Emit( RuntimeEvent::SteeringApplied { message_id: steer.message_id, turn_ordinal: turn_ordinal.saturating_add(1), attachments: steer.attachments });
                                }
                            }
                            yield TurnStep::NextTurn; return;
                        }
                        if revise {
                            run.audit_revisions += 1;
                            window.irreducible_message_bytes = window.irreducible_message_bytes
                                .saturating_add(measure_message(&assistant));
                            Arc::make_mut(messages).push(assistant);
                            run.live_turns.push(turn_ordinal);
                            let mut notice = String::from(runtime::AUDIT_REVISION_NOTICE);
                            for finding in &verdict.findings {
                                notice.push_str("\n- ");
                                notice.push_str(finding);
                            }
                            Arc::make_mut(messages).push(Message::user(notice));
                            window.irreducible_message_bytes = window.irreducible_message_bytes
                                .saturating_add(measure_message(messages.last().expect("just pushed")));
                            yield TurnStep::NextTurn; return;
                        }
                    }
                    if let Some(applied) = apply_steering(&mut run.stall, steering, Arc::make_mut(messages), &mut window.irreducible_message_bytes, workspace, file_state).await {
                        run.loop_guard.mutation();
                        window.irreducible_message_bytes = window.irreducible_message_bytes.saturating_add(measure_message(&assistant));
                        let keep = messages.len() - applied.len();
                        let queued = Arc::make_mut(messages).split_off(keep);
                        Arc::make_mut(messages).push(assistant);
                        run.live_turns.push(turn_ordinal);
                        Arc::make_mut(messages).extend(queued);
                        for steer in applied { yield TurnStep::Emit( RuntimeEvent::SteeringApplied { message_id: steer.message_id, turn_ordinal: turn_ordinal.saturating_add(1), attachments: steer.attachments }); }
                        yield TurnStep::NextTurn; return;
                    }
                    if let Some(kind) = run.budget.exceeded(tokio::time::Instant::now()) {
                        yield TurnStep::Emit( RuntimeEvent::BudgetExhausted { exhaustion: run.budget.exhaustion(kind, false, tokio::time::Instant::now()) });
                        yield TurnStep::Stop; return;
                    }
                    let final_output = match &output {
                        None => None,
                        Some(schema) => {
                            let answer = assistant
                                .content()
                                .iter()
                                .filter_map(|block| match block {
                                    ContentBlock::Text { text } => Some(text.as_str()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            let validation = schema.validate(&answer);
                            if let Err(errors) = &validation
                                && run.output_repairs < schema.repair_turns()
                            {
                                run.output_repairs += 1;
                                yield TurnStep::Emit( RuntimeEvent::OutputRepairRequested {
                                    turn_ordinal,
                                    repair: run.output_repairs,
                                    errors: output::bounded_errors(errors.clone()),
                                });
                                window.irreducible_message_bytes = window.irreducible_message_bytes
                                    .saturating_add(measure_message(&assistant));
                                Arc::make_mut(messages).push(assistant);
                                run.live_turns.push(turn_ordinal);
                                let text = output::repair_notice(errors);
                                Arc::make_mut(messages).push(Message::user(text.clone()));
                                slice.pending_goal_notices.push(runtime::OrderedTurnNotice {
                                    text,
                                    preceding_deliveries: slice.pending_deliveries,
                                });
                                window.irreducible_message_bytes = window.irreducible_message_bytes
                                    .saturating_add(measure_message(messages.last().expect("just pushed")));
                                yield TurnStep::NextTurn; return;
                            }
                            Some(Box::new(output::final_output(validation, run.output_repairs)))
                        }
                    };
                    if let Some(applied) = apply_steering(&mut run.stall, steering, Arc::make_mut(messages), &mut window.irreducible_message_bytes, workspace, file_state).await {
                        run.loop_guard.mutation();
                        window.irreducible_message_bytes = window.irreducible_message_bytes.saturating_add(measure_message(&assistant));
                        let keep = messages.len() - applied.len();
                        let queued = Arc::make_mut(messages).split_off(keep);
                        Arc::make_mut(messages).push(assistant);
                        run.live_turns.push(turn_ordinal);
                        Arc::make_mut(messages).extend(queued);
                        for steer in applied { yield TurnStep::Emit( RuntimeEvent::SteeringApplied { message_id: steer.message_id, turn_ordinal: turn_ordinal.saturating_add(1), attachments: steer.attachments }); }
                        yield TurnStep::NextTurn; return;
                    }
                     if summarizer.is_some() {
                         match run.summary_fold.advance(&mut run.summary_fold_text) {
                             Ok(Some(next)) => {
                                 *messages = Arc::new(next);
                                 env.reducible_messages = messages.len().saturating_sub(1);
                                 env.inherited_effects.clear();
                                 window.reducible_message_bytes = measure_messages(&messages[..env.reducible_messages]);
                                 window.irreducible_message_bytes = measure_messages(&messages[env.reducible_messages..]);
                                 window.compatible_request = None;
                                 yield TurnStep::NextTurn; return;
                             }
                             Ok(None) => {}
                             Err(error) => {
                                 yield TurnStep::Emit(RuntimeEvent::Failed {
                                     kind: RunFailureKind::ProviderResponse,
                                     message: error.to_string(),
                                 });
                                 yield TurnStep::Stop; return;
                             }
                         }
                     }
                    yield TurnStep::Emit( RuntimeEvent::Completed { final_output });
                    yield TurnStep::Stop; return;


        }
    }
    fn settle_tools(
        &mut self,
        turn_ordinal: u32,
        final_answer_turn: bool,
        calls: Vec<RuntimeToolCall>,
    ) -> impl futures_util::Stream<Item = TurnStep<ToolOutcomes>> + Send + '_ {
        stream! {
                    let Self { env, steering, run, .. } = self;
                    let RunEnvironment { plan, cancelled, gate, file_state, spawner, history, goal, spills, audit_hook, tool_tasks, output: _, limits, deadline, base_specs, static_filter, .. } = env;
                    let limits = *limits;
                    let workspace = &plan.workspace;
                    let catalog = &plan.catalog;
                    let _model = &plan.runtime.model;
                    let skills = &plan.skills;
                    let pack_roots = &plan.pack_roots;
                    let hosts = &plan.hosts;
                    let delegation = &plan.runtime.delegation;
                    let shell_policy = &plan.runtime.shell;
                    let approved_read_images = &plan.runtime.approved_read_images;
                    let process_producer = &plan.runtime.process_producer;
                    let network_policy = &plan.runtime.network;
                    let reasoning_effort = plan.runtime.reasoning_effort;


                        let mut results: Vec<Option<RetainedResult>> = vec![None; calls.len()];
                        let mut started_calls = vec![false; calls.len()];
                        let mut tool_scope = ToolScope { interrupted: false };
                        let bounded_child_spend = limits.max_cost_usd_nanos.is_some()
                            || limits.max_total_tokens.is_some()
                            || limits.max_input_tokens.is_some()
                            || limits.max_output_tokens.is_some();
                        let detach_spawns = !bounded_child_spend;
                        let execute_one = |call: RuntimeToolCall,
                                           output: Option<
                            tokio::sync::mpsc::Sender<String>,
                        >, child_limits: Result<runtime::ChildBudget, BudgetLimitKind>, offered: bool| {
                            let workspace = (*workspace).clone();
                            let file_state = Arc::clone(file_state);
                            let cancelled = cancelled.clone();
                            let catalog = Arc::clone(catalog);
                            let skills = Arc::clone(skills);
                            let pack_roots = Arc::clone(pack_roots);
                            let hosts = Arc::clone(hosts);
                            let spawner = spawner.clone();
                            let tool_tasks = tool_tasks.clone();
                            let history = history.clone();
                            let spills = spills.clone();
                            let delegation = Arc::clone(delegation);
                            let shell_policy = Arc::clone(shell_policy);
                            let approved_read_images = Arc::clone(approved_read_images);
                            let process_producer = Arc::clone(process_producer);
                            let network_policy = Arc::clone(network_policy);

                            let tool_span = tracing::info_span!(target: "qq::diagnostic", "tool", call_ordinal = call.call_ordinal);
                            async move {
                                let mut child_spend: Option<SpawnAgentSpend> = None;
                                let host = catalog.lookup(&call.name).map(|entry| entry.host);
                                let strict_refusal = (matches!(call.name.as_str(), "shell" | "exec")
                                    && shell_policy.builtin_preference == runtime::BuiltinPreference::Strict)
                                    .then(|| {
                                        let command = match approval::classify(call.effect, &call.name, &call.arguments, &network_policy) {
                                            approval::ToolClass::Shell { command, .. } => command,
                                            _ => String::new(),
                                        };
                                        runtime::builtin_alternative(&command)
                                    })
                                    .flatten()
                                    .filter(|alternative| alternative.refuse)
                                    .map(|alternative| {
                                        format!(
                                            "use_builtin: {} is refused under policy.builtin_preference=strict; use {} instead",
                                            alternative.program, alternative.builtin
                                        )
                                    });
                                let result = match call.rejection.clone().or_else(|| strict_refusal.map(runtime::CallRejection::Runtime)) {
                                    Some(error) => tools::ToolOutput::verbatim_error(error.into_message()),
                                    None if host == Some(catalog::ToolHost::SpawnAgent) => match &spawner {
                                        Some(spawner) => {
                                            match serde_json::from_str::<tools::SpawnAgentArgs>(
                                                &call.arguments,
                                            ) {
                                                Ok(arguments) if arguments.task.trim().is_empty() => {
                                                    tools::ToolOutput::refused("task must not be empty".to_owned())
                                                }
                                                Ok(mut arguments) => {
                                                    arguments.model = arguments.model.and_then(|model| {
                                                        let model = model.trim().to_owned();
                                                        (!model.is_empty()).then_some(model)
                                                    });
                                                    let resolution = resolve_delegation_route(
                                                        &delegation,
                                                        arguments.model,
                                                        arguments.role,
                                                        reasoning_effort,
                                                    );
                                                    match (child_limits, resolution) {
                                                        (_, Err(message)) => tools::ToolOutput::refused(message),
                                                        (Err(kind), Ok(_)) => tools::ToolOutput::refused(
                                                            format!(
                                                                "this run cannot afford a sub-agent: its {} budget is spent; continue with what you have",
                                                                kind.as_str()
                                                            ),
                                                        ),
                                                        (Ok(child_budget), Ok((model, child_effort))) => {
                                                            let outcome = spawner
                                                                .spawn(SpawnRequest {
                                                                    call_id: call.id,
                                                                    task: arguments.task,
                                                                    model,
                                                                    reasoning_effort: child_effort,
                                                                    authority: arguments.authority,
                                                                    budget: child_budget,
                                                                    purpose: qq_protocol::SessionPurpose::Task,
                                                                    detached: detach_spawns,
                                                                })
                                                                .await;
                                                            if !outcome.detached {
                                                                child_spend = Some(outcome.spend);
                                                            }
                                                            tools::bounded_result(
                                                                outcome.content,
                                                                outcome.is_error,
                                                            )
                                                        }
                                                    }
                                                }
                                                Err(error) => tools::ToolOutput::refused(format!("invalid arguments: {error}")),
                                            }
                                        }
                                        None => {
                                            tools::ToolOutput::refused(SPAWN_UNAVAILABLE_RESULT)
                                        }
                                    },
                                    None if host == Some(catalog::ToolHost::WaitAgents) => match &spawner {
                                        Some(spawner) => match serde_json::from_str::<tools::WaitAgentsArgs>(&call.arguments) {
                                            Ok(arguments) if !(1..=tools::MAX_WAIT_AGENTS_SECS).contains(&arguments.timeout_seconds) => {
                                                tools::ToolOutput::refused(format!("timeout_seconds must be between 1 and {}", tools::MAX_WAIT_AGENTS_SECS))
                                            }
                                            Ok(arguments) if arguments.ids.as_ref().is_some_and(|ids| ids.len() > usize::from(sessions::MAX_SPAWNED_CHILDREN_PER_RUN)) => {
                                                tools::ToolOutput::refused(format!("ids may name at most {} sub-agents", sessions::MAX_SPAWNED_CHILDREN_PER_RUN))
                                            }
                                            Ok(arguments) => match arguments
                                                .ids
                                                .map(|ids| ids.iter().map(|id| id.trim().parse::<qq_protocol::SessionId>()).collect::<Result<Vec<_>, _>>())
                                                .transpose()
                                            {
                                                Err(_) => tools::ToolOutput::refused("ids must be sub-agent ids from spawn_agent results".to_owned()),
                                                Ok(ids) => match spawner
                                                    .wait_children(ids, Duration::from_secs(arguments.timeout_seconds))
                                                    .await
                                                {
                                                    Ok(report) => tools::bounded_result(report.render(arguments.timeout_seconds), false),
                                                    Err(error) => tools::ToolOutput::unobserved(error.to_string()),
                                                },
                                            },
                                            Err(error) => tools::ToolOutput::refused(format!("invalid arguments: {error}")),
                                        },
                                        None => tools::ToolOutput::refused(SPAWN_UNAVAILABLE_RESULT),
                                    },
                                    None if host == Some(catalog::ToolHost::CancelAgent) => match &spawner {
                                        Some(spawner) => match serde_json::from_str::<tools::CancelAgentArgs>(&call.arguments) {
                                            Ok(arguments) => match arguments.id.trim().parse::<qq_protocol::SessionId>() {
                                                Ok(id) => match spawner.cancel_child(id).await {
                                                    Ok(outcome) => {
                                                        let (text, is_error) = outcome.render(id);
                                                        tools::bounded_result(text, is_error)
                                                    }
                                                    Err(error) => tools::ToolOutput::unobserved(error.to_string()),
                                                },
                                                Err(_) => tools::ToolOutput::refused("id must be a sub-agent id from a spawn_agent result".to_owned()),
                                            },
                                            Err(error) => tools::ToolOutput::refused(format!("invalid arguments: {error}")),
                                        },
                                        None => tools::ToolOutput::refused(SPAWN_UNAVAILABLE_RESULT),
                                    },
                                    None if host == Some(catalog::ToolHost::CommandStatus) => {
                                        tools::command_status(&tool_tasks, &call.arguments, cancelled).await
                                    }
                                    None if host == Some(catalog::ToolHost::SearchHistory) && history.is_some() => {
                                        let history = history.expect("the history searcher was just checked");
                                        match serde_json::from_str::<SearchHistoryArgs>(&call.arguments) {
                                            Ok(arguments) if arguments.query.trim().is_empty() => {
                                                tools::ToolOutput::refused("query must not be empty")
                                            }
                                            Ok(arguments) => {
                                                let limit = arguments.limit.clamp(1, crate::runtime::MAX_HISTORY_MATCHES);
                                                match history.search(arguments.query.clone(), limit, arguments.cursor).await {
                                                    Ok(search) => tools::bounded_result(
                                                        render_history_matches(&arguments.query, &search),
                                                        false,
                                                    ),
                                                    Err(error) => tools::ToolOutput::unobserved(error),
                                                }
                                            }
                                            Err(error) => tools::ToolOutput::refused(format!("invalid arguments: {error}")),
                                        }
                                    }
                                    None if host == Some(catalog::ToolHost::ReadToolResult) && spills.is_some() => {
                                        let spills = spills.expect("the spill reader was just checked");
                                        match serde_json::from_str::<ReadToolResultArgs>(&call.arguments) {
                                            Ok(arguments) => match runtime::SpillHandle::parse(&arguments.handle) {
                                                None => tools::ToolOutput::refused("handle_invalid: expected t:<tool>:<call8>:<digest8>".to_owned()),
                                                Some(handle) if handle.call_prefix == "00000000" && handle.digest_prefix == "00000000" => tools::ToolOutput::refused("handle_placeholder: t:<tool>:<call8>:<digest8> is the example shape; copy the handle from the …[qq: … t:…]… marker of the result you want".to_owned()),
                                                Some(handle) => match spills.read(handle).await {
                                                    Ok(runtime::SpillRead::Found { text, .. }) => {
                                                        match render_tool_result(&arguments.handle, &arguments, &text) {
                                                            Ok(page) => tools::ToolOutput::exact(page, &runtime::READ_TOOL_RESULT_BOUNDS),
                                                            Err(error) => tools::ToolOutput::refused(error),
                                                        }
                                                    }
                                                    Ok(runtime::SpillRead::Missing) => tools::bounded_result(
                                                        format!("spill_missing: no stored output for {}", arguments.handle),
                                                        true,
                                                    ),
                                                    Ok(runtime::SpillRead::Evicted) => tools::bounded_result(
                                                        format!("spill_evicted: the stored output for {} was reclaimed by the session's 64 MiB cap", arguments.handle),
                                                        true,
                                                    ),
                                                    Ok(runtime::SpillRead::ForeignSession) => tools::ToolOutput::refused("handle_foreign_session: stored outputs are readable only by the session that produced them".to_owned()),
                                                    Err(error) => tools::ToolOutput::unobserved(error),
                                                },
                                            },
                                            Err(error) => tools::ToolOutput::refused(format!("invalid arguments: {error}")),
                                        }
                                    }
                                    None if host == Some(catalog::ToolHost::LoadSkill) => {
                                        match serde_json::from_str::<workspace::skills::LoadSkillArgs>(&call.arguments) {
                                            Ok(arguments) => match workspace::load_disclosed_skill(
                                                workspace,
                                                pack_roots,
                                                skills,
                                                cancelled,
                                                arguments.name.trim().to_owned(),
                                                arguments.arguments,
                                                &tool_tasks,
                                            )
                                            .await
                                            {
                                                Ok(guidance) => {
                                                    tools::bounded_result(guidance.render_for_tool(), false)
                                                }
                                                Err(error) => tools::bounded_result(error.to_string(), true),
                                            },
                                            Err(error) => tools::ToolOutput::refused(format!("invalid arguments: {error}")),
                                        }
                                    }
                                    None if let Some(catalog::ToolHost::External { .. }) = host
                                        && !offered =>
                                    {
                                        tools::ToolOutput::refused(
                                            format!(
                                                "{} is not available in this run yet; call {} with keywords \
                                                 describing it first",
                                                call.name, catalog::SELECT_TOOLS_TOOL
                                            ),
                                        )
                                    }
                                    None if let Some(catalog::ToolHost::External { host: index }) = host => {
                                        let outcome = match hosts[index].call_observed(
                                            call.name.clone(), call.arguments.clone(), cancelled,
                                        ) {
                                            hosts::HostCallObservation::Rendered(future) => future.await.map(|result| (result, None)),
                                            hosts::HostCallObservation::Observed(future) => future.await.map(|observed| (observed.result, observed.result_digest)),
                                        };
                                        match outcome {
                                            Ok((outcome, identity)) => {
                                                let mut result = tools::bounded_result(outcome.content, outcome.is_error);
                                                if let Some(identity) = identity { result.result_digest = identity; }
                                                result
                                            }
                                            Err(error) => hosts::host_error_result(&error),
                                        }
                                    }
                                    None => {
                                        tools::execute_with_images(
                                            workspace,
                                            file_state,
                                            call.name.clone(),
                                            call.arguments.clone(),
                                            cancelled,
                                            output,
                                            tool_tasks,
                                            shell_policy,
                                            network_policy,
                                            approved_read_images,
                                            process_producer,
                                            Some(call.id),
                                        )
                                        .await
                                    }
                                };
                                (call, result, child_spend)
                            }.instrument(tool_span)
                        };
                        let overlaps = |call: &RuntimeToolCall| {
                            !(bounded_child_spend && catalog.lookup(&call.name).is_some_and(|entry| entry.host == catalog::ToolHost::SpawnAgent))
                            && !catalog.lookup(&call.name).is_some_and(|entry| {
                                matches!(entry.host, catalog::ToolHost::WaitAgents | catalog::ToolHost::CancelAgent | catalog::ToolHost::CommandStatus | catalog::ToolHost::UpdateGoal | catalog::ToolHost::SelectTools)
                            })
                            && matches!(
                                approval::classify(call.effect, &call.name, &call.arguments, network_policy),
                                approval::ToolClass::ReadOnly
                            )
                        };
                        let mut cursor = 0_usize;
                        let mut serial_suffix = false;
                        while cursor < calls.len() && !tool_scope.interrupted {
                            let call = &calls[cursor];
                            let admission_key = run.admission_key(call.id);
                            if run.loop_guard.no_progress() {
                                for (index, remaining) in calls.iter().enumerate().skip(cursor) {
                                    if results[index].is_none() {
                                        let message = "not executed: run paused after 512 quiet tool attempts without progress".to_owned();
                                        results[index] = Some(RetainedResult::error(message.clone()));
                                        yield TurnStep::Emit(RuntimeEvent::ToolCallUnexecuted { id: remaining.id, message, interrupted: false });
                                    }
                                }
                                break;
                            }
                            if let Some(result) = &results[cursor] {
                                if result.human_input { run.stall.progress(); } else { run.stall.settled(false); }
                                run.observe_tool(call, result, network_policy, catalog.lookup(&call.name));
                                cursor += 1;
                                continue;
                            }
                            if let Some(rejection) = &call.rejection {
                                let message = rejection.clone().into_message();
                                let result = RetainedResult::error(message.clone());
                                run.observe_tool(call, &result, network_policy, catalog.lookup(&call.name));
                                results[cursor] = Some(result);
                                yield TurnStep::Emit(RuntimeEvent::ToolCallUnexecuted { id: call.id, message: message.clone(), interrupted: false });
                                cursor += 1;
                                continue;
                            }
                            if admission_key.is_some_and(|key| run.loop_guard.reject(key)) {
                                let message = "not executed: identical tool results repeated without progress; change the approach or inputs".to_owned();
                                let result = RetainedResult::error(message.clone());
                                run.observe_tool(call, &result, network_policy, catalog.lookup(&call.name));
                                run.stall.settled(false);
                                results[cursor] = Some(result);
                                yield TurnStep::Emit(RuntimeEvent::ToolCallUnexecuted { id: call.id, message, interrupted: false });
                                cursor += 1;
                                continue;
                            }
                            let batch_start = cursor;
                            let read_only = overlaps(&calls[cursor]);
                            let reading = !serial_suffix && read_only;
                            if !read_only && catalog.lookup(&calls[cursor].name).map(|entry| entry.host) != Some(catalog::ToolHost::SelectTools) {
                                serial_suffix = true;
                            }
                            let mut keys = Vec::new();
                            let capacity = if reading { usize::from(512_u16.saturating_sub(run.loop_guard.quiet_attempts())).min(MAX_PARALLEL_READS) } else { 1 };
                            while cursor < calls.len() && cursor - batch_start < capacity {
                                let call = &calls[cursor];
                                let key = run.admission_key(call.id).unwrap_or_else(|| CallKey::new(&call.name, &call.arguments));
                                if results[cursor].is_some() || call.rejection.is_some()
                                    || (reading && !overlaps(call)) || keys.contains(&key)
                                    || (run.admission_key(call.id).is_some() && run.loop_guard.reject(key)) {
                                    break;
                                }
                                let index = cursor;
                            let decision = {
                                let interrupt = interrupt_requested(steering, run.handled_interrupt);
                                tokio::select! {
                                    biased;
                                    () = interrupt => None,
                                    decision = gate.resolve(call) => Some(decision),
                                }
                            };
                            let Some(decision) = decision else {
                                tool_scope.interrupted = true;
                                break;
                            };
                            match decision {
                                GateDecision::Execute => {}
                                GateDecision::Deny { message } => {
                                    results[index] = Some(RetainedResult::error(message.clone()));
                                    yield TurnStep::Emit( RuntimeEvent::ToolCallDenied { id: call.id, message });
                                }
                                GateDecision::Answered { result } => {
                                    results[index] = Some(RetainedResult::answered(result.clone()));
                                    yield TurnStep::Emit( RuntimeEvent::ToolCallAnswered { id: call.id, result });
                                }
                                GateDecision::Fail { kind, message } => {
                                    yield TurnStep::Emit( RuntimeEvent::Failed { kind, message });
                                    yield TurnStep::Stop; return;
                                }
                            }
                                if results[index].is_some() { break; }
                                keys.push(key);
                                cursor += 1;
                            }
                            if tool_scope.interrupted { break; }
                            if cursor == batch_start { continue; }
                            if reading {
                                let child_limits = run.budget.child_budget(tokio::time::Instant::now());
                                let overlapped = calls[batch_start..cursor].iter().map(|call| {
                                    (call.clone(), catalog.exposure() == catalog::Exposure::Full || run.pins.names().contains(&call.name))
                                }).collect::<Vec<_>>();
                                for (admitted, _) in &overlapped {
                                    started_calls[usize::from(admitted.call_ordinal - 1)] = true;
                                    yield TurnStep::Emit(RuntimeEvent::ToolCallStarted { id: admitted.id, may_change_files: false });
                                }
                            let mut executions = futures_stream::iter(
                                overlapped.into_iter().map(|(call, offered)| execute_one(call, None, child_limits, offered)),
                            )
                                .buffer_unordered(MAX_PARALLEL_READS);
                            loop {
                                let interrupt = interrupt_requested(steering, run.handled_interrupt);
                                let next = tokio::select! {
                                    biased;
                                    () = interrupt => {
                                        tool_scope.interrupted = true;
                                        break;
                                    }
                                    next = executions.next() => next,
                                };
                                let Some((call, result, child_spend)) = next else {
                                    break;
                                };
                                if let Err(error) = tool_tasks.check() {
                                    yield TurnStep::Emit( RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() });
                                    yield TurnStep::Stop; return;
                                }
                                if let Some(spend) = child_spend {
                                    run.budget.charge_child(spend.usage, spend.cost_usd_nanos);
                                    if let Some(spawner) = &spawner { spawner.acknowledge(call.id); }
                                }
                                note_audited_action(
                                    &mut run.audit_triggers,
                                    &mut run.audit_actions,
                                    audit_hook.is_some(),
                                    &call,
                                    &result,
                                );
                                if call.rejection.is_none() {
                                    let entry = catalog.lookup(&call.name);
                                    let receipt = entry.is_some_and(|entry| entry.host == catalog::ToolHost::SpawnAgent)
                                        && child_spend.is_none();
                                    run.stall.settled(!receipt && runtime::is_progress(&call, entry, &result));
                                }
                                let mut result = result;
                                run.hint_repeated_argument_error(&call.name, &mut result);
                                let result = cite_spill(result, &call.name, call.id, spills.is_some());
                                results[usize::from(call.call_ordinal - 1)] =
                                    Some(RetainedResult::retain(&result, &call.name, call.id));
                                yield TurnStep::Emit( RuntimeEvent::ToolCallFinished {
                                    id: call.id,
                                    result: result.model_text,
                                    is_error: result.is_error,
                                    file_states: result.file_states,
                                    display: result.ui_payload,
                                    spill: result.spill,
                                    execution_context: result.execution_context,
                                });
                            }
                            for index in batch_start..cursor {
                                if let Some(result) = &results[index] {
                                    run.observe_tool(&calls[index], result, network_policy, catalog.lookup(&calls[index].name));
                                }
                            }
                            continue;
                            }
                            let call = calls[batch_start].clone();
                            started_calls[usize::from(call.call_ordinal - 1)] = true;
                            yield TurnStep::Emit(RuntimeEvent::ToolCallStarted { id: call.id, may_change_files: may_change_files(&call, network_policy, catalog.lookup(&call.name)) });
                            if catalog.lookup(&call.name).map(|entry| entry.host) == Some(catalog::ToolHost::SelectTools) {
                                let tool_span = tracing::info_span!(target: "qq::diagnostic", "tool", call_ordinal = call.call_ordinal);
                                let (result, changed) = tool_span.in_scope(|| select_tools(catalog, &mut run.pins, &call.arguments, static_filter.as_ref().expect("selector requires tools")));
                                run.stall.settled(false);
                                let retained = RetainedResult::retain(&result, &call.name, call.id);
                                run.observe_tool(&call, &retained, network_policy, catalog.lookup(&call.name));
                                results[usize::from(call.call_ordinal - 1)] = Some(retained);
                                if changed {
                                    run.tool_specs = catalog.specs_with_pins(base_specs, &run.pins, static_filter.as_ref().expect("pins require tools"));
                                    run.tool_schema = catalog.schema_measurement(&run.tool_specs);
                                }
                                drop(tool_span);
                                yield TurnStep::Emit(RuntimeEvent::ToolCallFinished { id: call.id, result: result.model_text, is_error: result.is_error, file_states: Vec::new(), display: result.ui_payload, spill: None, execution_context: None, });
                                continue;
                            }

                                if catalog.lookup(&call.name).map(|entry| entry.host) == Some(catalog::ToolHost::UpdateGoal) {

                                    let tool_span = tracing::info_span!(target: "qq::diagnostic", "tool", call_ordinal = call.call_ordinal);
                                    let index = usize::from(call.call_ordinal - 1);
                                    if steering.as_ref().is_some_and(|steering| *steering.interrupts.borrow() > run.handled_interrupt) {
                                        tool_scope.interrupted = true;
                                        results[index] = Some(RetainedResult::interrupted(INTERRUPTED_TOOL_RESULT.to_owned()));
                                        drop(tool_span);
                                        yield TurnStep::Emit(RuntimeEvent::ToolCallFinished {
                                            id:call.id, result:INTERRUPTED_TOOL_RESULT.to_owned(), is_error:true,
                                            file_states:Vec::new(), display:None, spill:None,
                                         execution_context: None, });
                                        break;
                                    }
                            let result = match serde_json::from_str::<tools::goal::UpdateGoalArgs>(&call.arguments) {
                                Err(error) => tools::ToolOutput::refused(format!("invalid arguments: {error}")),
                                Ok(arguments) => match goal {
                                    None => tools::ToolOutput::refused("goal_not_claimed: this run has no goal capability"),
                                    Some(goal) => {
                                        let interrupt = interrupt_requested(steering, run.handled_interrupt);
                                        let admitted = tokio::select! {
                                            biased;
                                            () = cancelled.cancelled() => { yield TurnStep::Stop; return; },
                                            () = runtime::RunDeadline::wait(*deadline) => {
                                                yield TurnStep::Emit(RuntimeEvent::BudgetExhausted { exhaustion: deadline.expect("only a finite deadline wakes").exhaustion() });
                                                yield TurnStep::Stop; return;
                                            },
                                            () = interrupt => None,
                                            receipt = goal.admit_update(arguments).instrument(tool_span.clone()) => Some(receipt),
                                        };
                                        match admitted {
                                            None => {
                                                tool_scope.interrupted = true;
                                                tools::ToolOutput::interrupted(INTERRUPTED_TOOL_RESULT.to_owned())
                                            }
                                            Some(admitted) => {
                                                let updated = match admitted {
                                                    Err(error) => Err(error),
                                                    Ok(receipt) => match receipt.instrument(tool_span.clone()).await {
                                                        Ok(updated) => updated,
                                                        Err(_) => Err(sessions::SessionRuntimeError::Unavailable),
                                                    },
                                                };
                                                match updated {
                                        Err(error) => {
                                            yield TurnStep::Emit(RuntimeEvent::Failed { kind:RunFailureKind::Server, message:format!("persist goal update: {error}") });
                                            yield TurnStep::Stop; return;
                                        }
                                        Ok(updated) => {
                                            if updated.checklist_changed { run.loop_guard.mutation(); }
                                            match serde_json::to_string(&updated) {
                                            Ok(text) => {
                                                if updated.added == 0 && updated.changed == 0 && updated.dropped == 0 && !updated.notes_applied && !updated.status_applied && !updated.wait_applied && !updated.rejected.is_empty() {
                                                    tools::ToolOutput::refused(text)
                                                } else {
                                                    tools::bounded_result(text, !updated.rejected.is_empty())
                                                }
                                            },
                                            Err(error) => {
                                                yield TurnStep::Emit(RuntimeEvent::Failed { kind:RunFailureKind::Server, message:format!("encode goal update result: {error}") });
                                                yield TurnStep::Stop; return;
                                            }
                                        }
                                        }

                                                }
                                            }
                                        }
                                    }
                                }
                            };
                            let mut result = result;
                            run.hint_repeated_argument_error(&call.name, &mut result);
                            run.stall.settled(!result.is_error);
                            run.observe_tool(&call, &RetainedResult::retain(&result, &call.name, call.id), network_policy, catalog.lookup(&call.name));
                            results[index] = Some(RetainedResult::retain(&result, &call.name, call.id));
                            drop(tool_span);
                            yield TurnStep::Emit(RuntimeEvent::ToolCallFinished {
                                id:call.id, result:result.model_text, is_error:result.is_error,
                                file_states:Vec::new(), display:result.ui_payload, spill:None,
                             execution_context: None, });
                                    if tool_scope.interrupted {
                                        break;
                                    }
                                    continue;
                                }
                                let call_id = call.id;
                                let mut call_id_holder = Some(call.clone());
                                let (delta_sender, mut deltas) =
                                    tokio::sync::mpsc::channel::<String>(SHELL_OUTPUT_QUEUE_CAPACITY);
                                let offered = catalog.exposure() == catalog::Exposure::Full || run.pins.names().contains(&call.name);
                                let mut execution = Box::pin(execute_one(call, Some(delta_sender), run.budget.child_budget(tokio::time::Instant::now()), offered));
                                let mut execution_scope = ExecutionScope { output_closed: false };
                                let (call, result, child_spend) = loop {
                                    let interrupt = interrupt_requested(steering, run.handled_interrupt);
                                    tokio::select! {
                                        biased;
                                        () = interrupt => {
                                            drop(execution);
                                            if let Err(error) = tool_tasks.drain_calls().await {
                                                yield TurnStep::Emit( RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() });
                                                yield TurnStep::Stop; return;
                                            }
                                            if let Some(spawner) = &spawner {
                                                match spawner.drain_attached().await {
                                                    Ok(spends) => for spend in spends { run.budget.charge_child(spend.usage, spend.cost_usd_nanos); },
                                                    Err(error) => {
                                                        yield TurnStep::Emit( RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() });
                                                        yield TurnStep::Stop; return;
                                                    }
                                                }
                                            }
                                            break (call_id_holder.take().expect("call retained"), tools::ToolOutput::interrupted(INTERRUPTED_TOOL_RESULT.to_owned()), None);
                                        }
                                        chunk = deltas.recv(), if !execution_scope.output_closed => match chunk {
                                            Some(chunk) => {
                                                yield TurnStep::Emit( RuntimeEvent::ToolCallOutputDelta { id: call_id, chunk });
                                            }
                                            None => execution_scope.output_closed = true,
                                        },
                                        completed = &mut execution => {
                                            drop(execution);
                                            break completed;
                                        },
                                    }
                                };
                                if let Err(error) = tool_tasks.check() {
                                    yield TurnStep::Emit( RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() });
                                    yield TurnStep::Stop; return;
                                }
                                let interrupted_here = result.model_text == INTERRUPTED_TOOL_RESULT && result.is_error && result.file_states.is_empty() && steering.as_ref().is_some_and(|steering| *steering.interrupts.borrow() > run.handled_interrupt);
                                while let Ok(chunk) = deltas.try_recv() {
                                    yield TurnStep::Emit( RuntimeEvent::ToolCallOutputDelta { id: call_id, chunk });
                                }
                                if let Some(spend) = child_spend {
                                    run.budget.charge_child(spend.usage, spend.cost_usd_nanos);
                                    if let Some(spawner) = &spawner { spawner.acknowledge(call.id); }
                                }
                                note_audited_action(
                                    &mut run.audit_triggers,
                                    &mut run.audit_actions,
                                    audit_hook.is_some(),
                                    &call,
                                    &result,
                                );
                                if call.rejection.is_none() {
                                    let entry = catalog.lookup(&call.name);
                                    let receipt = entry.is_some_and(|entry| entry.host == catalog::ToolHost::SpawnAgent)
                                        && child_spend.is_none();
                                    run.stall.settled(!receipt && runtime::is_progress(&call, entry, &result));
                                }
                                run.observe_tool(&call, &RetainedResult::retain(&result, &call.name, call.id), network_policy, catalog.lookup(&call.name));
                                let mut result = result;
                                run.hint_repeated_argument_error(&call.name, &mut result);
                                let result = cite_spill(result, &call.name, call.id, spills.is_some());
                                results[usize::from(call.call_ordinal - 1)] =
                                    Some(RetainedResult::retain(&result, &call.name, call.id));
                                yield TurnStep::Emit( RuntimeEvent::ToolCallFinished {
                                    id: call.id,
                                    result: result.model_text,
                                    is_error: result.is_error,
                                    file_states: result.file_states,
                                    display: result.ui_payload,
                                    spill: result.spill,
                                    execution_context: result.execution_context,
                                });
                                if interrupted_here {
                                    tool_scope.interrupted = true;
                                    break;
                                }
                        }
                        if tool_scope.interrupted {
                            if let Err(error) = tool_tasks.drain_calls().await {
                                                yield TurnStep::Emit( RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() });
                                                yield TurnStep::Stop; return;
                                            }
                            if let Some(spawner) = &spawner {
                                match spawner.drain_attached().await {
                                    Ok(spends) => for spend in spends { run.budget.charge_child(spend.usage, spend.cost_usd_nanos); },
                                    Err(error) => {
                                        yield TurnStep::Emit( RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() });
                                        yield TurnStep::Stop; return;
                                    }
                                }
                            }
                            run.handled_interrupt = steering
                                .as_ref()
                                .map_or(run.handled_interrupt, |steering| *steering.interrupts.borrow());
                            for (index, call) in calls.iter().enumerate() {
                                if results[index].is_none() {
                                    if started_calls[index] {
                                        let result = tool_tasks.finished_result(call.id)
                                            .unwrap_or_else(|| tools::ToolOutput::interrupted(INTERRUPTED_TOOL_RESULT.to_owned()));
                                        let result = cite_spill(result, &call.name, call.id, spills.is_some());
                                        results[index] = Some(RetainedResult::retain(&result, &call.name, call.id));
                                        yield TurnStep::Emit(RuntimeEvent::ToolCallFinished {
                                            id: call.id,
                                            result: result.model_text,
                                            is_error: result.is_error,
                                            file_states: result.file_states,
                                            display: result.ui_payload,
                                            spill: result.spill,
                                            execution_context: result.execution_context,
                                        });
                                    } else {
                                        results[index] = Some(RetainedResult::interrupted(INTERRUPTED_TOOL_RESULT.to_owned()));
                                        yield TurnStep::Emit(RuntimeEvent::ToolCallUnexecuted { id: call.id, message: INTERRUPTED_TOOL_RESULT.to_owned(), interrupted: true });
                                    }
                                }
                            }
                            yield TurnStep::Emit( RuntimeEvent::Interrupted { turn_ordinal });
                        }

        yield TurnStep::Ready(ToolOutcomes { turn_ordinal, final_answer_turn, calls, results });
                }
    }
    fn close_tool_turn(
        &mut self,
        outcomes: ToolOutcomes,
    ) -> impl futures_util::Stream<Item = TurnStep<()>> + Send + '_ {
        stream! {
                    let Self { env, messages, steering, run, window, .. } = self;
                    let RunEnvironment { plan, file_state, spills, .. } = env;
                    let workspace = &plan.workspace;

        let ToolOutcomes { turn_ordinal, final_answer_turn, calls, results } = outcomes;
                        let mut output_scope = ResultScope { budget: tools::TurnOutputBudget::for_results(
                            calls.iter().zip(results.iter()).map(|(call, result)| (call.name.as_str(), result.as_ref().expect("all outcomes retained").model_text.as_str()))) };
                        let stored = spills.is_some();
                        let result_blocks = calls
                            .iter()
                            .zip(results.into_iter())
                            .map(|(call, result)| {
                                let result = result.expect("every bounded tool execution completed");
                                let mut content = result.model_text;
                                let recall = match (result.spill_handle.as_deref(), stored) {
                                    (Some(handle), _) => tools::ResultRecall::Spill(handle),
                                    (None, true) => tools::ResultRecall::StoredResult {
                                        tool: &call.name,
                                        call: call.id,
                                    },
                                    (None, false) => tools::ResultRecall::None,
                                };
                                output_scope.budget.admit(&call.name,&mut content, recall);
                                run.budget.charge_tool_output(content.len());
                                ContentBlock::ToolResult {
                                    call_id: call.provider_call_id.clone(),
                                    content,
                                    is_error: result.is_error,
                                }
                            })
                            .collect();
                        let projection = Arc::new(runtime::ProjectedToolTurn {
                            message: Message::tool_results(result_blocks),
                        });
                        yield TurnStep::Emit(RuntimeEvent::ToolResultsProjected {
                            turn_ordinal,
                            projection: Arc::clone(&projection),
                        });
                        let runtime::ProjectedToolTurn { message: tool_results } =
                            Arc::unwrap_or_clone(projection);
                        window.irreducible_message_bytes = window.irreducible_message_bytes
                            .saturating_add(measure_message(&tool_results));
                        Arc::make_mut(messages).push(tool_results);
                        if final_answer_turn {
                            if let Some(kind) = run.budget.exceeded(tokio::time::Instant::now())
                                && matches!(
                                    kind,
                                    BudgetLimitKind::Cost
                                        | BudgetLimitKind::CostUnknown
                                        | BudgetLimitKind::TotalTokens
                                )
                            {
                                let exhaustion = run.budget.exhaustion(kind, false, tokio::time::Instant::now());
                                yield TurnStep::Emit( RuntimeEvent::BudgetExhausted { exhaustion });
                                yield TurnStep::Stop; return;
                            }
                            yield TurnStep::Emit( RuntimeEvent::Completed { final_output: None });
                            yield TurnStep::Stop; return;
                        }
                        if let Some(applied) = apply_steering(&mut run.stall, steering, Arc::make_mut(messages), &mut window.irreducible_message_bytes, workspace, file_state).await {
                        run.loop_guard.mutation();
                            for steer in applied {
                                yield TurnStep::Emit( RuntimeEvent::SteeringApplied {
                                    message_id: steer.message_id,
                                    turn_ordinal: turn_ordinal.saturating_add(1),
                                    attachments: steer.attachments,
                                });
                            }
                        }

                        if run.loop_guard.no_progress() {
                            yield TurnStep::Emit(RuntimeEvent::Paused { pause: Box::new(qq_protocol::RunPause::NoProgress {
                                turn_ordinal, slices: run.slices, calls: run.loop_guard.eligible_attempts(),
                            }) });
                            yield TurnStep::Stop; return;
                        }
        yield TurnStep::NextTurn;
                }
    }
}
