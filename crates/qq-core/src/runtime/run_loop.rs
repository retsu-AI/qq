// Execution stays pull-driven: dropping the stream drops its in-flight work.
use crate::*;

impl plan::CompiledAgentPlan {
    /// Executes one run from this plan. No filesystem discovery happens
    /// before the first provider request unless the prompt invokes a command
    /// or skill, whose document is then read from the already opened
    /// workspace.
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
        // The transcript is shared with each turn's request by reference
        // count; once the provider stream is dropped the run is the only
        // holder and appends in place (`Arc::make_mut` copies nothing).
        let mut messages = Arc::new(messages);
        let provider = Arc::clone(&plan.runtime.provider);
        let model = Arc::clone(&plan.runtime.model);
        let model_max_output_tokens = plan.runtime.max_output_tokens;
        // The empty-truncation raise may go past the configured cap up to the
        // catalog ceiling; never below the configured cap itself.
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
        let hosts = Arc::clone(&plan.hosts);
        let context_sources = Arc::clone(&plan.runtime.context_sources);
        let context_cache = Arc::clone(&plan.runtime.context_cache);
        let profile_name = plan.descriptor().profile.as_str().to_owned();
        let delegation = Arc::clone(&plan.runtime.delegation);
        let shell_policy = Arc::clone(&plan.runtime.shell);
        let network_policy = Arc::clone(&plan.runtime.network);
        let checkpoint = plan.runtime.checkpoint.clone();
        let reasoning_effort = plan.runtime.reasoning_effort;
        let turn_recovery = plan.runtime.turn_recovery;
        let events: RuntimeStream = Box::pin(stream! {
            let RunCapabilities {
                spawner,
                allow_guidance,
                slash_is_literal,
                allow_tools,
                read_only,
                max_output_tokens,
                limits,
                routing_spend,
                execution_started: _,
                pricing,
                history,
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
            } = capabilities;
            let tool_tasks = tool_tasks.unwrap_or_default();
            let mut steering = steering;
            let mut handled_interrupt = steering
                .as_ref()
                .map_or(0, |steering| *steering.interrupts.borrow());
            let mut max_output_tokens = max_output_tokens
                .unwrap_or(model_max_output_tokens)
                .min(model_max_output_tokens);
            // Empty truncations raise the cap toward `output_ceiling` once
            // per run: the catalog limit (policy-bounded), which is above the
            // configured cap whenever the catalog knows one.
            let mut empty_output_retries = 0_u16;
            // Consecutive truncated turns that produced nothing at all (not
            // even a complete tool call). Only these are "spent on
            // reasoning" in the terminal diagnostic; a raise taken for a
            // call-then-cut turn consumes the allowance but not this count.
            let mut reasoning_only_truncations = 0_u16;
            // The session owner supplies the original execution admission,
            // including time spent loading or automatically compacting.
            let mut budget = BudgetMeter::new(limits, pricing, started);
            if let Some(spend) = routing_spend {
                budget.charge_child(spend.usage, spend.estimated_cost_usd_nanos);
            }
            let _cancel_on_drop = CancelOnDrop(cancelled.clone());
            yield RuntimeEvent::Started;

            // Completed turns can persist an assistant message with no model-visible
            // content (reasoning-only or a call-less empty completion). Live turns
            // already skip those; reconstructed history must too, or a follow-up
            // on a finished session fails before the provider is reached.
            messages = Arc::new(usable_conversation(Arc::unwrap_or_clone(messages)));
            if messages.is_empty() {
                yield RuntimeEvent::Failed {
                    kind: RunFailureKind::InvalidCommand,
                    message: "conversation messages must not be empty".to_owned(),
                };
                return;
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
                    yield RuntimeEvent::Failed {
                        kind: RunFailureKind::InvalidCommand,
                        message: error.to_string(),
                    };
                    return;
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
                        yield RuntimeEvent::Failed {
                            kind: RunFailureKind::InvalidCommand,
                            message: error.to_string(),
                        };
                        return;
                    }
                    Err(error) => {
                        yield RuntimeEvent::Failed {
                            kind: RunFailureKind::Configuration,
                            message: error.to_string(),
                        };
                        return;
                    }
                },
            };
            // Context sources run once, after guidance and before any
            // provider work, each under its own deadline. Their output is
            // appended to this run's system prompt only; nothing durable
            // changes. A fail-closed failure settles the run here.
            let mut context_blocks = String::new();
            let mut context_records = Vec::new();
            // A summarizer reads no context sources: its system prompt is the
            // session's plan-constant prefix, which is what the provider
            // cached (ADR-0056 § 5).
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
                        yield RuntimeEvent::Failed {
                            kind: RunFailureKind::ContextSource,
                            message,
                        };
                        return;
                    }
                }
            }
            // The plan's catalog is the tool list: the static tools this run
            // may use (the sub-agent tool only when it may spawn, recall only
            // for durable session runs) plus every external tool under full
            // exposure. Under progressive exposure the model pins external
            // tools with `select_tools`; pins extend this base list.
            let prefix_key = match summarizer {
                Some(key) => key,
                None => plan::PromptPrefixKey {
                    tools: allow_tools.then_some(catalog::StaticFilter {
                        spawn_agent: spawner.is_some(),
                        search_history: history.is_some(),
                        read_tool_result: spills.is_some(),
                        load_skill: allow_guidance,
                        read_only,
                    }),
                    guidance: allow_guidance,
                    subagent,
                },
            };
            let static_filter = prefix_key.tools;
            let allow_tools = static_filter.is_some();
            let base_specs: Arc<[ToolSpec]> = match &static_filter {
                Some(filter) => catalog.base_specs(filter),
                None => Arc::from([]),
            };
            let mut pins = catalog::PinSet::default();
            // A recovered run re-pins what its earlier `select_tools` calls
            // pinned, so the resumed request offers the same schemas.
            if allow_tools && catalog.exposure() == catalog::Exposure::Progressive {
                recover_pins(&messages, &catalog, &mut pins);
            }
            let mut tool_specs: Arc<[ToolSpec]> = if pins.is_empty() {
                Arc::clone(&base_specs)
            } else {
                catalog.specs_with_pins(&base_specs, &pins)
            };
            // The plan-constant prefix is built once per capability set and
            // its SHA-256 state continued over this run's suffix, so neither
            // the prompt body nor its hash is recomputed per run.
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
            let mut tool_schema = catalog.schema_measurement(&tool_specs);
            let mut prompt_identity = Some(Arc::new(RunPromptIdentity {
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
            // Only the transcript preceding the accepted prompt can be
            // replaced by a between-run compaction. Everything appended by
            // this run is irreducible until the run settles.
            let reducible_messages = messages.len().saturating_sub(1);
            let mut reducible_message_bytes = measure_messages(&messages[..reducible_messages]);
            let mut irreducible_message_bytes =
                measure_messages(&messages[reducible_messages..]);
            // The last provider-measured request as (system bytes, tool
            // schema bytes, message bytes, measured input tokens). The next
            // request's estimate starts from the measurement and follows each
            // component's byte delta, so a checkpoint notice, a budget-final
            // turn, or an in-run prune adjusts the chain instead of dropping
            // it back to the raw byte estimate.
            let mut compatible_request: Option<(u64, u64, u64, u64)> = None;
            // Effects of this run's admitted calls by provider call id, so a
            // turn that would overflow the window can stub the stale
            // read-only results in memory before failing. Results the run
            // inherited carry their stored effects (`inherited_effects`).
            let mut call_effects = HashMap::<String, catalog::EffectClass>::new();

            let mut slice_tool_calls = 0_usize;
            // Calls since the run last produced output (ADR-0054 § 1). Only
            // a run that can call tools has anything to report.
            let mut stall = runtime::StallScope::new(if stall_exempt || !allow_tools || summarizer.is_some() {
                runtime::StallPolicy::Exempt
            } else if subagent.is_some() {
                runtime::StallPolicy::Subagent
            } else {
                runtime::StallPolicy::Root
            });
            let mut output_continuations = 0_u16;
            // Retries spent on the current turn's transient provider faults;
            // a completed turn resets it.
            let mut turn_retries = 0_u16;
            // What this run did, for the heuristic audit trigger and the
            // auditor's action summary. Only roots with a hook keep actions.
            let mut audit_triggers = runtime::AuditTriggers::default();
            let mut audit_actions: Vec<runtime::AuditedAction> = Vec::new();
            let mut audit_revisions = 0_u16;
            let mut checkpoint_context = checkpoint.as_ref().map(|_| runtime::CheckpointContext::new(&messages));
            // Repair turns spent against the output contract, for the whole
            // run: neither an audit revision nor steering resets them.
            let mut output_repairs = 0_u8;
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
            let mut model_text_bytes = 0_usize;
            let mut continuing_slice = false;
            // The report or final-answer notice already in the conversation
            // and not yet answered. It pins the turn's kind: a retried,
            // truncated, or interrupted attempt is the same report under the
            // same notice, even when an applied steer has since reset the
            // stall count, and a final-answer turn stays final.
            let mut placed_report: Option<runtime::TurnNotice> = None;
            // The notice placed before the next turn's request, persisted with
            // the first turn row that request produces.
            let mut pending_notice: Option<runtime::TurnNotice> = None;
            // Durable turns already replaced by in-run compaction: the live
            // transcript's assistant messages after the summary are turns
            // `compacted_turns + 1..`, and the next cutoff is durable too.
            let mut compacted_turns: u32 = 0;
            // The provider rejected the previous request for its window even
            // though the estimate said it fit. The provider's verdict is
            // authoritative: the next attempt compacts before sending
            // regardless of the estimate. Granted once per turn ordinal; a
            // second rejection of the same turn fails the run as before.
            let mut provider_overflowed = false;
            let mut reactive_compaction_turn: Option<u32> = None;
            // A summarizer gets one turn of rejected calls to recover with a
            // summary; a second fails closed.
            let mut summarizer_rejected_turns = 0_u8;
            // The turn a wait for sub-agent answers already delivered for.
            let mut wait_delivered_for: Option<u32> = None;
            'turns: for turn_ordinal in 1..=u32::MAX {
                // Settled detached children answer here, at the one boundary
                // every turn passes: after the previous turn's results and
                // steering, before this request is built (ADR-0054 § 4); a
                // child still working may send its newest report. The store
                // commits the delivery before the notice joins context. A run
                // with no background child makes no store call.
                // A wait that just delivered for this turn used its boundary;
                // anything settling since waits for the next one, so one
                // boundary spends one turn's tool-output budget.
                let delivered_by_wait = std::mem::take(&mut wait_delivered_for) == Some(turn_ordinal);
                if let Some(spawner) = &spawner
                    && !delivered_by_wait
                    && spawner.outstanding_detached() > 0
                    && let Err(error) = deliver_children(
                        spawner,
                        Boundary { turn_ordinal, reports: runtime::ReportDelivery::Always },
                        Arc::make_mut(&mut messages),
                        &mut irreducible_message_bytes,
                        &mut budget,
                        &mut stall,
                        checkpoint_context.as_mut(),
                    )
                    .await
                {
                    yield RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() };
                    return;
                }
                // Caller budgets are decided at the turn boundary, before any
                // provider request. A spent work budget grants one final
                // response that asks for no tool calls; a second spent check, an elapsed wall
                // clock, or unmeasurable cost settles the run here.
                let budget_final_turn = match budget.before_turn(
                    tokio::time::Instant::now(),
                    if allow_tools { MAX_TOOL_CALLS_PER_TURN } else { 0 },
                ) {
                    BudgetDecision::Continue => false,
                    BudgetDecision::FinalResponse(_) => true,
                    BudgetDecision::Exhausted(exhaustion) => {
                        yield RuntimeEvent::BudgetExhausted { exhaustion };
                        return;
                    }
                };
                // Reserve enough capacity for the largest valid provider
                // turn. Without this reservation, a slice at (for example)
                // 255 calls could accept a 16-call turn and overshoot its
                // strict ceiling before reaching the next turn boundary.
                // The checkpoint turn is persisted but is not the run's
                // terminal outcome; the next turn starts a new slice. Tools
                // stay declared so a model that calls one anyway gets a
                // rejection result rather than a protocol failure.
                let slice_checkpoint = !budget_final_turn
                    && slice_tool_calls
                        .saturating_add(MAX_TOOL_CALLS_PER_TURN)
                        > MAX_TOOL_CALLS_PER_SLICE;
                // A report turn is due at the slice boundary, or after
                // `STALL_REPORT_CALLS` calls that produced nothing. A
                // sub-agent's report turn after enough of them without work
                // is its final answer. The budget-final turn outranks both.
                let report_due = match (budget_final_turn, placed_report) {
                    (true, _) => runtime::ReportDue::None,
                    (false, Some(runtime::TurnNotice::FinalAnswer)) => runtime::ReportDue::FinalAnswer,
                    (false, Some(runtime::TurnNotice::Report | runtime::TurnNotice::StallReport)) => {
                        runtime::ReportDue::Report
                    }
                    (false, Some(runtime::TurnNotice::Continuation) | None) => {
                        stall.due(slice_checkpoint)
                    }
                };
                let final_answer_turn = report_due == runtime::ReportDue::FinalAnswer;
                let checkpoint_turn = report_due == runtime::ReportDue::Report;
                // Which report this is: a pinned one keeps the kind it was
                // asked as; a new one is the slice checkpoint when the slice
                // is full, else a stall report.
                let slice_report = match placed_report {
                    Some(placed) => placed == runtime::TurnNotice::Report,
                    None => slice_checkpoint,
                };
                let continuation_turn = std::mem::take(&mut continuing_slice);
                // The checkpoint and continuation notices join the
                // conversation as runtime messages, so the system prompt and
                // its cached prefix stay the run's own (ADR-0054 § 2).
                // A checkpoint resets the slice, so the next turn is never a
                // checkpoint too; a budget-final turn asks for no tool calls,
                // so it is not told that tools are available again.
                debug_assert!(!(checkpoint_turn && continuation_turn));
                let notice = if (checkpoint_turn || final_answer_turn) && placed_report.is_none() {
                    let notice = if final_answer_turn {
                        runtime::TurnNotice::FinalAnswer
                    } else if slice_report {
                        runtime::TurnNotice::Report
                    } else {
                        runtime::TurnNotice::StallReport
                    };
                    placed_report = Some(notice);
                    Some(notice)
                } else if continuation_turn && !budget_final_turn && !final_answer_turn {
                    Some(runtime::TurnNotice::Continuation)
                } else {
                    None
                };
                if let Some(notice) = notice {
                    let message = Message::user(notice.text());
                    irreducible_message_bytes =
                        irreducible_message_bytes.saturating_add(measure_message(&message));
                    Arc::make_mut(&mut messages).push(message);
                    pending_notice = Some(notice);
                }
                let request_system: Arc<str> = if budget_final_turn {
                    Arc::from(format!("{system}\n\n{BUDGET_FINAL_RESPONSE_NOTICE}"))
                } else {
                    Arc::clone(&system)
                };
                // A budget-final turn keeps its tools declared and asks for
                // none: dropping them is rejected by Bedrock once history
                // holds tool calls, and an unchanged tool block keeps its
                // cache entry. A call the model makes anyway still settles
                // the run (below), so the choice is advice, not the bound.
                let request_has_tools = allow_tools;
                let request_system_hash = if budget_final_turn {
                    ContentHash::from_bytes(Sha256::digest(request_system.as_bytes()).into())
                } else {
                    system_prompt_hash
                };
                let system_bytes = u64::try_from(request_system.len())
                    .unwrap_or(u64::MAX)
                    .saturating_add(CONTEXT_BLOCK_FRAMING_BYTES);
                let tool_schema_bytes = if request_has_tools {
                    tool_schema.bytes
                } else {
                    0
                };
                // The estimate every decision on this turn shares: the
                // provider-measured chain adjusted for each component's byte
                // delta when a compatible measurement exists, else the raw
                // byte ratio. The session layer's admission guard judges the
                // `Prepared` weight with exactly this figure, so recovery
                // (stubbing, in-run compaction) must be triggered by it too;
                // deciding on the raw ratio here while admission used the
                // measured chain let code-heavy runs skip recovery and then
                // fail closed at the guard with zero compaction attempts.
                let estimate_input_tokens = |chain: Option<(u64, u64, u64, u64)>, reducible: u64, irreducible: u64| -> u64 {
                    let message_bytes = reducible.saturating_add(irreducible);
                    match chain {
                        Some((previous_system, previous_tools, previous_messages, measured)) => {
                            // Calibrate against the whole measured request,
                            // then apply each component's delta at that ratio.
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
                                .saturating_add(u64::from(max_output_tokens))
                                > u64::from(window)
                        })
                };
                // Mid-run the transcript cannot be compacted, but read-only
                // results older than the recency window are re-derivable and
                // can be stubbed in place. Do that before a later turn is
                // refused for the window: the same rewrite assembly applies
                // between runs, applied to the live messages. The measured
                // chain credits the removed bytes; it runs only when the
                // estimate says the request would not fit.
                let would_overflow = provider_overflowed
                    || over_window(estimate_input_tokens(
                        compatible_request,
                        reducible_message_bytes,
                        irreducible_message_bytes,
                    ));
                if would_overflow && {
                    // Results the run inherited are classified by their
                    // stored effects, in block order, exactly as assembly
                    // classifies them, so this prune seam replays byte for
                    // byte; a row without one falls back to the built-in
                    // read-only names in both. The run's own results use
                    // the effect each call was admitted under.
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
                                    .unwrap_or_else(|| call_effects.get(call_id).copied())
                            } else {
                                call_effects.get(call_id).copied()
                            };
                            if let Some(effect) = effect {
                                effects.insert((message_index, block_index), effect);
                            }
                        }
                    }
                    sessions::prune_stale_tool_results(
                        Arc::make_mut(&mut messages).as_mut_slice(),
                        &effects,
                    )
                } {
                    reducible_message_bytes = measure_messages(&messages[..reducible_messages]);
                    irreducible_message_bytes = measure_messages(&messages[reducible_messages..]);
                    yield RuntimeEvent::ContextPruned { turn_ordinal };
                }
                // Still over the window after stubbing, or the provider said
                // so itself: summarize this run's own earlier turns and
                // continue. This is a safe boundary —
                // every tool result of the previous turn is durable and in
                // context, nothing is in flight, and steering was applied.
                // Everything but the last `CONTEXT_PRUNE_KEEP_TURNS` turns
                // (and their results) is replaced by one summary message;
                // the prompt and the session context before it stay. A
                // failure here is the same context failure the session layer
                // would have raised, with the compactor's reason attached.
                let provider_rejected_window = provider_overflowed;
                let still_overflows = std::mem::take(&mut provider_overflowed)
                    || over_window(estimate_input_tokens(
                        compatible_request,
                        reducible_message_bytes,
                        irreducible_message_bytes,
                    ));
                if still_overflows && let Some(compactor) = compactor.as_ref() {
                    let run_start = reducible_messages.saturating_add(1);
                    let boundary = sessions::in_run_compaction_boundary(
                        &messages[run_start..],
                        sessions::CONTEXT_PRUNE_KEEP_TURNS,
                    );
                    if let Some((replace_through, replaced_turns)) = boundary {
                        let turn_cutoff = compacted_turns.saturating_add(replaced_turns);
                        // The summarizer sends the request this turn would
                        // have sent, cut at the boundary: the same system
                        // prompt, tools, and message prefix, so it reads the
                        // provider cache the run's own turns wrote. It is
                        // judged on the same estimate as every decision in
                        // this loop. When that says it would not fit with the
                        // summarizer's reserve, or the provider has just
                        // rejected the estimate, the session context before
                        // the prompt is dropped: a cache miss, not a request
                        // over the window.
                        let cut = run_start + replace_through;
                        let summarizer_fits = !provider_rejected_window
                            && plan.runtime.context_window.is_none_or(|window| {
                                estimate_input_tokens(
                                    compatible_request,
                                    reducible_message_bytes,
                                    measure_messages(&messages[reducible_messages..cut]),
                                )
                                .saturating_add(u64::from(
                                    sessions::context::summarizer_output_tokens(
                                        model_max_output_tokens,
                                        Some(window),
                                    ),
                                ))
                                    <= u64::from(window)
                            });
                        let transcript = if summarizer_fits {
                            messages[..cut].to_vec()
                        } else {
                            messages[reducible_messages..cut].to_vec()
                        };
                        // The summarizer is one silent provider turn; this
                        // run's next turn reports `WaitingForProvider` again.
                        yield RuntimeEvent::ActivityChanged {
                            activity: RunActivity::Compacting,
                        };
                        match compactor
                            .compact(runtime::InRunCompactionRequest {
                                transcript,
                                turn_cutoff,
                                system: Arc::clone(&system),
                                tools: Arc::clone(&tool_specs),
                            })
                            .await
                        {
                            Ok(summary) => {
                                let summary = Message::user(format!(
                                    "{}\n\n{}",
                                    sessions::IN_RUN_COMPACTION_PREAMBLE, summary.summary
                                ));
                                let live = Arc::make_mut(&mut messages);
                                live.splice(run_start..run_start + replace_through, [summary]);
                                compacted_turns = turn_cutoff;
                                // The measured chain covered the replaced
                                // turns; the next provider usage re-seeds it.
                                compatible_request = None;
                                irreducible_message_bytes = measure_messages(&messages[reducible_messages..]);
                                yield RuntimeEvent::InRunCompacted { turn_ordinal, turn_cutoff };
                            }
                            Err(error) => {
                                yield match error {
                                    runtime::InRunCompactionError::Persistence(_) => RuntimeEvent::Failed {
                                        kind: RunFailureKind::Server,
                                        message: format!("in-run compaction failed: {error}"),
                                    },
                                    runtime::InRunCompactionError::SummarizerFailed(_)
                                    | runtime::InRunCompactionError::Unavailable(_) => RuntimeEvent::Failed {
                                        kind: RunFailureKind::Policy,
                                        message: format!(
                                            "the context grew past the model window during this run and in-run compaction did not produce a usable smaller context: {error}; run /compact or start a new session, then retry"
                                        ),
                                    },
                                };
                                return;
                            }
                        }
                    }
                }
                let message_bytes = reducible_message_bytes.saturating_add(irreducible_message_bytes);
                // The weight the session layer admits carries the same
                // measured figure the recovery decisions above used, so the
                // guard cannot disagree with the loop about whether this
                // request fits. `None` after an in-run compaction: the chain
                // covered replaced turns and the next usage re-seeds it.
                let compatible_input_tokens = compatible_request.map(|chain| {
                    estimate_input_tokens(Some(chain), reducible_message_bytes, irreducible_message_bytes)
                });
                yield RuntimeEvent::Prepared {
                    turn_ordinal,
                    identity: prompt_identity.take(),
                    static_prefix: PreparedStaticPrefix::new(
                        request_system_hash,
                        request_has_tools.then_some(tool_schema.hash),
                    ),
                    weight: PreparedRequestWeight {
                        max_output_tokens,
                        system_bytes,
                        tool_schema_bytes,
                        reducible_message_bytes,
                        irreducible_message_bytes,
                        compatible_input_tokens,
                    },
                };
                // The provider owns every retry: it resends only while nothing
                // has streamed, so this loop sees each logical turn once and
                // can never duplicate output.
                let request = ModelRequest::new(
                    Arc::clone(&model),
                    Arc::clone(&messages),
                    max_output_tokens,
                );
                let request = match reasoning_effort {
                    Some(effort) => request.with_reasoning_effort(effort),
                    None => request,
                };
                let request = if request_has_tools {
                    let request = request
                        .with_tools(Arc::clone(&tool_specs))
                        .with_system(Arc::clone(&request_system));
                    if budget_final_turn || final_answer_turn {
                        request.with_tool_choice(qq_provider::ToolChoice::None)
                    } else {
                        request
                    }
                } else {
                    request.with_system(Arc::clone(&request_system))
                };
                let mut activity = RunActivity::WaitingForProvider;
                yield RuntimeEvent::ActivityChanged { activity };
                let mut provider_events = provider.stream(request);
                let mut pending_calls = Vec::<PendingToolCall>::new();
                let mut calls_by_provider_id = HashMap::<String, usize>::new();
                let mut replay = None;
                let mut blocks = Vec::<TurnBlock>::new();
                let mut terminal_usage = None;
                let mut completed = false;
                let mut reasoning_bytes = 0_usize;
                let mut open_reasoning = None;
                let mut interrupted_turn = false;
                let mut truncated_turn = false;
                // Why the provider stopped short; `Paused` must be resent as
                // the provider requires, `OutputTokens` may not be worth it.
                let mut truncation_reason = qq_provider::IncompleteReason::OutputTokens;
                let mut streamed_visible_output = false;
                // A transient provider fault. The provider's own ledger
                // (ADR-0005) already resent while nothing had streamed, at
                // sub-minute backoff; a fault that reaches here has outlasted
                // it or arrived mid-stream. The run owns recovery from here
                // (ADR-0040): commit what streamed, re-issue the turn at
                // minute-scale backoff, pause when the allowance is spent.
                let mut turn_fault: Option<(RunFailureKind, String)> = None;
                loop {
                    // An interrupting steer ends the stream here. Text that
                    // already streamed is kept as the partial turn; tool
                    // calls the model had begun are dropped, because their
                    // arguments may be incomplete and nothing has executed.
                    let interrupt = async {
                        match &mut steering {
                            Some(steering) => loop {
                                if *steering.interrupts.borrow() > handled_interrupt {
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
                        event = provider_events.next() => StreamStep::Event(event),
                    };
                    let event = match event {
                        StreamStep::Interrupted => {
                            interrupted_turn = true;
                            completed = true;
                            break;
                        }
                        StreamStep::Event(Some(event)) => event,
                        StreamStep::Event(None) => break,
                    };
                    match event {
                        Ok(ProviderEvent::Replay { data }) => {
                            if replay.is_some() || data.len() > 16 * 1024 * 1024 {
                                yield RuntimeEvent::Failed { kind: RunFailureKind::ProviderProtocol, message: "invalid or oversized provider continuation".to_owned() };
                                return;
                            }
                            replay = Some(data);
                        }
                        Ok(ProviderEvent::ReasoningStarted { kind }) => {
                            if open_reasoning.is_some() {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: "provider started a reasoning block before completing the previous block".to_owned(),
                                };
                                return;
                            }
                            open_reasoning = Some(kind);
                            if activity != RunActivity::Reasoning {
                                activity = RunActivity::Reasoning;
                                yield RuntimeEvent::ActivityChanged { activity };
                            }
                            yield RuntimeEvent::ReasoningStarted { kind };
                        }
                        Ok(ProviderEvent::ReasoningDelta { kind, text }) => {
                            if open_reasoning != Some(kind) {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: "provider streamed reasoning outside its matching block".to_owned(),
                                };
                                return;
                            }
                            if reasoning_bytes.saturating_add(text.len()) > MAX_RUN_REASONING_BYTES {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::Policy,
                                    message: "displayable reasoning exceeded the 1 MiB per-run limit".to_owned(),
                                };
                                return;
                            }
                            reasoning_bytes += text.len();
                            if !text.is_empty() {
                                yield RuntimeEvent::ReasoningDelta { kind, text };
                            }
                        }
                        Ok(ProviderEvent::ReasoningCompleted { kind }) => {
                            if open_reasoning != Some(kind) {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: "provider completed an unknown reasoning block".to_owned(),
                                };
                                return;
                            }
                            open_reasoning = None;
                            yield RuntimeEvent::ReasoningCompleted { kind };
                        }
                        Ok(ProviderEvent::OutputTextDelta { text }) => {
                            if activity != RunActivity::GeneratingResponse {
                                activity = RunActivity::GeneratingResponse;
                                yield RuntimeEvent::ActivityChanged { activity };
                            }
                            if model_text_bytes.saturating_add(text.len()) > MAX_RUN_MODEL_TEXT_BYTES {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::Policy,
                                    message: "model text exceeded the 16 MiB per-run limit".to_owned(),
                                };
                                return;
                            }
                            model_text_bytes += text.len();
                            append_turn_text(&mut blocks, &text);
                            yield RuntimeEvent::OutputTextDelta { text };
                        }
                        Ok(ProviderEvent::RefusalDelta { text }) => {
                            if activity != RunActivity::GeneratingResponse {
                                activity = RunActivity::GeneratingResponse;
                                yield RuntimeEvent::ActivityChanged { activity };
                            }
                            if model_text_bytes.saturating_add(text.len()) > MAX_RUN_MODEL_TEXT_BYTES {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::Policy,
                                    message: "model text exceeded the 16 MiB per-run limit".to_owned(),
                                };
                                return;
                            }
                            model_text_bytes += text.len();
                            append_turn_text(&mut blocks, &text);
                            yield RuntimeEvent::RefusalDelta { text };
                        }
                        Ok(ProviderEvent::ToolCallStarted { id, name }) => {
                            if budget_final_turn {
                                // The model called a tool on the final
                                // response anyway (Bedrock Converse cannot
                                // ask for none). The budget still settles
                                // the run: exhaustion is never a provider
                                // failure, and no more work may be spent.
                                let BudgetDecision::Exhausted(mut exhaustion) = budget
                                    .before_turn(tokio::time::Instant::now(), 0)
                                else {
                                    unreachable!("a requested final response always settles the run")
                                };
                                exhaustion.final_response = false;
                                yield RuntimeEvent::BudgetExhausted { exhaustion };
                                return;
                            }
                            if !request_has_tools {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: "provider requested a tool after the request declared no tools".to_owned(),
                                };
                                return;
                            }
                            if activity != RunActivity::PreparingToolCall {
                                activity = RunActivity::PreparingToolCall;
                                yield RuntimeEvent::ActivityChanged { activity };
                            }
                            if id.is_empty() || id.len() > MAX_TOOL_CALL_ID_BYTES {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: "provider tool call ID is empty or exceeds 1 KiB".to_owned(),
                                };
                                return;
                            }
                            if name.is_empty() || name.len() > MAX_TOOL_NAME_BYTES {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: "provider tool name is empty or exceeds 128 bytes".to_owned(),
                                };
                                return;
                            }
                            if pending_calls.len() >= MAX_ADMITTED_TOOL_CALLS_PER_TURN {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!("model requested more than {MAX_ADMITTED_TOOL_CALLS_PER_TURN} tools in one turn"),
                                };
                                return;
                            }
                            // Past the executable cap, or on the checkpoint
                            // turn, a call is still admitted so the transcript
                            // keeps one result per call, but it settles as a
                            // tool error the model can act on instead of
                            // failing the whole run. Such calls never execute,
                            // so they do not count against the slice or the
                            // run's tool-call budget.
                            let over_cap = pending_calls.len() >= MAX_TOOL_CALLS_PER_TURN;
                            let rejection = if summarizer.is_some() {
                                Some(SUMMARIZER_TOOL_REJECTION.to_owned())
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
                            if calls_by_provider_id.contains_key(&id) {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!("provider reused tool call ID {id:?} in one turn"),
                                };
                                return;
                            }
                            let index = pending_calls.len();
                            calls_by_provider_id.insert(id.clone(), index);
                            pending_calls.push(PendingToolCall {
                                provider_call_id: id,
                                name,
                                arguments: String::new(),
                                completed: false,
                                rejection,
                            });
                            if !over_cap && !checkpoint_turn && !final_answer_turn {
                                slice_tool_calls += 1;
                            }
                            blocks.push(TurnBlock::ToolCall(index));
                        }
                        Ok(ProviderEvent::ToolCallArgumentsDelta { id, json }) => {
                            let Some(index) = calls_by_provider_id.get(&id).copied() else {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!("provider streamed arguments for unknown tool call {id:?}"),
                                };
                                return;
                            };
                            let call = &mut pending_calls[index];
                            if call.completed {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!("provider streamed arguments after completing tool call {id:?}"),
                                };
                                return;
                            }
                            if call.arguments.len().saturating_add(json.len()) > MAX_TOOL_ARGUMENT_BYTES {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!("tool call {id:?} arguments exceed the 64 KiB limit"),
                                };
                                return;
                            }
                            call.arguments.push_str(&json);
                        }
                        Ok(ProviderEvent::ToolCallCompleted { id }) => {
                            let Some(index) = calls_by_provider_id.get(&id).copied() else {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!("provider completed unknown tool call {id:?}"),
                                };
                                return;
                            };
                            let call = &mut pending_calls[index];
                            if call.completed {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!("provider completed tool call {id:?} twice"),
                                };
                                return;
                            }
                            let arguments = if call.arguments.trim().is_empty() {
                                "{}"
                            } else {
                                &call.arguments
                            };
                            // Malformed argument JSON is the model's mistake, not a
                            // run failure: return a retryable tool error instead.
                            let parsed: serde_json::Value = match serde_json::from_str(arguments)
                            {
                                Ok(arguments) => arguments,
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
                            if open_reasoning.is_some() {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: "provider completed the turn with an unfinished reasoning block".to_owned(),
                                };
                                return;
                            }
                            if let Some(call) = pending_calls.iter().find(|call| !call.completed) {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!(
                                        "provider completed the turn before tool call {:?}",
                                        call.provider_call_id
                                    ),
                                };
                                return;
                            }
                            terminal_usage = usage.map(provider_usage);
                            completed = true;
                            break;
                        }
                        Ok(ProviderEvent::Incomplete { usage, reason }) => {
                            // The turn is a valid prefix but the model was
                            // not done. Text stands; any tool call it had
                            // begun carries incomplete arguments and is
                            // dropped, so nothing from this turn executes.
                            // Continuation (or the typed failure) is decided
                            // after the partial turn is committed.
                            if open_reasoning.is_some() {
                                yield RuntimeEvent::ReasoningCompleted {
                                    kind: open_reasoning.take().expect("checked"),
                                };
                            }
                            terminal_usage = usage.map(provider_usage);
                            truncated_turn = true;
                            truncation_reason = reason;
                            completed = true;
                            break;
                        }
                        Err(error) => {
                            let kind = run_failure_kind(error.kind());
                            // The provider's window verdict overrides the
                            // estimate. With turns to compact and a compactor
                            // to do it, re-plan this turn once with compaction
                            // forced; nothing streamed, so nothing is lost.
                            let reactive_compaction = kind
                                == RunFailureKind::ProviderContextExceeded
                                && compactor.is_some()
                                && reactive_compaction_turn != Some(turn_ordinal)
                                && blocks.is_empty()
                                && pending_calls.is_empty()
                                && sessions::in_run_compaction_boundary(
                                    &messages[reducible_messages.saturating_add(1)..],
                                    sessions::CONTEXT_PRUNE_KEEP_TURNS,
                                )
                                .is_some();
                            if reactive_compaction {
                                reactive_compaction_turn = Some(turn_ordinal);
                                provider_overflowed = true;
                                yield RuntimeEvent::ProviderOverflow {
                                    turn_ordinal,
                                    message: error.to_string(),
                                };
                                continue 'turns;
                            }
                            if !recoverable_turn_fault(kind) {
                                yield RuntimeEvent::Failed {
                                    kind,
                                    message: error.to_string(),
                                };
                                return;
                            }
                            if let Some(kind) = open_reasoning.take() {
                                yield RuntimeEvent::ReasoningCompleted { kind };
                            }
                            turn_fault = Some((kind, error.to_string()));
                            completed = true;
                            break;
                        }
                    }
                }

                // The stream holds the transcript; release it before this
                // turn appends so the append never copies.
                drop(provider_events);

                if !completed {
                    // The provider restarts a stream that ends before its
                    // first event; one that ends after events is a transport
                    // fault from the run's point of view: the reply is
                    // incomplete and nothing above the provider may resend
                    // the same stream, but the run may re-issue the turn.
                    if let Some(kind) = open_reasoning.take() {
                        yield RuntimeEvent::ReasoningCompleted { kind };
                    }
                    turn_fault = Some((
                        RunFailureKind::ProviderTransport,
                        "provider stream ended without a terminal event".to_owned(),
                    ));
                } else if turn_fault.is_none()
                    && !interrupted_turn
                    && !truncated_turn
                    && terminal_usage.is_none()
                    && pending_calls.is_empty()
                    && blocks.iter().all(|block| matches!(block, TurnBlock::Text(text) if text.trim().is_empty()))
                    && replay.is_none()
                    && messages
                        .iter()
                        .rev()
                        .take_while(|message| message.role() == Role::User)
                        .any(|message| {
                            message.content().iter().any(|block| matches!(block, ContentBlock::ToolResult { .. }))
                        })
                {
                    // The model was handed fresh tool results and the stream
                    // carried nothing back: no text, no call, no usage. That
                    // is a gateway that swallowed an upstream failure into a
                    // bare terminal event, not an answer, and settling
                    // `completed` would present silence as a finished reply.
                    // Treat it as the transient fault it is: the same bounded
                    // re-issue as a stream cut short. "Nothing" is measured
                    // the way `has_content` measures it: whitespace-only text
                    // would be dropped from the transcript anyway. The results
                    // are found among every user message since the last
                    // assistant turn — a checkpoint notice or steering joins
                    // after them and must not hide them. An empty reply to the
                    // prompt itself (turn one, no results) stays a completion:
                    // the placeholder keeps the transcript well-formed.
                    turn_fault = Some((
                        RunFailureKind::ProviderTransport,
                        "provider completed the turn after tool results with no content and no usage".to_owned(),
                    ));
                }

                if interrupted_turn || truncated_turn || turn_fault.is_some() {
                    if interrupted_turn {
                        handled_interrupt = steering
                            .as_ref()
                            .map_or(handled_interrupt, |steering| *steering.interrupts.borrow());
                    }
                    // Only fully streamed calls could be executed; an interrupt
                    // or truncation executes none, so the partial turn carries
                    // text alone. A call the model did finish streaming is
                    // still visible output: that truncation is continued, not
                    // treated as an all-reasoning turn. A call cut mid-
                    // arguments is not: the resend would carry nothing new.
                    streamed_visible_output = pending_calls.iter().any(|call| call.completed);
                    blocks.retain(|block| matches!(block, TurnBlock::Text(_)));
                    pending_calls.clear();
                }

                compatible_request = terminal_usage.map(|usage| {
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

                let assistant_content = blocks
                    .into_iter()
                    .filter_map(|block| match block {
                        TurnBlock::Text(text) if text.is_empty() => None,
                        TurnBlock::Text(text) => Some(ContentBlock::Text { text }),
                        TurnBlock::ToolCall(index) => {
                            let call = &pending_calls[index];
                            // `arguments` is canonical serde_json output once
                            // the call completed, so it is a valid RawValue.
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
                    // Effect is resolved once here from the catalog and
                    // travels with the call; policy never re-derives it from
                    // the name. A name the catalog does not hold is not
                    // executable, so it settles as a tool error before any
                    // gate sees it.
                    let known = catalog.lookup(&pending.name).map(|entry| entry.effect);
                    #[cfg(test)]
                    let known = known.or_else(|| tools::test_tool_effect(&pending.name));
                    let (effect, rejection) = match known {
                        Some(effect) => (effect, pending.rejection),
                        None => (
                            catalog::EffectClass::ReadOnly,
                            Some(format!("unknown tool {:?}", pending.name)),
                        ),
                    };
                    if rejection.is_none() {
                        call_effects.insert(pending.provider_call_id.clone(), effect);
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
                    yield RuntimeEvent::Failed {
                        kind: RunFailureKind::Server,
                        message,
                    };
                    return;
                }
                if checkpoint.as_ref().is_some_and(|reviewer| reviewer.reviews_tools())
                    && calls.iter().filter(|call| call.rejection.is_none()).count() > 1
                {
                    for call in &mut calls {
                        if call.rejection.is_none() {
                            call.rejection = Some(
                                "JEV enforcement admits one tool call per model turn so each result is reviewed before any later tool executes; retry this call alone"
                                    .to_owned(),
                            );
                        }
                    }
                }
                // The completed turn and its requested calls travel on one event
                // so the store can persist them atomically.
                yield RuntimeEvent::AssistantTurnCompleted {
                    turn_ordinal,
                    message: assistant.clone(),
                    usage: terminal_usage,
                    calls: calls.clone(),
                    truncated: truncated_turn,
                    notice: pending_notice.take(),
                };
                budget.charge_turn(terminal_usage);
                budget.charge_tool_calls(calls.iter().filter(|call| call.rejection.is_none()).count());
                if summarizer.is_some() && !calls.is_empty() {
                    summarizer_rejected_turns += 1;
                    if summarizer_rejected_turns > 1 {
                        yield RuntimeEvent::Failed {
                            kind: RunFailureKind::ProviderProtocol,
                            message: "the compaction summarizer called a tool on two turns; tools are unavailable during compaction".to_owned(),
                        };
                        return;
                    }
                }

                if let Some((kind, message)) = turn_fault {
                    // The partial turn is durable. Re-issue the turn after a
                    // bounded backoff, or pause the run once the allowance
                    // for this turn is spent. Cancellation and the run
                    // deadline both cut the sleep short.
                    if turn_retries >= MAX_TURN_RETRIES {
                        yield RuntimeEvent::Paused {
                            pause: Box::new(qq_protocol::RunPause {
                                kind,
                                message,
                                turn_ordinal,
                                attempts: turn_retries,
                            }),
                        };
                        return;
                    }
                    turn_retries += 1;
                    let delay = turn_recovery.delay(turn_retries);
                    yield RuntimeEvent::TurnRetrying {
                        turn_ordinal,
                        attempt: turn_retries,
                        delay,
                        kind,
                        message,
                    };
                    if assistant.has_content() {
                        irreducible_message_bytes = irreducible_message_bytes
                            .saturating_add(measure_message(&assistant));
                        Arc::make_mut(&mut messages).push(assistant);
                    }
                    if messages.last().is_some_and(|message| message.role() == Role::Assistant) {
                        Arc::make_mut(&mut messages).push(Message::user(TURN_RETRY_CONTINUE_NOTICE));
                        irreducible_message_bytes = irreducible_message_bytes
                            .saturating_add(measure_message(messages.last().expect("just pushed")));
                    }
                    let sleep = tokio::time::sleep(delay);
                    tokio::pin!(sleep);
                    tokio::select! {
                        biased;
                        () = cancelled.cancelled() => return,
                        () = runtime::RunDeadline::wait(deadline) => {
                            yield RuntimeEvent::BudgetExhausted {
                                exhaustion: deadline.expect("only a finite deadline wakes").exhaustion(),
                            };
                            return;
                        }
                        () = &mut sleep => {}
                    }
                    continue;
                }
                turn_retries = 0;
                if truncated_turn {
                    // A reserved final response that ran out of room cannot be
                    // continued: the budget already settles the run below.
                    // Otherwise resume, bounded, or settle with the reason.
                    if !budget_final_turn {
                        if !assistant.has_content()
                            && truncation_reason == qq_provider::IncompleteReason::OutputTokens
                        {
                            // Nothing that would change the resend: no text,
                            // and any tool call was dropped with the cut. Either
                            // the whole cap went to hidden reasoning, or the
                            // model streamed a complete call and ran out after
                            // it. A continuation notice cannot help because
                            // there is nothing to continue and the request would
                            // be resent byte-for-byte. Raise the cap once toward
                            // the ceiling. When that is spent: an all-reasoning
                            // turn settles with the cause and both remedies
                            // named; a turn that streamed a call is continued
                            // like any visible truncation (a fresh sample may
                            // fit). A provider pause with no text is not this
                            // case: it must be resent.
                            let can_raise = empty_output_retries < MAX_EMPTY_OUTPUT_RETRIES
                                && max_output_tokens < output_ceiling;
                            if streamed_visible_output {
                                reasoning_only_truncations = 0;
                            } else {
                                reasoning_only_truncations =
                                    reasoning_only_truncations.saturating_add(1);
                            }
                            if !can_raise && !streamed_visible_output {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderOutputTruncated,
                                    message: format!(
                                        "the provider stopped at its output token limit ({max_output_tokens} tokens) \
                                         without producing any visible output on {} consecutive turn{}; the \
                                         limit was spent on reasoning. {}",
                                        reasoning_only_truncations,
                                        if reasoning_only_truncations == 1 { "" } else { "s" },
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
                                        }
                                    ),
                                };
                                return;
                            }
                        }
                        if !assistant.has_content()
                            && truncation_reason == qq_provider::IncompleteReason::OutputTokens
                            && empty_output_retries < MAX_EMPTY_OUTPUT_RETRIES
                            && max_output_tokens < output_ceiling
                        {
                            // The retry counts against the run's shared
                            // continuation cap, which the client renders
                            // against `max_output_continuations`.
                            if output_continuations >= MAX_OUTPUT_CONTINUATIONS {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderOutputTruncated,
                                    message: format!(
                                        "the provider stopped at its output token limit ({max_output_tokens} tokens) on \
                                         {} consecutive turns; the partial answer is in the transcript",
                                        u32::from(MAX_OUTPUT_CONTINUATIONS) + 1
                                    ),
                                };
                                return;
                            }
                            empty_output_retries += 1;
                            max_output_tokens = max_output_tokens
                                .saturating_mul(2)
                                .min(output_ceiling);
                            // The retry is a continuation of the same answer
                            // (1-based, bounded by MAX_EMPTY_OUTPUT_RETRIES
                            // plus MAX_OUTPUT_CONTINUATIONS across the run).
                            output_continuations += 1;
                            yield RuntimeEvent::OutputTruncated {
                                turn_ordinal,
                                continuation: output_continuations,
                            };
                            continue;
                        }
                        if output_continuations >= MAX_OUTPUT_CONTINUATIONS {
                            yield RuntimeEvent::Failed {
                                kind: RunFailureKind::ProviderOutputTruncated,
                                message: format!(
                                    "the provider stopped at its output token limit ({max_output_tokens} tokens) on \
                                     {} consecutive turns; the partial answer is in the transcript",
                                    u32::from(MAX_OUTPUT_CONTINUATIONS) + 1
                                ),
                            };
                            return;
                        }
                        output_continuations += 1;
                        yield RuntimeEvent::OutputTruncated {
                            turn_ordinal,
                            continuation: output_continuations,
                        };
                        if assistant.has_content() {
                            reasoning_only_truncations = 0;
                            irreducible_message_bytes = irreducible_message_bytes
                                .saturating_add(measure_message(&assistant));
                            Arc::make_mut(&mut messages).push(assistant);
                        }
                        if messages.last().is_some_and(|message| message.role() == Role::Assistant) {
                            Arc::make_mut(&mut messages).push(Message::user(OUTPUT_TRUNCATED_CONTINUE_NOTICE));
                            irreducible_message_bytes = irreducible_message_bytes
                                .saturating_add(measure_message(messages.last().expect("just pushed")));
                        }
                        continue;
                    }
                } else {
                    output_continuations = 0;
                    // A turn that completed (text or tool calls) ends any run
                    // of reasoning-only truncations the diagnostic counts.
                    reasoning_only_truncations = 0;
                }

                if interrupted_turn {
                    yield RuntimeEvent::Interrupted { turn_ordinal };
                    if assistant.has_content() {
                        irreducible_message_bytes = irreducible_message_bytes
                            .saturating_add(measure_message(&assistant));
                        Arc::make_mut(&mut messages).push(assistant);
                    }
                    // The interrupt exists to apply steering now. Nothing
                    // queued means the client raced a finishing run; continue
                    // with the next turn so the model resumes from its text.
                    if let Some(applied) = apply_steering(&mut stall, &mut steering, Arc::make_mut(&mut messages), &mut irreducible_message_bytes, checkpoint_context.as_mut(), &workspace, &file_state).await {
                        for steer in applied {
                            yield RuntimeEvent::SteeringApplied {
                                message_id: steer.message_id,
                                turn_ordinal: turn_ordinal.saturating_add(1),
                                attachments: steer.attachments,
                            };
                        }
                    }
                    if messages.last().is_some_and(|message| message.role() == Role::Assistant) {
                        // Providers require alternation; an interrupted turn
                        // with no steering to inject cannot be resent as-is.
                        Arc::make_mut(&mut messages).push(Message::user(INTERRUPT_CONTINUE_NOTICE));
                        irreducible_message_bytes = irreducible_message_bytes
                            .saturating_add(measure_message(messages.last().expect("just pushed")));
                    }
                    continue;
                }

                if budget_final_turn {
                    // The reserved final response has been persisted; the
                    // run settles with the limit that spent its budget.
                    let BudgetDecision::Exhausted(exhaustion) = budget.before_turn(
                        tokio::time::Instant::now(),
                        0,
                    ) else {
                        unreachable!("a requested final response always settles the run")
                    };
                    yield RuntimeEvent::BudgetExhausted { exhaustion };
                    return;
                }
                // Cost and token bounds are only observable after a turn. A
                // completed run that overran them settles as exhausted, not
                // completed, so no client can mistake the overrun for success.
                if calls.is_empty()
                    && let Some(kind) = budget.exceeded(tokio::time::Instant::now())
                    && matches!(
                        kind,
                        BudgetLimitKind::Cost
                            | BudgetLimitKind::CostUnknown
                            | BudgetLimitKind::TotalTokens
                    )
                {
                    let exhaustion = budget.exhaustion(kind, false, tokio::time::Instant::now());
                    yield RuntimeEvent::BudgetExhausted { exhaustion };
                    return;
                }

                // A sub-agent's final-answer turn ends the run whatever it
                // returned (ADR-0054 § 3): its calls were admitted with a
                // not-executed result and settle through the result path
                // below, then the run completes there. A reply with no calls
                // completes here. Jev final review, the audit hook, and
                // steering cannot redirect it; an empty reply leaves the
                // parent the child's latest report.
                if final_answer_turn && calls.is_empty() {
                    yield RuntimeEvent::Completed { final_output: None };
                    return;
                }
                if checkpoint_turn {
                    // The persisted turn is the slice boundary whether or not
                    // the model obeyed the notice. Calls it made anyway were
                    // admitted with a rejection result above and settle
                    // through the ordinary result path below, so the next
                    // turn sees one result per call and can re-issue them.
                    // An empty reply is a missed report: the slice still
                    // resets and the run continues (ADR-0054 § 2). A stall
                    // report is the same kind of turn; it leaves the slice
                    // count alone, since its calls still ran in this slice.
                    if slice_report {
                        slice_tool_calls = 0;
                    }
                    stall.reported();
                    placed_report = None;
                    continuing_slice = true;
                    if calls.is_empty() {
                        // Assembly drops an empty turn and fills the gap
                        // between the two runtime notices with this same
                        // placeholder, so live and replayed context match.
                        let assistant = if assistant.has_content() {
                            assistant
                        } else {
                            Message::assistant(EMPTY_TURN_PLACEHOLDER)
                        };
                        irreducible_message_bytes = irreducible_message_bytes
                            .saturating_add(measure_message(&assistant));
                        Arc::make_mut(&mut messages).push(assistant);
                        // Steering that arrived during the report is applied
                        // here, before the continuation notice, exactly as at
                        // any other turn boundary.
                        if let Some(applied) = apply_steering(&mut stall, &mut steering, Arc::make_mut(&mut messages), &mut irreducible_message_bytes, checkpoint_context.as_mut(), &workspace, &file_state).await {
                            for steer in applied {
                                yield RuntimeEvent::SteeringApplied {
                                    message_id: steer.message_id,
                                    turn_ordinal: turn_ordinal.saturating_add(1),
                                    attachments: steer.attachments,
                                };
                            }
                        }
                        continue;
                    }
                }
                if calls.is_empty() {
                    // Steering that arrived during the final turn is not
                    // dropped: the run continues with it instead of
                    // completing, exactly as if the model had called a tool.
                    if let Some(applied) = apply_steering(&mut stall, &mut steering, Arc::make_mut(&mut messages), &mut irreducible_message_bytes, checkpoint_context.as_mut(), &workspace, &file_state).await {
                        // A reply the run continues past stays in context, so
                        // it must be provider-valid: an empty one takes the
                        // placeholder assembly fills the gap with on replay.
                        let assistant = if assistant.has_content() {
                            assistant
                        } else {
                            Message::assistant(EMPTY_TURN_PLACEHOLDER)
                        };
                        irreducible_message_bytes = irreducible_message_bytes
                            .saturating_add(measure_message(&assistant));
                        let keep = messages.len() - applied.len();
                        let steering_messages = Arc::make_mut(&mut messages).split_off(keep);
                        Arc::make_mut(&mut messages).push(assistant);
                        Arc::make_mut(&mut messages).extend(steering_messages);
                        for steer in applied {
                            yield RuntimeEvent::SteeringApplied {
                                message_id: steer.message_id,
                                turn_ordinal: turn_ordinal.saturating_add(1),
                                attachments: steer.attachments,
                            };
                        }
                        continue;
                    }
                    // A reply without tool calls while detached children are
                    // still out is not the run's answer yet: wait for the next
                    // answer (or steering), deliver it at the next boundary,
                    // and run another turn (ADR-0054 § 4). Nothing here
                    // selects on cancellation or the deadline: the session
                    // consumer drops this stream on cancellation
                    // (`execution.rs`, the `cancellation.changed()` arm) and
                    // `RunDeadline::enforce` drops it at the deadline.
                    if let Some(spawner) = &spawner
                        && spawner.outstanding_detached() > 0
                    {
                        // Empty while waiting is likely (nothing left to do):
                        // the same placeholder keeps the request valid.
                        let assistant = if assistant.has_content() {
                            assistant
                        } else {
                            Message::assistant(EMPTY_TURN_PLACEHOLDER)
                        };
                        irreducible_message_bytes = irreducible_message_bytes
                            .saturating_add(measure_message(&assistant));
                        Arc::make_mut(&mut messages).push(assistant);
                        // A "waiting for sub-agents" activity is client work
                        // (AC14); clients see the child sessions meanwhile.
                        // The wait ends with something in context after the
                        // reply, applied steering or a delivered answer, so
                        // live and replayed context stay identical.
                        #[cfg(test)]
                        spawner.waiting_for_test();
                        let mut delivery_retry = SUBAGENT_DELIVERY_RETRY;
                        loop {
                            tokio::select! {
                                biased;
                                () = steering_arrived(&mut steering, handled_interrupt) => {}
                                () = spawner.child_settled() => {}
                            }
                            handled_interrupt = steering
                                .as_ref()
                                .map_or(handled_interrupt, |steering| *steering.interrupts.borrow());
                            if let Some(applied) = apply_steering(&mut stall, &mut steering, Arc::make_mut(&mut messages), &mut irreducible_message_bytes, checkpoint_context.as_mut(), &workspace, &file_state).await {
                                for steer in applied {
                                    yield RuntimeEvent::SteeringApplied {
                                        message_id: steer.message_id,
                                        turn_ordinal: turn_ordinal.saturating_add(1),
                                        attachments: steer.attachments,
                                    };
                                }
                                break;
                            }
                            match deliver_children(
                                spawner,
                                Boundary { turn_ordinal: turn_ordinal.saturating_add(1), reports: runtime::ReportDelivery::WithAnswers },
                                Arc::make_mut(&mut messages),
                                &mut irreducible_message_bytes,
                                &mut budget,
                                &mut stall,
                                checkpoint_context.as_mut(),
                            )
                            .await
                            {
                                // Only an answer ends the wait, and reports
                                // come only with one (`WithAnswers`): the
                                // reply already said nothing is left to do
                                // until answers arrive, and the delivery
                                // that ends the wait is this boundary's
                                // only one, so it spends one budget and
                                // precedes any steering, as replay places it.
                                Ok(delivered) if delivered.answers > 0 => {
                                    wait_delivered_for = Some(turn_ordinal.saturating_add(1));
                                    break;
                                }
                                Ok(delivered) => {
                                    debug_assert_eq!(delivered.reports, 0);
                                    // A settled child whose own descendants
                                    // are still settling has no readable spend
                                    // yet; its delivery is retried after they
                                    // settle. Pause briefly so the wake does
                                    // not spin.
                                    if spawner.settled_detached() {
                                        tokio::time::sleep(delivery_retry).await;
                                        delivery_retry = (delivery_retry * 2).min(SUBAGENT_DELIVERY_RETRY_MAX);
                                    }
                                }
                                Err(error) => {
                                    yield RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() };
                                    return;
                                }
                            }
                        }
                        continue;
                    }
                    // The candidate final answer. A root run whose work meets
                    // the audit trigger hands it to a read-only auditor before
                    // completing; a revise verdict continues the loop once
                    // with the findings, then the revision stands. Budget final
                    // turns settle above and are never audited.
                    // The first answer is always audited; a revision is audited
                    // only while another revision could follow, so the answer
                    // at the cap stands as given.
                    if let Some(hook) = &audit_hook
                        && (audit_revisions == 0
                            || audit_revisions < plan.runtime.audit.max_revisions)
                        && audit_triggers.fires(plan.runtime.audit.mode)
                        && let Ok(mut child_limits) = budget.child_budget(tokio::time::Instant::now())
                    {
                        // The auditor inherits the parent's remainder but is
                        // also bounded on its own: a verdict is a few reads,
                        // not a second open-ended run.
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
                                actions: audit_actions.clone(),
                                role: plan.runtime.audit.role,
                                revision: audit_revisions,
                            }, child_limits);
                        let verdict = tokio::select! {
                            biased;
                            () = interrupt_requested(&mut steering, handled_interrupt) => None,
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
                                        yield RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() };
                                        return;
                                    }
                                };
                                let spend = match spends.as_slice() {
                                    [] => SpawnAgentSpend::NONE,
                                    [spend] => *spend,
                                    _ => {
                                        yield RuntimeEvent::Failed { kind: RunFailureKind::Server, message: "audit cleanup found multiple uncharged children".to_owned() };
                                        return;
                                    }
                                };
                                runtime::AuditVerdict {
                                    usage: spend.usage, cost_usd_nanos: spend.cost_usd_nanos,
                                    ..runtime::AuditVerdict::unavailable()
                                }
                            }
                        };
                        budget.charge_child(verdict.usage, verdict.cost_usd_nanos);
                        hook.acknowledge();
                        let revise = verdict.outcome == qq_protocol::AuditOutcome::Revised
                            && audit_revisions < plan.runtime.audit.max_revisions;
                        yield RuntimeEvent::Audited {
                            outcome: verdict.outcome,
                            findings: verdict.findings.clone(),
                            revisions: audit_revisions,
                            usage: verdict.usage,
                            cost_usd_nanos: verdict.cost_usd_nanos,
                            audit_session: verdict.audit_session,
                        };
                        if audit_interrupted {
                            handled_interrupt = steering.as_ref().map_or(handled_interrupt, |steering| *steering.interrupts.borrow());
                            irreducible_message_bytes = irreducible_message_bytes.saturating_add(measure_message(&assistant));
                            Arc::make_mut(&mut messages).push(assistant);
                            yield RuntimeEvent::Interrupted { turn_ordinal };
                            if let Some(applied) = apply_steering(&mut stall, &mut steering, Arc::make_mut(&mut messages), &mut irreducible_message_bytes, checkpoint_context.as_mut(), &workspace, &file_state).await {
                                for steer in applied {
                                    yield RuntimeEvent::SteeringApplied { message_id: steer.message_id, turn_ordinal: turn_ordinal.saturating_add(1), attachments: steer.attachments };
                                }
                            }
                            continue;
                        }
                        if revise {
                            audit_revisions += 1;
                            irreducible_message_bytes = irreducible_message_bytes
                                .saturating_add(measure_message(&assistant));
                            Arc::make_mut(&mut messages).push(assistant);
                            let mut notice = String::from(runtime::AUDIT_REVISION_NOTICE);
                            for finding in &verdict.findings {
                                notice.push_str("\n- ");
                                notice.push_str(finding);
                            }
                            Arc::make_mut(&mut messages).push(Message::user(notice));
                            irreducible_message_bytes = irreducible_message_bytes
                                .saturating_add(measure_message(messages.last().expect("just pushed")));
                            continue;
                        }
                    }
                    // Steering accepted while an audit ran still owns the next boundary.
                    if let Some(applied) = apply_steering(&mut stall, &mut steering, Arc::make_mut(&mut messages), &mut irreducible_message_bytes, checkpoint_context.as_mut(), &workspace, &file_state).await {
                        irreducible_message_bytes = irreducible_message_bytes.saturating_add(measure_message(&assistant));
                        let keep = messages.len() - applied.len();
                        let queued = Arc::make_mut(&mut messages).split_off(keep);
                        Arc::make_mut(&mut messages).push(assistant);
                        Arc::make_mut(&mut messages).extend(queued);
                        for steer in applied { yield RuntimeEvent::SteeringApplied { message_id: steer.message_id, turn_ordinal: turn_ordinal.saturating_add(1), attachments: steer.attachments }; }
                        continue;
                    }
                    if let Some(kind) = budget.exceeded(tokio::time::Instant::now()) {
                        yield RuntimeEvent::BudgetExhausted { exhaustion: budget.exhaustion(kind, false, tokio::time::Instant::now()) };
                        return;
                    }
                    // The answer that survived audit and steering is the one
                    // the contract judges. A failure within the repair
                    // allowance continues the loop with the errors as a
                    // runtime notice; past it, the run completes with the
                    // typed failure rather than a synthetic outcome.
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
                                && output_repairs < schema.repair_turns()
                            {
                                output_repairs += 1;
                                yield RuntimeEvent::OutputRepairRequested {
                                    turn_ordinal,
                                    repair: output_repairs,
                                    errors: output::bounded_errors(errors.clone()),
                                };
                                irreducible_message_bytes = irreducible_message_bytes
                                    .saturating_add(measure_message(&assistant));
                                Arc::make_mut(&mut messages).push(assistant);
                                Arc::make_mut(&mut messages).push(Message::user(output::repair_notice(errors)));
                                irreducible_message_bytes = irreducible_message_bytes
                                    .saturating_add(measure_message(messages.last().expect("just pushed")));
                                continue;
                            }
                            Some(Box::new(output::final_output(validation, output_repairs)))
                        }
                    };
                    if let Some(reviewer) = &checkpoint {
                        let answer = assistant
                            .content()
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        let context = checkpoint_context.as_mut().expect("enabled review context");
                        let correlation = format!("final:{turn_ordinal}");
                        // A review the harness could not run is recorded as
                        // `Unavailable` and the candidate completes: the
                        // verdict is evidence for the user, not the run's
                        // outcome. Only the reviewer's own RED verdict, within
                        // the repair allowance, redirects the run.
                        let final_evidence = if context.task_overflow {
                            Err("JEV final checkpoint was not sent because the original task exceeded the exact review bound")
                        } else {
                            context.final_evidence(answer).ok_or(
                                "JEV final checkpoint was not sent because the combined candidate and tool evidence exceeded the exact review bound",
                            )
                        };
                        match final_evidence {
                            Err(feedback) => {
                                yield RuntimeEvent::CheckpointReviewed {
                                    spend: None,
                                    correlation,
                                    phase: qq_protocol::CheckpointPhase::FinalCandidate,
                                    tool_call_id: None,
                                    outcome: qq_protocol::CheckpointOutcome::Unavailable,
                                    confidence: None,
                                    feedback: feedback.to_owned(),
                                };
                            }
                            Ok(final_evidence) => {
                        let remaining = match budget.remaining(tokio::time::Instant::now()) {
                            Ok(remaining) => remaining,
                            Err(kind) => {
                                yield RuntimeEvent::BudgetExhausted { exhaustion: budget.exhaustion(kind, false, tokio::time::Instant::now()) };
                                return;
                            }
                        };
                        if let Err(error) = context.admit(remaining.max_cost_usd_nanos, reviewer.max_cost_usd_nanos()) {
                            if let Some(kind) = error.budget_kind() {
                                yield RuntimeEvent::BudgetExhausted { exhaustion: budget.exhaustion(kind, false, tokio::time::Instant::now()) };
                            } else {
                                yield RuntimeEvent::Failed { kind: RunFailureKind::Policy, message: error.to_string() };
                            }
                            return;
                        }
                        let request = runtime::CheckpointRequest {
                            correlation: correlation.clone(),
                            phase: runtime::CheckpointPhase::FinalCandidate,
                            tool_call_id: None,
                            tool: None,
                            task: context.task.clone(),
                            evidence: final_evidence,
                            is_error: false,
                        };
                                yield RuntimeEvent::CheckpointStarted {
                            correlation: correlation.clone(), phase: qq_protocol::CheckpointPhase::FinalCandidate, tool_call_id: None,
                        };
                        let verdict = tokio::select! {
                                    biased;
                                    () = interrupt_requested(&mut steering, handled_interrupt) => None,
                                    verdict = runtime::assess_checkpoint(reviewer.as_ref(), request) => Some(verdict),
                                };
                                let Some(verdict) = verdict else {
                                    budget.charge_child(None, None);
                                    yield RuntimeEvent::CheckpointReviewed {
                                        spend: Some(qq_protocol::CheckpointSpend::default()),
                                        correlation,
                                        phase: qq_protocol::CheckpointPhase::FinalCandidate,
                                        tool_call_id: None,
                                        outcome: qq_protocol::CheckpointOutcome::Unavailable,
                                        confidence: None,
                                        feedback: "Final review interrupted by new user input; no assessment was recorded".to_owned(),
                                    };
                                    handled_interrupt = steering.as_ref().map_or(handled_interrupt, |steering| *steering.interrupts.borrow());
                                    irreducible_message_bytes = irreducible_message_bytes.saturating_add(measure_message(&assistant));
                                    Arc::make_mut(&mut messages).push(assistant);
                                    yield RuntimeEvent::Interrupted { turn_ordinal };
                                    if let Some(applied) = apply_steering(&mut stall, &mut steering, Arc::make_mut(&mut messages), &mut irreducible_message_bytes, checkpoint_context.as_mut(), &workspace, &file_state).await {
                                        for steer in applied {
                                            yield RuntimeEvent::SteeringApplied { message_id: steer.message_id, turn_ordinal: turn_ordinal.saturating_add(1), attachments: steer.attachments };
                                        }
                                    }
                                    continue;
                                };
                        budget.charge_child(verdict.spend.usage, verdict.spend.estimated_cost_usd_nanos);
                        yield RuntimeEvent::CheckpointReviewed {
                                spend: Some(verdict.spend),
                            correlation,
                            phase: qq_protocol::CheckpointPhase::FinalCandidate,
                            tool_call_id: None,
                            outcome: checkpoint_protocol_outcome(verdict.outcome),
                            confidence: verdict.confidence,
                            feedback: runtime::bounded_checkpoint_text(&verdict.feedback),
                        };
                        if let Some(kind) = budget.exceeded(tokio::time::Instant::now()) {
                            yield RuntimeEvent::BudgetExhausted { exhaustion: budget.exhaustion(kind, false, tokio::time::Instant::now()) };
                            return;
                        }
                        // `Unavailable` (timeout, malformed reply, outage) is
                        // already recorded above; the candidate stands. A RED
                        // verdict redirects the run while a correction attempt
                        // remains; once both are spent the run completes with
                        // the verdict on record rather than failing.
                        if !verdict.outcome.allows_progress()
                            && verdict.outcome != runtime::CheckpointOutcome::Unavailable
                            && checkpoint_context.as_mut().expect("enabled review context").repair()
                        {
                            irreducible_message_bytes = irreducible_message_bytes
                                .saturating_add(measure_message(&assistant));
                            Arc::make_mut(&mut messages).push(assistant);
                            let notice = format!(
                                "JEV RED {}. Correct the candidate or gather missing direct evidence before attempting completion. Feedback: {}",
                                verdict.outcome.label(), verdict.feedback
                            );
                            Arc::make_mut(&mut messages).push(Message::user(notice));
                            irreducible_message_bytes = irreducible_message_bytes
                                .saturating_add(measure_message(messages.last().expect("just pushed")));
                            continue;
                        }
                            }
                        }
                    }
                    // A review may await remote inference. Input accepted during
                    // that wait belongs to this run, not its successor.
                    if let Some(applied) = apply_steering(&mut stall, &mut steering, Arc::make_mut(&mut messages), &mut irreducible_message_bytes, checkpoint_context.as_mut(), &workspace, &file_state).await {
                        irreducible_message_bytes = irreducible_message_bytes.saturating_add(measure_message(&assistant));
                        let keep = messages.len() - applied.len();
                        let queued = Arc::make_mut(&mut messages).split_off(keep);
                        Arc::make_mut(&mut messages).push(assistant);
                        Arc::make_mut(&mut messages).extend(queued);
                        for steer in applied { yield RuntimeEvent::SteeringApplied { message_id: steer.message_id, turn_ordinal: turn_ordinal.saturating_add(1), attachments: steer.attachments }; }
                        continue;
                    }
                    yield RuntimeEvent::Completed { final_output };
                    return;
                }
                irreducible_message_bytes = irreducible_message_bytes
                    .saturating_add(measure_message(&assistant));
                Arc::make_mut(&mut messages).push(assistant);

                // Policy resolves sequentially in request order, after the
                // turn and its `requested` call rows are persisted, so
                // approval prompts arrive one at a time. Calls with malformed
                // arguments never reach the gate: there is nothing executable
                // to approve, so they short-circuit to their tool error below.
                let mut results: Vec<Option<RetainedResult>> = vec![None; calls.len()];
                let mut turn_interrupted_in_tools = false;
                for (index, call) in calls.iter().enumerate() {
                    if call.rejection.is_some() {
                        continue;
                    }
                    if turn_interrupted_in_tools {
                        // Calls behind an interrupted approval wait never
                        // execute; they settle like calls behind a cancel.
                        results[index] = Some(RetainedResult::error(INTERRUPTED_TOOL_RESULT.to_owned()));
                        continue;
                    }
                    // An approval wait is a boundary too: an interrupting
                    // steer withdraws the pending request instead of leaving
                    // the user to answer a question the steer made moot.
                    let decision = {
                        let interrupt = interrupt_requested(&mut steering, handled_interrupt);
                        tokio::select! {
                            biased;
                            () = interrupt => None,
                            decision = gate.resolve(call) => Some(decision),
                        }
                    };
                    let Some(decision) = decision else {
                        turn_interrupted_in_tools = true;
                        results[index] = Some(RetainedResult::error(INTERRUPTED_TOOL_RESULT.to_owned()));
                        continue;
                    };
                    // Reviewer spend is charged whatever the verdict; the
                    // wrapped decision is then applied like any other.
                    let decision = match decision {
                        GateDecision::Reviewed { decision, spend } => {
                            budget.charge_child(spend.usage, spend.cost_usd_nanos);
                            yield RuntimeEvent::ReviewCharged {
                                usage: spend.usage,
                                cost_usd_nanos: spend.cost_usd_nanos,
                            };
                            *decision
                        }
                        decision => decision,
                    };
                    match decision {
                        GateDecision::Execute => {}
                        GateDecision::Deny { message } => {
                            // A denied call counts toward the stall report:
                            // a run that keeps asking for denied calls is
                            // not producing anything either.
                            stall.settled(false);
                            results[index] = Some(RetainedResult::error(message.clone()));
                            yield RuntimeEvent::ToolCallDenied { id: call.id, message };
                        }
                        // The gate persisted and published the answered call;
                        // the answer is the result and nothing executes.
                        GateDecision::Answered { result } => {
                            // The human answered: new input, like a steer.
                            stall.progress();
                            results[index] = Some(RetainedResult::answered(result.clone()));
                            yield RuntimeEvent::ToolCallAnswered { id: call.id, result };
                        }
                        GateDecision::Fail { kind, message } => {
                            yield RuntimeEvent::Failed { kind, message };
                            return;
                        }
                        GateDecision::Reviewed { .. } => {
                            unreachable!("reviewed decisions are unwrapped once, never nested")
                        }
                    }
                }
                let approved = calls
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| results[*index].is_none() && !turn_interrupted_in_tools)
                    .map(|(_, call)| call.clone())
                    .collect::<Vec<_>>();
                for call in &approved {
                    yield RuntimeEvent::ToolCallStarted { id: call.id };
                }

                // `select_tools` mutates run state (the pin set), so it
                // executes here, before the concurrent dispatch below, in
                // request order. It is read-only and instantaneous.
                let mut pins_changed = false;
                for (index, call) in calls.iter().enumerate() {
                    if results[index].is_some() || call.rejection.is_some() {
                        continue;
                    }
                    if !matches!(
                        catalog.lookup(&call.name).map(|entry| entry.host),
                        Some(catalog::ToolHost::SelectTools)
                    ) {
                        continue;
                    }
                    let (result, changed) = select_tools(&catalog, &mut pins, &call.arguments);
                    stall.settled(false);
                    pins_changed |= changed;
                    results[index] = Some(RetainedResult::retain(&result, &call.name, call.id));
                    yield RuntimeEvent::ToolCallFinished {
                        id: call.id,
                        result: result.model_text,
                        is_error: result.is_error,
                        file_states: Vec::new(),
                        display: result.ui_payload,
                        spill: None,
                    };
                }
                if pins_changed {
                    tool_specs = catalog.specs_with_pins(&base_specs, &pins);
                    tool_schema = catalog.schema_measurement(&tool_specs);
                }
                let approved = approved
                    .into_iter()
                    .filter(|call| results[usize::from(call.call_ordinal - 1)].is_none())
                    .collect::<Vec<_>>();
                let bounded_child_spend = limits.max_cost_usd_nanos.is_some()
                    || limits.max_total_tokens.is_some()
                    || limits.max_input_tokens.is_some()
                    || limits.max_output_tokens.is_some();
                // A read spawn returns on durable admission and its answer
                // arrives at a later boundary (ADR-0054 § 4), unless the run
                // has a finite token or cost bound: each child is granted the
                // parent's whole remainder, so overlapping children could
                // overspend it, and those runs keep blocking spawns.
                let detach_spawns = !bounded_child_spend;
                let execute_one = |call: RuntimeToolCall,
                                   output: Option<
                    tokio::sync::mpsc::Sender<String>,
                >, child_limits: Result<runtime::ChildBudget, BudgetLimitKind>| {
                    let workspace = workspace.clone();
                    let file_state = Arc::clone(&file_state);
                    let cancelled = cancelled.clone();
                    let catalog = Arc::clone(&catalog);
                    let skills = Arc::clone(&skills);
                    let pack_roots = Arc::clone(&pack_roots);
                    let hosts = Arc::clone(&hosts);
                    let spawner = spawner.clone();
                    let tool_tasks = tool_tasks.clone();
                    let history = history.clone();
                    let spills = spills.clone();
                    let delegation = Arc::clone(&delegation);
                    let shell_policy = Arc::clone(&shell_policy);
                    let network_policy = Arc::clone(&network_policy);
                    // Under progressive exposure only pinned externals were
                    // offered; a call to one that was not is refused with the
                    // way to make it available.
                    let offered = catalog.exposure() == catalog::Exposure::Full
                        || pins.names().contains(&call.name);
                    async move {
                        // `Some` when a sub-agent ran: its spend (or unknown
                        // spend) is charged to the parent's budgets.
                        let mut child_spend: Option<SpawnAgentSpend> = None;
                        let host = catalog.lookup(&call.name).map(|entry| entry.host);
                        // Strict built-in preference refuses a shell habit
                        // before execution: the benchmark arm for what shell
                        // costs. `hint` runs the command and appends a line.
                        let strict_refusal = (matches!(call.name.as_str(), "shell" | "exec")
                            && shell_policy.builtin_preference == runtime::BuiltinPreference::Strict)
                            .then(|| {
                                // The command text for either tool: exec's
                                // argv rendered as the equivalent line.
                                let command = match approval::classify(call.effect, &call.name, &call.arguments, &network_policy) {
                                    approval::ToolClass::Shell { command, .. } => command,
                                    _ => String::new(),
                                };
                                runtime::builtin_alternative(&command)
                            })
                            .flatten()
                            .map(|(program, builtin)| {
                                format!(
                                    "use_builtin: {program} is refused under policy.builtin_preference=strict; use {builtin} instead"
                                )
                            });
                        let result = match call.rejection.clone().or(strict_refusal) {
                            Some(error) => tools::ToolOutput::verbatim_error(error),
                            // spawn_agent dispatches to the session layer. A
                            // run without a spawner rejects the call outright:
                            // the declaration is already absent there, but a
                            // model may still guess the name.
                            None if host == Some(catalog::ToolHost::SpawnAgent) => match &spawner {
                                Some(spawner) => {
                                    match serde_json::from_str::<tools::SpawnAgentArgs>(
                                        &call.arguments,
                                    ) {
                                        Ok(arguments) if arguments.task.trim().is_empty() => {
                                            tools::bounded_result(
                                                "task must not be empty".to_owned(),
                                                true,
                                            )
                                        }
                                        Ok(mut arguments) => {
                                            arguments.model = arguments.model.and_then(|model| {
                                                let model = model.trim().to_owned();
                                                (!model.is_empty()).then_some(model)
                                            });
                                            // Role and roster resolve here, the one
                                            // choke point: an exact model must be a
                                            // roster route when a roster exists, and
                                            // a role maps to the first roster route
                                            // declaring it. The session spawner then
                                            // validates the resolved route against
                                            // the authenticated served model list
                                            // before any durable child state exists.
                                            let resolution = resolve_delegation_route(
                                                &delegation,
                                                arguments.model,
                                                arguments.role,
                                                reasoning_effort,
                                            );
                                            match (child_limits, resolution) {
                                                (_, Err(message)) => tools::bounded_result(message, true),
                                                (Err(kind), Ok(_)) => tools::bounded_result(
                                                    format!(
                                                        "this run cannot afford a sub-agent: its {} budget is spent; continue with what you have",
                                                        kind.as_str()
                                                    ),
                                                    true,
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
                                                    // A detached child's spend is
                                                    // charged when its answer is
                                                    // delivered, not here.
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
                                        Err(error) => tools::bounded_result(
                                            format!("invalid arguments: {error}"),
                                            true,
                                        ),
                                    }
                                }
                                None => {
                                    tools::bounded_result(SPAWN_UNAVAILABLE_RESULT.to_owned(), true)
                                }
                            },
                            // Waiting and cancelling act on this run's own
                            // background children; their answers arrive as
                            // delivered notices at the next boundary, never
                            // in these results (ADR-0054 § 4).
                            None if host == Some(catalog::ToolHost::WaitAgents) => match &spawner {
                                Some(spawner) => match serde_json::from_str::<tools::WaitAgentsArgs>(&call.arguments) {
                                    Ok(arguments) if !(1..=tools::MAX_WAIT_AGENTS_SECS).contains(&arguments.timeout_seconds) => {
                                        tools::bounded_result(
                                            format!("timeout_seconds must be between 1 and {}", tools::MAX_WAIT_AGENTS_SECS),
                                            true,
                                        )
                                    }
                                    Ok(arguments) if arguments.ids.as_ref().is_some_and(|ids| ids.len() > usize::from(sessions::MAX_SPAWNED_CHILDREN_PER_RUN)) => {
                                        tools::bounded_result(
                                            format!("ids may name at most {} sub-agents", sessions::MAX_SPAWNED_CHILDREN_PER_RUN),
                                            true,
                                        )
                                    }
                                    Ok(arguments) => match arguments
                                        .ids
                                        .map(|ids| ids.iter().map(|id| id.trim().parse::<qq_protocol::SessionId>()).collect::<Result<Vec<_>, _>>())
                                        .transpose()
                                    {
                                        Err(_) => tools::bounded_result(
                                            "ids must be sub-agent ids from spawn_agent results".to_owned(),
                                            true,
                                        ),
                                        Ok(ids) => match spawner
                                            .wait_children(ids, Duration::from_secs(arguments.timeout_seconds))
                                            .await
                                        {
                                            Ok(report) => tools::bounded_result(report.render(arguments.timeout_seconds), false),
                                            Err(error) => tools::bounded_result(error.to_string(), true),
                                        },
                                    },
                                    Err(error) => tools::bounded_result(format!("invalid arguments: {error}"), true),
                                },
                                None => tools::bounded_result(SPAWN_UNAVAILABLE_RESULT.to_owned(), true),
                            },
                            None if host == Some(catalog::ToolHost::CancelAgent) => match &spawner {
                                Some(spawner) => match serde_json::from_str::<tools::CancelAgentArgs>(&call.arguments) {
                                    Ok(arguments) => match arguments.id.trim().parse::<qq_protocol::SessionId>() {
                                        Ok(id) => match spawner.cancel_child(id).await {
                                            Ok(outcome) => {
                                                let (text, is_error) = outcome.render(id);
                                                tools::bounded_result(text, is_error)
                                            }
                                            Err(error) => tools::bounded_result(error.to_string(), true),
                                        },
                                        Err(_) => tools::bounded_result(
                                            "id must be a sub-agent id from a spawn_agent result".to_owned(),
                                            true,
                                        ),
                                    },
                                    Err(error) => tools::bounded_result(format!("invalid arguments: {error}"), true),
                                },
                                None => tools::bounded_result(SPAWN_UNAVAILABLE_RESULT.to_owned(), true),
                            },
                            // Full-transcript recall dispatches to the session
                            // layer; the tool is declared only when a searcher
                            // exists, so a guessed call is simply unknown here.
                            None if host == Some(catalog::ToolHost::SearchHistory) && history.is_some() => {
                                let history = history.expect("the history searcher was just checked");
                                match serde_json::from_str::<SearchHistoryArgs>(&call.arguments) {
                                    Ok(arguments) if arguments.query.trim().is_empty() => {
                                        tools::bounded_result("query must not be empty".to_owned(), true)
                                    }
                                    Ok(arguments) => {
                                        let limit = arguments.limit.clamp(1, crate::runtime::MAX_HISTORY_MATCHES);
                                        match history.search(arguments.query.clone(), limit).await {
                                            Ok(search) => tools::bounded_result(
                                                render_history_matches(&arguments.query, &search),
                                                false,
                                            ),
                                            Err(error) => tools::bounded_result(error, true),
                                        }
                                    }
                                    Err(error) => tools::bounded_result(
                                        format!("invalid arguments: {error}"),
                                        true,
                                    ),
                                }
                            }
                            // A stored complete output, read back exactly and
                            // unmasked: the model asked for a range of what it
                            // already produced. Session-scoped by the reader.
                            None if host == Some(catalog::ToolHost::ReadToolResult) && spills.is_some() => {
                                let spills = spills.expect("the spill reader was just checked");
                                match serde_json::from_str::<ReadToolResultArgs>(&call.arguments) {
                                    Ok(arguments) => match runtime::SpillHandle::parse(&arguments.handle) {
                                        None => tools::bounded_result(
                                            "handle_invalid: expected t:<tool>:<call8>:<digest8>".to_owned(),
                                            true,
                                        ),
                                        Some(handle) => match spills.read(handle).await {
                                            Ok(runtime::SpillRead::Found { text, .. }) => {
                                                match render_tool_result(&arguments.handle, &arguments, &text) {
                                                    Ok(page) => tools::ToolOutput::exact(page, &runtime::READ_TOOL_RESULT_BOUNDS),
                                                    Err(error) => tools::bounded_result(error, true),
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
                                            Ok(runtime::SpillRead::ForeignSession) => tools::bounded_result(
                                                "handle_foreign_session: stored outputs are readable only by the session that produced them".to_owned(),
                                                true,
                                            ),
                                            Err(error) => tools::bounded_result(error, true),
                                        },
                                    },
                                    Err(error) => tools::bounded_result(
                                        format!("invalid arguments: {error}"),
                                        true,
                                    ),
                                }
                            }
                            // The model asked for a disclosed skill body. Same
                            // bounds as a `/name` invocation; failures are
                            // tool errors, not run failures.
                            None if host == Some(catalog::ToolHost::LoadSkill) => {
                                match serde_json::from_str::<workspace::skills::LoadSkillArgs>(&call.arguments) {
                                    Ok(arguments) => match workspace::load_disclosed_skill(
                                        workspace,
                                        pack_roots,
                                        skills,
                                        cancelled,
                                        arguments.name.trim().to_owned(),
                                        &tool_tasks,
                                    )
                                    .await
                                    {
                                        Ok(guidance) => {
                                            tools::bounded_result(guidance.render_for_tool(), false)
                                        }
                                        Err(error) => tools::bounded_result(error.to_string(), true),
                                    },
                                    Err(error) => tools::bounded_result(
                                        format!("invalid arguments: {error}"),
                                        true,
                                    ),
                                }
                            }
                            // External calls dispatch to their host by index;
                            // the outcome flows through the same bounded-result
                            // truncation as built-in tools, so an external call
                            // is indistinguishable from a built-in on the wire.
                            None if let Some(catalog::ToolHost::External { .. }) = host
                                && !offered =>
                            {
                                tools::bounded_result(
                                    format!(
                                        "{} is not available in this run yet; call {} with keywords \
                                         describing it first",
                                        call.name, catalog::SELECT_TOOLS_TOOL
                                    ),
                                    true,
                                )
                            }
                            None if let Some(catalog::ToolHost::External { host: index }) = host => {
                                match hosts[index]
                                    .call(call.name.clone(), call.arguments.clone(), cancelled)
                                    .await
                                {
                                    Ok(outcome) => tools::bounded_result(outcome.content, outcome.is_error),
                                    Err(error) => hosts::host_error_result(&error),
                                }
                            }
                            None => {
                                tools::execute(
                                    workspace,
                                    file_state,
                                    call.name.clone(),
                                    call.arguments.clone(),
                                    cancelled,
                                    output,
                                    tool_tasks,
                                    shell_policy,
                                    network_policy,
                                )
                                .await
                            }
                        };
                        (call, result, child_spend)
                    }
                };
                // Read-only turns overlap under a small bound; a turn with any
                // mutating, shell, or external call runs entirely in request
                // order so side effects never interleave and every read is
                // deterministically ordered against the mutations. Only a
                // read child may overlap: a write child is a mutation.
                // Finite spend cannot be granted independently to overlapping children.
                // Unbounded and duration-only read fanout retains its concurrency.
                // A wait or cancel runs in request order, after the spawns
                // before it in the turn, so it sees the children they started.
                let overlaps = |call: &RuntimeToolCall| {
                    !(bounded_child_spend && catalog.lookup(&call.name).is_some_and(|entry| entry.host == catalog::ToolHost::SpawnAgent))
                    && !catalog.lookup(&call.name).is_some_and(|entry| {
                        matches!(entry.host, catalog::ToolHost::WaitAgents | catalog::ToolHost::CancelAgent)
                    })
                    && matches!(
                        approval::classify(call.effect, &call.name, &call.arguments, &network_policy),
                        approval::ToolClass::ReadOnly
                    )
                };
                // The leading run of read-only calls overlaps under a small
                // bound; from the first mutating, shell, or external call on,
                // the rest runs in request order so side effects never
                // interleave and every read that follows a mutation is
                // deterministically ordered against it. A read that precedes
                // every mutation sees the same workspace either way. Only a
                // read child may overlap: a write child is a mutation, and
                // finite spend cannot be granted independently to overlapping
                // children.
                let leading_reads = approved.iter().take_while(|call| overlaps(call)).count();
                let mut ordered = approved;
                let overlapped: Vec<RuntimeToolCall> = ordered.drain(..leading_reads).collect();
                if !overlapped.is_empty() {
                    let child_limits = budget.child_budget(tokio::time::Instant::now());
                    let mut executions = futures_stream::iter(
                        overlapped.into_iter().map(|call| execute_one(call, None, child_limits)),
                    )
                        .buffer_unordered(MAX_PARALLEL_READS);
                    loop {
                        let interrupt = interrupt_requested(&mut steering, handled_interrupt);
                        let next = tokio::select! {
                            biased;
                            () = interrupt => {
                                turn_interrupted_in_tools = true;
                                break;
                            }
                            next = executions.next() => next,
                        };
                        let Some((call, result, child_spend)) = next else {
                            break;
                        };
                        if let Err(error) = tool_tasks.check() {
                            yield RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() };
                            return;
                        }
                        if let Some(spend) = child_spend {
                            budget.charge_child(spend.usage, spend.cost_usd_nanos);
                            if let Some(spawner) = &spawner { spawner.acknowledge(call.id); }
                        }
                        note_audited_action(
                            &mut audit_triggers,
                            &mut audit_actions,
                            audit_hook.is_some(),
                            &call,
                            &result,
                        );
                        // Runtime rejections (over the cap, made in a report
                        // turn, Jev's one-call rule, unknown or malformed)
                        // never ran and are not counted.
                        if call.rejection.is_none() {
                            // A detached spawn's receipt is not an answer: the
                            // answer is progress when it is delivered.
                            let entry = catalog.lookup(&call.name);
                            let receipt = entry.is_some_and(|entry| entry.host == catalog::ToolHost::SpawnAgent)
                                && child_spend.is_none();
                            stall.settled(!receipt && runtime::is_progress(&call, entry, &result));
                        }
                        let result = cite_spill(result, &call.name, call.id, spills.is_some());
                        results[usize::from(call.call_ordinal - 1)] =
                            Some(RetainedResult::retain(&result, &call.name, call.id));
                        yield RuntimeEvent::ToolCallFinished {
                            id: call.id,
                            result: result.model_text,
                            is_error: result.is_error,
                            file_states: result.file_states,
                            display: result.ui_payload,
                            spill: result.spill,
                        };
                    }
                }
                if !turn_interrupted_in_tools {
                    for call in ordered {
                        // Live output chunks (shell) interleave with execution:
                        // drain the channel while the call runs so long
                        // commands render as they print.
                        let call_id = call.id;
                        let mut call_id_holder = Some(call.clone());
                        let (delta_sender, mut deltas) =
                            tokio::sync::mpsc::channel::<String>(SHELL_OUTPUT_QUEUE_CAPACITY);
                        let mut execution = Box::pin(execute_one(call, Some(delta_sender), budget.child_budget(tokio::time::Instant::now())));
                        let mut output_closed = false;
                        let (call, result, child_spend) = loop {
                            let interrupt = interrupt_requested(&mut steering, handled_interrupt);
                            tokio::select! {
                                biased;
                                () = interrupt => {
                                    // Stop dispatch, then await owned local work
                                    // and children before applying steering.
                                    drop(execution);
                                    if let Err(error) = tool_tasks.drain().await {
                                        yield RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() };
                                        return;
                                    }
                                    if let Some(spawner) = &spawner {
                                        match spawner.drain_attached().await {
                                            Ok(spends) => for spend in spends { budget.charge_child(spend.usage, spend.cost_usd_nanos); },
                                            Err(error) => {
                                                yield RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() };
                                                return;
                                            }
                                        }
                                    }
                                    break (call_id_holder.take().expect("call retained"), tools::ToolOutput::verbatim_error(INTERRUPTED_TOOL_RESULT.to_owned()), None);
                                }
                                chunk = deltas.recv(), if !output_closed => match chunk {
                                    Some(chunk) => {
                                        yield RuntimeEvent::ToolCallOutputDelta { id: call_id, chunk };
                                    }
                                    // Keep selecting interruption after output closes.
                                    None => output_closed = true,
                                },
                                completed = &mut execution => break completed,
                            }
                        };
                        if let Err(error) = tool_tasks.check() {
                            yield RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() };
                            return;
                        }
                        let interrupted_here = result.model_text == INTERRUPTED_TOOL_RESULT && result.is_error && result.file_states.is_empty() && steering.as_ref().is_some_and(|steering| *steering.interrupts.borrow() > handled_interrupt);
                        // Chunks sent in the execution's final poll may still
                        // be buffered; drain them before the terminal event.
                        while let Ok(chunk) = deltas.try_recv() {
                            yield RuntimeEvent::ToolCallOutputDelta { id: call_id, chunk };
                        }
                        if let Some(spend) = child_spend {
                            budget.charge_child(spend.usage, spend.cost_usd_nanos);
                            if let Some(spawner) = &spawner { spawner.acknowledge(call.id); }
                        }
                        note_audited_action(
                            &mut audit_triggers,
                            &mut audit_actions,
                            audit_hook.is_some(),
                            &call,
                            &result,
                        );
                        // Runtime rejections (over the cap, made in a report
                        // turn, Jev's one-call rule, unknown or malformed)
                        // never ran and are not counted.
                        if call.rejection.is_none() {
                            // A detached spawn's receipt is not an answer: the
                            // answer is progress when it is delivered.
                            let entry = catalog.lookup(&call.name);
                            let receipt = entry.is_some_and(|entry| entry.host == catalog::ToolHost::SpawnAgent)
                                && child_spend.is_none();
                            stall.settled(!receipt && runtime::is_progress(&call, entry, &result));
                        }
                        let result = cite_spill(result, &call.name, call.id, spills.is_some());
                        results[usize::from(call.call_ordinal - 1)] =
                            Some(RetainedResult::retain(&result, &call.name, call.id));
                        yield RuntimeEvent::ToolCallFinished {
                            id: call.id,
                            result: result.model_text,
                            is_error: result.is_error,
                            file_states: result.file_states,
                            display: result.ui_payload,
                            spill: result.spill,
                        };
                        if interrupted_here {
                            turn_interrupted_in_tools = true;
                            break;
                        }
                    }
                }
                if turn_interrupted_in_tools {
                    if let Err(error) = tool_tasks.drain().await {
                                        yield RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() };
                                        return;
                                    }
                    if let Some(spawner) = &spawner {
                        match spawner.drain_attached().await {
                            Ok(spends) => for spend in spends { budget.charge_child(spend.usage, spend.cost_usd_nanos); },
                            Err(error) => {
                                yield RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() };
                                return;
                            }
                        }
                    }
                    handled_interrupt = steering
                        .as_ref()
                        .map_or(handled_interrupt, |steering| *steering.interrupts.borrow());
                    // Calls that never finished settle as interrupted so the
                    // transcript stays provider-valid: one result per call.
                    for (index, call) in calls.iter().enumerate() {
                        if results[index].is_none() {
                            results[index] = Some(RetainedResult::error(INTERRUPTED_TOOL_RESULT.to_owned()));
                            yield RuntimeEvent::ToolCallFinished {
                                id: call.id,
                                result: INTERRUPTED_TOOL_RESULT.to_owned(),
                                is_error: true,
                                file_states: Vec::new(),
                                display: None,
                                spill: None,
                            };
                        }
                    }
                    yield RuntimeEvent::Interrupted { turn_ordinal };
                }
                // Mandatory post-result checkpoints run after every outcome,
                // including denied, malformed, unknown, interrupted, MCP, and
                // child-task results. Their feedback enters the same retained
                // result the next provider turn sees. The reviewer is an
                // internal capability and is never represented as a tool call,
                // so this cannot recursively checkpoint itself.
                let mut checkpoint_correction_notice = None;
                if let Some(reviewer) = &checkpoint {
                    let context = checkpoint_context.as_mut().expect("enabled review context");
                    let mut correction = Vec::new();
                    for (call, retained) in calls.iter().zip(results.iter_mut()) {
                        let retained = retained.as_mut().expect("every tool outcome is retained before checkpointing");
                        let correlation = format!("tool:{}", call.id);
                        let evidence = format!(
                            "tool: {}\narguments: {}\nresult: {}",
                            call.name,
                            tools::output::mask_secrets(call.arguments.clone()),
                            retained.model_text
                        );
                        let evidence = tools::output::mask_secrets(evidence);
                        context.record(format!("{correlation}\n{evidence}"));
                        if !reviewer.reviews_tools() {
                            continue;
                        }
                        let overflow = if context.task_overflow {
                            Some("original task")
                        } else if !runtime::checkpoint_text_fits(&evidence) {
                            Some("tool arguments and result")
                        } else {
                            None
                        };
                        if let Some(field) = overflow {
                            // Recorded as unreviewed; the durable result stands
                            // and the run continues.
                            let feedback = format!(
                                "JEV tool checkpoint was not sent because {field} exceeded the exact review bound"
                            );
                            retained.model_text.push_str(&format!("\n\n[JEV unavailable: {feedback}]"));
                            yield RuntimeEvent::CheckpointReviewed {
                                spend: None,
                                correlation,
                                phase: qq_protocol::CheckpointPhase::ToolResult,
                                tool_call_id: Some(call.id),
                                outcome: qq_protocol::CheckpointOutcome::Unavailable,
                                confidence: None,
                                feedback,
                            };
                            continue;
                        }
                        let remaining = match budget.remaining(tokio::time::Instant::now()) {
                            Ok(remaining) => remaining,
                            Err(kind) => {
                                yield RuntimeEvent::BudgetExhausted { exhaustion: budget.exhaustion(kind, false, tokio::time::Instant::now()) };
                                return;
                            }
                        };
                        if let Err(error) = context.admit(remaining.max_cost_usd_nanos, reviewer.max_cost_usd_nanos()) {
                            if let Some(kind) = error.budget_kind() {
                                yield RuntimeEvent::BudgetExhausted { exhaustion: budget.exhaustion(kind, false, tokio::time::Instant::now()) };
                            } else {
                                yield RuntimeEvent::Failed { kind: RunFailureKind::Policy, message: error.to_string() };
                            }
                            return;
                        }
                        let request = runtime::CheckpointRequest {
                            correlation: correlation.clone(),
                            phase: runtime::CheckpointPhase::ToolResult,
                            tool_call_id: Some(call.id),
                            tool: Some(call.name.clone()),
                            task: context.task.clone(),
                            evidence,
                            is_error: retained.is_error,
                        };
                        yield RuntimeEvent::CheckpointStarted {
                            correlation: correlation.clone(), phase: qq_protocol::CheckpointPhase::ToolResult, tool_call_id: Some(call.id),
                        };
                        let verdict = runtime::assess_checkpoint(reviewer.as_ref(), request).await;
                        budget.charge_child(verdict.spend.usage, verdict.spend.estimated_cost_usd_nanos);
                        yield RuntimeEvent::CheckpointReviewed {
                                spend: Some(verdict.spend),
                            correlation,
                            phase: qq_protocol::CheckpointPhase::ToolResult,
                            tool_call_id: Some(call.id),
                            outcome: checkpoint_protocol_outcome(verdict.outcome),
                            confidence: verdict.confidence,
                            feedback: runtime::bounded_checkpoint_text(&verdict.feedback),
                        };
                        let marker = if verdict.outcome.allows_progress() { "GREEN" } else { "RED" };
                        retained.model_text.push_str(&format!(
                            "\n\n[JEV {marker} {}: {}]",
                            verdict.outcome.label(),
                            runtime::bounded_checkpoint_text(&verdict.feedback)
                        ));
                        if let Some(kind) = budget.exceeded(tokio::time::Instant::now()) {
                            yield RuntimeEvent::BudgetExhausted { exhaustion: budget.exhaustion(kind, false, tokio::time::Instant::now()) };
                            return;
                        }
                        // An unavailable reviewer leaves its marker on the
                        // retained result and the run continues; only a RED
                        // verdict asks for correction.
                        if !verdict.outcome.allows_progress()
                            && verdict.outcome != runtime::CheckpointOutcome::Unavailable
                        {
                            correction.push(format!("{}: {}", call.name, verdict.feedback));
                        }
                    }
                    // Both correction attempts spent: the RED markers stay on
                    // the results as evidence and the model proceeds without
                    // another redirect, instead of the run failing.
                    if !correction.is_empty() && context.repair() {
                        checkpoint_correction_notice = Some(format!(
                            "JEV RED. Do not claim completion. Produce fresh direct evidence or correct the work, then retry one tool call. Feedback:\n- {}",
                            correction.join("\n- ")
                        ));
                    }
                }
                // The per-turn output budget: results enter context in call
                // order, and a late result that would overshoot is re-bounded
                // to the remainder. The persisted row keeps the per-call
                // bounded text; context assembly re-applies this same
                // projection to the stored rows (`append_run_turns`), so a
                // replayed turn is byte-identical to what the model saw here.
                // A cut names its recall path: the spill when the call
                // spilled, else the stored result row itself in a session run.
                let mut turn_output = tools::TurnOutputBudget::new();
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
                        turn_output.admit(&mut content, recall);
                        budget.charge_tool_output(content.len());
                        ContentBlock::ToolResult {
                            call_id: call.provider_call_id.clone(),
                            content,
                            is_error: result.is_error,
                        }
                    })
                    .collect();
                let tool_results = Message::tool_results(result_blocks);
                irreducible_message_bytes = irreducible_message_bytes
                    .saturating_add(measure_message(&tool_results));
                Arc::make_mut(&mut messages).push(tool_results);
                if let Some(notice) = checkpoint_correction_notice.take() {
                    let notice = Message::user(notice);
                    irreducible_message_bytes = irreducible_message_bytes
                        .saturating_add(measure_message(&notice));
                    Arc::make_mut(&mut messages).push(notice);
                }
                // The final-answer turn's calls never ran; their results are
                // durable, so the run completes with the turn as it stands
                // (ADR-0054 § 3). Steering is left for the child's next run.
                if final_answer_turn {
                    // As at any completion: a run that overran its cost or
                    // token bound settles as exhausted, never completed.
                    if let Some(kind) = budget.exceeded(tokio::time::Instant::now())
                        && matches!(
                            kind,
                            BudgetLimitKind::Cost
                                | BudgetLimitKind::CostUnknown
                                | BudgetLimitKind::TotalTokens
                        )
                    {
                        let exhaustion = budget.exhaustion(kind, false, tokio::time::Instant::now());
                        yield RuntimeEvent::BudgetExhausted { exhaustion };
                        return;
                    }
                    yield RuntimeEvent::Completed { final_output: None };
                    return;
                }
                // The boundary: every result of this turn is in context, and
                // the next request has not been built. Steering joins here as
                // a user message after the tool results.
                if let Some(applied) = apply_steering(&mut stall, &mut steering, Arc::make_mut(&mut messages), &mut irreducible_message_bytes, checkpoint_context.as_mut(), &workspace, &file_state).await {
                    for steer in applied {
                        yield RuntimeEvent::SteeringApplied {
                            message_id: steer.message_id,
                            turn_ordinal: turn_ordinal.saturating_add(1),
                            attachments: steer.attachments,
                        };
                    }
                }
            }

            yield RuntimeEvent::Failed {
                kind: RunFailureKind::Policy,
                message: "run exhausted the durable u32 model-turn ordinal space".to_owned(),
            };
        });
        match (deadline, deadline_resources) {
            (Some(deadline), Some((cancelled, tools, spawner, audit_hook))) => {
                deadline.enforce(events, cancelled, tools, spawner, audit_hook)
            }
            _ => events,
        }
    }
}
