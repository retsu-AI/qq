//! Command application inside the store transaction: `execute_command` for
//! every `SessionCommand`, child-run creation, steering, session deletion, and
//! workspace/session bookkeeping.

use super::*;

pub(super) struct AppliedCommand {
    pub(super) receipt: CommandReceipt,
    pub(super) schedule: bool,
    /// Other runs whose in-memory cancellation must be signalled with this
    /// command: an auto-compaction made unnecessary by its queued prompt, or
    /// running child work durably owned by a cancelled parent.
    pub(super) cascade_cancels: Vec<RunId>,
    /// Wakes the single promotion worker when this command has durable outbox
    /// work. Replays retain the signal while the same row remains pending.
    pub(super) grant_promotion_pending: bool,
    /// The receipt was replayed from the command journal; in-memory signals
    /// that already fired the first time (steering hand-off) must not repeat.
    pub(super) replayed: bool,
}

pub(super) struct CreatedChildRun {
    pub(super) session_id: SessionId,
    pub(super) run_id: RunId,
    pub(super) committed_through: EventCursor,
}

/// One approve-for-workspace promotion carried out of the command
/// transaction. The durable config write happens after the approval commits,
/// so a promotion failure can never fail the approval that requested it.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct PendingGrantPromotion {
    pub(super) workspace_id: WorkspaceId,
    pub(super) workspace_path: String,
    pub(super) session_id: SessionId,
    pub(super) run_id: RunId,
    pub(super) command_id: CommandId,
    pub(super) grant: ApprovalGrant,
}

/// The durable identity of the run that owns a new child.
#[derive(Clone, Copy)]
pub(super) struct ChildRunParent {
    pub(super) workspace_id: WorkspaceId,
    pub(super) session_id: SessionId,
    pub(super) run_id: RunId,
    /// The `spawn_agent` call that requested the child, when known.
    pub(super) tool_call_id: Option<ToolCallId>,
    /// The parent's own depth; the child is one deeper.
    pub(super) depth: u16,
    /// The root run of the tree the child joins.
    pub(super) root_run_id: RunId,
}

/// What a new child is admitted with: its model, task, remaining budget, and
/// the authority and purpose its parent granted.
pub(super) struct ChildAdmission {
    pub(super) profile: AgentProfileId,
    pub(super) model: ModelSelection,
    /// The child's own effort pin. `None` inherits the parent session's.
    pub(super) reasoning_effort: Option<qq_provider::ReasoningEffort>,
    pub(super) task: String,
    pub(super) limits: RunLimits,
    pub(super) approval_mode: ApprovalMode,
    pub(super) purpose: SessionPurpose,
}

/// Most sessions one root run's delegation tree may hold across every depth.
/// Bounds fan-out where per-run child caps alone would not (eight children
/// each spawning eight).
pub const MAX_DESCENDANTS_PER_ROOT: u16 = 24;

pub(super) fn create_child_run(
    connection: &mut Connection,
    store_id: StoreId,
    parent: ChildRunParent,
    admission: ChildAdmission,
) -> Result<CreatedChildRun, SessionRuntimeError> {
    let ChildAdmission {
        profile,
        model,
        reasoning_effort,
        task,
        limits,
        approval_mode,
        purpose,
    } = admission;
    // Children never hold more than Supervised authority; the spawner decides
    // between ReadOnly and Supervised and nothing else may reach here.
    if !matches!(
        approval_mode,
        ApprovalMode::ReadOnly | ApprovalMode::Supervised
    ) {
        return Err(SessionRuntimeError::ChildAuthorityEscalation);
    }
    let ChildRunParent {
        workspace_id,
        session_id: parent_session_id,
        run_id: parent_run_id,
        tool_call_id: spawned_by_tool_call_id,
        depth: parent_depth,
        root_run_id,
    } = parent;
    let depth = parent_depth.saturating_add(1);
    if depth > MAX_CHILD_DEPTH {
        return Err(SessionRuntimeError::ChildDepthExceeded);
    }
    validate_model_selection(&model)?;
    validate_run_limits(&limits)?;
    let limits_json = if limits.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&limits)?)
    };
    let task = task.trim().to_owned();
    if task.is_empty() {
        return Err(SessionRuntimeError::EmptyPrompt);
    }
    if task.len() > MAX_PROMPT_BYTES {
        return Err(SessionRuntimeError::PromptTooLarge);
    }
    let transaction = store::begin_unit(connection)?;
    ensure_workspace(&transaction, workspace_id)?;
    let parent_workspace = transaction
        .query_row(
            "SELECT s.workspace_id
             FROM sessions s JOIN runs r ON r.session_id = s.id
             WHERE s.id = ?1 AND r.id = ?2 AND r.status = 'running'
               AND r.cancel_requested = 0",
            params![parent_session_id.to_string(), parent_run_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or(SessionRuntimeError::RunNotFound)?;
    if parse_id::<WorkspaceId>(&parent_workspace)? != workspace_id {
        return Err(SessionRuntimeError::ParentWorkspaceMismatch);
    }
    let session_count: u32 = transaction.query_row(
        "SELECT COUNT(*) FROM sessions WHERE workspace_id = ?1",
        [workspace_id.to_string()],
        |row| row.get(0),
    )?;
    if session_count >= MAX_SESSIONS_PER_WORKSPACE {
        return Err(SessionRuntimeError::SessionLimitReached);
    }
    let descendants: u32 = transaction.query_row(
        "SELECT COUNT(*) FROM sessions WHERE root_run_id = ?1",
        [root_run_id.to_string()],
        |row| row.get(0),
    )?;
    if descendants >= u32::from(MAX_DESCENDANTS_PER_ROOT) {
        return Err(SessionRuntimeError::DescendantLimitReached);
    }

    let session_id = SessionId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    let run_id = RunId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    // This is one internal operation rather than a replayable client command;
    // a generated id satisfies the run's uniqueness contract and links both
    // child events to the same atomic cause.
    let command_id = CommandId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    let user_message_id = MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    let assistant_message_id =
        MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    let now = now_ms();
    transaction
        .execute(
            "INSERT INTO sessions(
                id, workspace_id, parent_id, owner_run_id, spawned_by_tool_call_id, title,
                status, queued_prompts, model, max_output_tokens, organization, approval_mode,
                created_at_ms, updated_at_ms, depth, root_run_id, purpose, profile, model_is_fallback,
                reasoning_effort, approval_delegate
             ) VALUES (?1, ?2, ?3, ?4, ?10, ?5, 'queued', 1, ?6, ?7, ?8, ?11, ?9, ?9, ?12, ?13, ?14, ?15, ?16,
                COALESCE(?17, (SELECT reasoning_effort FROM sessions WHERE id = ?3)),
                (SELECT approval_delegate FROM sessions WHERE id = ?3))",
            params![
                session_id.to_string(),
                workspace_id.to_string(),
                parent_session_id.to_string(),
                parent_run_id.to_string(),
                prompt_title(&task),
                model.model,
                model.max_output_tokens,
                model.organization,
                now,
                spawned_by_tool_call_id.map(|id| id.to_string()),
                approval_mode_str(approval_mode),
                depth,
                root_run_id.to_string(),
                purpose.as_str(),
                profile.as_str(),
                model.model_is_fallback,
                super::codec::reasoning_effort_column(reasoning_effort),
            ],
        )
        ?;
    transaction.execute(
        "INSERT INTO runs(
                id, session_id, command_id, user_message_id, assistant_message_id,
                status, created_at_ms, limits_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?7)",
        params![
            run_id.to_string(),
            session_id.to_string(),
            command_id.to_string(),
            user_message_id.to_string(),
            assistant_message_id.to_string(),
            now,
            limits_json,
        ],
    )?;
    transaction.execute(
        "INSERT INTO messages(
                id, session_id, run_id, ordinal, role, state, output, created_at_ms
             ) VALUES (?1, ?2, ?3, 1, 'user', 'queued', ?4, ?5)",
        params![
            user_message_id.to_string(),
            session_id.to_string(),
            run_id.to_string(),
            task,
            now,
        ],
    )?;

    let session = load_session_summary(&transaction, session_id)?;
    let created = append_event(
        &transaction,
        EventContext::for_run_ids(
            store_id,
            workspace_id,
            session_id,
            run_id,
            Some(command_id),
            now,
        ),
        SessionEvent::SessionCreated {
            session: Box::new(session.clone()),
        },
    )?;
    let message = load_message(&transaction, user_message_id)?;
    let run = load_run(&transaction, run_id)?;
    let queued = append_event(
        &transaction,
        EventContext::for_run_ids(
            store_id,
            workspace_id,
            session_id,
            run_id,
            Some(command_id),
            now,
        ),
        SessionEvent::PromptQueued {
            session: Box::new(session),
            message,
            run: Box::new(run),
            queue_position: 1,
        },
    )?;
    // An audit child announces itself on the parent run too, atomically with
    // its creation, so a client watching the parent learns which session is
    // the audit before any of its events arrive.
    let committed_through = if purpose == SessionPurpose::Audit {
        append_event(
            &transaction,
            EventContext::for_run_ids(
                store_id,
                workspace_id,
                parent_session_id,
                parent_run_id,
                Some(command_id),
                now,
            ),
            SessionEvent::RunAuditStarted {
                run_id: parent_run_id,
                audit_session_id: session_id,
            },
        )?
        .cursor
    } else {
        queued.cursor
    };
    transaction.commit()?;
    debug_assert_eq!(created.cursor.sequence + 1, queued.cursor.sequence);
    Ok(CreatedChildRun {
        session_id,
        run_id,
        committed_through,
    })
}

/// `canonical_workspace` is the resolved path for `ResolveWorkspace`, computed
/// by the caller on a blocking thread; the command itself is journaled as
/// submitted so idempotency compares what the client sent.
/// Who is asking, for the receipt bound. A store admits new work up to
/// `MAX_COMMANDS`; a client's control and cleanup commands up to the headroom
/// above it; and a cancel the runtime itself issues while settling (shutdown,
/// a parent settling its child) always, because refusing it would leave a run
/// unsettleable, which no capacity bound may do.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CommandOrigin {
    Client,
    /// A `CancelRun` the runtime issues on its own behalf during settlement.
    RuntimeSettlement,
}

pub(super) fn execute_command(
    connection: &mut Connection,
    store_id: StoreId,
    command_id: CommandId,
    command: SessionCommand,
    canonical_workspace: Option<Result<String, SessionRuntimeError>>,
    seed: &WorkspaceGrantSeed,
    origin: CommandOrigin,
) -> Result<AppliedCommand, SessionRuntimeError> {
    let request_json = serde_json::to_string(&command)?;
    if let Some((stored_request, stored_receipt)) = connection
        .query_row(
            "SELECT request_json, receipt_json FROM commands WHERE id = ?1",
            [command_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
    {
        if stored_request != request_json {
            return Err(SessionRuntimeError::IdempotencyConflict);
        }
        let receipt = serde_json::from_str(&stored_receipt)?;
        return Ok(AppliedCommand {
            receipt,
            schedule: false,
            replayed: true,
            cascade_cancels: match &command {
                SessionCommand::CancelRun { run_id } => {
                    cancellation_signal_run_ids(connection, *run_id)?
                }
                _ => Vec::new(),
            },
            grant_promotion_pending: connection.query_row(
                "SELECT EXISTS(
                         SELECT 1 FROM pending_workspace_grant_promotions
                         WHERE command_id = ?1
                     )",
                [command_id.to_string()],
                |row| row.get(0),
            )?,
        });
    }
    // The counter is maintained beside every insert (schema 25) so the bound
    // costs one row read instead of a table scan per command.
    let command_count: u32 = connection
        .prepare_cached("SELECT value FROM metadata WHERE key = 'command_count'")
        .and_then(|mut statement| statement.query_row([], |row| row.get::<_, String>(0)))?
        .parse()
        .map_err(|_| SessionRuntimeError::CODEC)?;
    // Control and cleanup commands are admitted past the new-work bound so a
    // store at the cap can still stop, resolve, and remove what it holds;
    // their own bound is the headroom above it. A settlement cancel the
    // runtime issues for itself is never refused: the alternative is a run
    // that can never settle.
    let bound = match (origin, command.kind().creates_work()) {
        (CommandOrigin::RuntimeSettlement, _) => None,
        (CommandOrigin::Client, true) => Some(MAX_COMMANDS),
        (CommandOrigin::Client, false) => Some(MAX_COMMANDS_WITH_CONTROL_HEADROOM),
    };
    if bound.is_some_and(|bound| command_count >= bound) {
        return Err(SessionRuntimeError::CommandLimitReached);
    }

    let transaction = store::begin_unit(connection)?;
    let now = now_ms();
    let mut grant_promotion_pending = false;
    let mut cascade_cancels = Vec::new();
    let (receipt, schedule) = match command {
        SessionCommand::ResolveWorkspace { .. } => {
            let canonical = canonical_workspace.ok_or(SessionRuntimeError::InvalidWorkspace)??;
            let path = canonical.as_str();
            let existing = transaction
                .query_row(
                    "SELECT id, next_sequence FROM workspaces WHERE path = ?1",
                    [path],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)),
                )
                .optional()?;
            let (workspace_id, sequence) = match existing {
                Some((id, sequence)) => (parse_id(&id)?, sequence),
                None => {
                    let workspace_count: u32 =
                        transaction
                            .query_row("SELECT COUNT(*) FROM workspaces", [], |row| row.get(0))?;
                    if workspace_count >= MAX_WORKSPACES {
                        return Err(SessionRuntimeError::WorkspaceLimitReached);
                    }
                    let workspace_id =
                        WorkspaceId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
                    transaction.execute(
                        "INSERT INTO workspaces(id, path, next_sequence) VALUES (?1, ?2, 0)",
                        params![workspace_id.to_string(), path],
                    )?;
                    (workspace_id, 0)
                }
            };
            (
                CommandReceipt {
                    command_id,
                    committed_through: EventCursor {
                        store_id,
                        workspace_id,
                        sequence,
                    },
                    outcome: CommandOutcome::WorkspaceResolved { workspace_id },
                },
                false,
            )
        }
        SessionCommand::CreateSession {
            workspace_id,
            parent_id,
            model,
            approval_mode,
            profile,
            reasoning_effort,
            correlation,
        } => {
            validate_model_selection(&model)?;
            let correlation_json = encode_correlation(&correlation)?;
            ensure_workspace(&transaction, workspace_id)?;
            let session_count: u32 = transaction.query_row(
                "SELECT COUNT(*) FROM sessions WHERE workspace_id = ?1",
                [workspace_id.to_string()],
                |row| row.get(0),
            )?;
            if session_count >= MAX_SESSIONS_PER_WORKSPACE {
                return Err(SessionRuntimeError::SessionLimitReached);
            }
            // A publicly parented session sits one level below its parent and
            // shares its parent's tree, so depth caps and cascades treat it
            // like a spawned child even though no run owns it.
            let mut depth = 0_u16;
            let mut root_run_id: Option<String> = None;
            if let Some(parent_id) = parent_id {
                let (parent_workspace, parent_depth, parent_root) = transaction
                    .query_row(
                        "SELECT workspace_id, depth, root_run_id FROM sessions WHERE id = ?1",
                        [parent_id.to_string()],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, u16>(1)?,
                                row.get::<_, Option<String>>(2)?,
                            ))
                        },
                    )
                    .optional()?
                    .ok_or(SessionRuntimeError::SessionNotFound)?;
                if parse_id::<WorkspaceId>(&parent_workspace)? != workspace_id {
                    return Err(SessionRuntimeError::ParentWorkspaceMismatch);
                }
                depth = parent_depth.saturating_add(1);
                if depth > MAX_CHILD_DEPTH {
                    return Err(SessionRuntimeError::ChildDepthExceeded);
                }
                root_run_id = parent_root;
            }
            let session_id = SessionId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            transaction
                .execute(
                    "INSERT INTO sessions(
                        id, workspace_id, parent_id, title, status, model,
                        max_output_tokens, organization, approval_mode,
                        created_at_ms, updated_at_ms, profile, correlation_json, depth,
                        root_run_id, model_is_fallback, reasoning_effort, approval_delegate
                     ) VALUES (?1, ?2, ?3, 'New session', 'idle', ?4, ?5, ?6, ?7, ?8, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                        (SELECT approval_delegate FROM sessions WHERE id = ?3))",
                    params![
                        session_id.to_string(),
                        workspace_id.to_string(),
                        parent_id.map(|id| id.to_string()),
                        model.model,
                        model.max_output_tokens,
                        model.organization,
                        approval_mode_str(approval_mode),
                        now,
                        (!profile.is_default()).then(|| profile.as_str().to_owned()),
                        correlation_json,
                        depth,
                        root_run_id,
                        model.model_is_fallback,
                        reasoning_effort_column(reasoning_effort),
                    ],
                )
                ?;
            insert_seed_grants(&transaction, session_id, seed, now)?;
            let summary = load_session_summary(&transaction, session_id)?;
            let event = append_event(
                &transaction,
                EventContext::for_session(
                    store_id,
                    workspace_id,
                    session_id,
                    Some(command_id),
                    now,
                ),
                SessionEvent::SessionCreated {
                    session: Box::new(summary),
                },
            )?;
            (
                CommandReceipt {
                    command_id,
                    committed_through: event.cursor,
                    outcome: CommandOutcome::SessionCreated { session_id },
                },
                false,
            )
        }
        SessionCommand::SubmitPrompt {
            session_id,
            input,
            limits,
            correlation,
            output,
        } => {
            // Syntactic bounds only: file parts are read when the run starts,
            // so admission never performs I/O and a stale attachment fails
            // the run, not the command.
            match validate_input(&input) {
                Ok(()) => {}
                Err(qq_protocol::InputError::Empty | qq_protocol::InputError::Blank) => {
                    return Err(SessionRuntimeError::EmptyPrompt);
                }
                Err(qq_protocol::InputError::TextTooLarge { .. }) => {
                    return Err(SessionRuntimeError::PromptTooLarge);
                }
                Err(error) => return Err(SessionRuntimeError::InvalidInput(error)),
            }
            validate_run_limits(&limits)?;
            // The contract is compiled here only to reject it: a schema the
            // runtime cannot enforce must fail the command, not the run. The
            // claim recompiles from the persisted JSON so a restart enforces
            // exactly what the caller accepted.
            let output_contract_json = match &output {
                None => None,
                Some(contract) => {
                    crate::output::CompiledOutputSchema::compile(contract)
                        .map_err(SessionRuntimeError::InvalidOutputContract)?;
                    Some(serde_json::to_string(contract)?)
                }
            };
            let correlation_json = encode_correlation(&correlation)?;
            let input_json = serde_json::to_string(&input)?;
            // The transcript row carries the rendered text: text parts
            // verbatim, attachments as `@path` placeholders. Slash escaping
            // applies to the rendered text exactly as it did to the string.
            let prompt = crate::input::render_text(&input).trim().to_owned();
            // Slash names the runtime can never resolve fail the command, not
            // the run: a run row would only add a "previous run failed"
            // notice to the next prompt.
            runtime::validate_slash_prompt(&prompt)
                .map_err(SessionRuntimeError::InvalidSlashCommand)?;
            let prompt = prompt
                .strip_prefix("//")
                .map_or(prompt.clone(), |literal| format!("/{literal}"));
            let (workspace_id, queued, title) = transaction
                .query_row(
                    "SELECT workspace_id, queued_prompts, title FROM sessions WHERE id = ?1",
                    [session_id.to_string()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, u16>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()?
                .ok_or(SessionRuntimeError::SessionNotFound)?;
            if queued >= MAX_PENDING_PROMPTS {
                return Err(SessionRuntimeError::QueueFull);
            }
            // An over-budget prompt is admitted rather than rejected: claiming
            // it auto-compacts the session first and re-checks, so the hard
            // budget fails the run only after one compaction attempt could
            // not shrink the assembly under it (the last resort, not the
            // policy).
            let workspace_id = parse_id(&workspace_id)?;
            let run_id = RunId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            let message_id = MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            // Assistant message rows are created lazily, one per model turn,
            // at each turn's first text delta. The run row's
            // assistant_message_id starts as a placeholder and is updated to
            // the current turn's message as the run advances.
            let assistant_message_id =
                MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            let ordinal: u64 = transaction.query_row(
                "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM messages WHERE session_id = ?1",
                [session_id.to_string()],
                |row| row.get(0),
            )?;
            // Limits are persisted with the run row so a restart enforces the
            // bound the caller accepted, never a later default. An empty
            // set stores NULL, matching historical unlimited runs.
            let limits_json = if limits.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&limits)?)
            };
            transaction.execute(
                "INSERT INTO runs(
                        id, session_id, command_id, user_message_id, assistant_message_id,
                        status, created_at_ms, limits_json, input_json, correlation_json,
                        output_contract_json
                     ) VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?7, ?8, ?9, ?10)",
                params![
                    run_id.to_string(),
                    session_id.to_string(),
                    command_id.to_string(),
                    message_id.to_string(),
                    assistant_message_id.to_string(),
                    now,
                    limits_json,
                    input_json,
                    correlation_json,
                    output_contract_json,
                ],
            )?;
            transaction.execute(
                "INSERT INTO messages(
                        id, session_id, run_id, ordinal, role, state, output, created_at_ms,
                        input_json
                     ) VALUES (?1, ?2, ?3, ?4, 'user', 'queued', ?5, ?6, ?7)",
                params![
                    message_id.to_string(),
                    session_id.to_string(),
                    run_id.to_string(),
                    ordinal,
                    prompt,
                    now,
                    input_json,
                ],
            )?;
            let next_queued = queued + 1;
            let next_title = if ordinal == 1 {
                prompt_title(&prompt)
            } else {
                title
            };
            transaction
                .execute(
                    "UPDATE sessions
                     SET title = ?2, status = CASE WHEN active_run_id IS NULL THEN 'queued' ELSE status END,
                         queued_prompts = ?3, updated_at_ms = ?4
                     WHERE id = ?1",
                    params![session_id.to_string(), next_title, next_queued, now],
                )
                ?;
            let summary = load_session_summary(&transaction, session_id)?;
            let message = load_message(&transaction, message_id)?;
            let run = load_run(&transaction, run_id)?;
            let event = append_event(
                &transaction,
                EventContext::for_run_ids(
                    store_id,
                    workspace_id,
                    session_id,
                    run_id,
                    Some(command_id),
                    now,
                ),
                SessionEvent::PromptQueued {
                    session: Box::new(summary),
                    message,
                    run: Box::new(run),
                    queue_position: next_queued,
                },
            )?;
            (
                CommandReceipt {
                    command_id,
                    committed_through: event.cursor,
                    outcome: CommandOutcome::PromptQueued {
                        session_id,
                        run_id,
                        queue_position: next_queued,
                    },
                },
                true,
            )
        }
        SessionCommand::SteerRun {
            run_id,
            input,
            interrupt: _,
        } => {
            match validate_input(&input) {
                Ok(()) => {}
                Err(qq_protocol::InputError::Empty | qq_protocol::InputError::Blank) => {
                    return Err(SessionRuntimeError::EmptyPrompt);
                }
                Err(qq_protocol::InputError::TextTooLarge { .. }) => {
                    return Err(SessionRuntimeError::PromptTooLarge);
                }
                Err(error) => return Err(SessionRuntimeError::InvalidInput(error)),
            }
            let (session_id, status, stored_outcome, kind) = transaction
                .query_row(
                    "SELECT session_id, status, outcome_json, kind FROM runs WHERE id = ?1",
                    [run_id.to_string()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    },
                )
                .optional()?
                .ok_or(SessionRuntimeError::RunNotFound)?;
            let session_id = parse_id(&session_id)?;
            let workspace_id = session_workspace(&transaction, session_id)?;
            if let Some(outcome) = stored_outcome {
                let outcome = serde_json::from_str(&outcome)?;
                let sequence = workspace_sequence(&transaction, workspace_id)?;
                (
                    CommandReceipt {
                        command_id,
                        committed_through: EventCursor {
                            store_id,
                            workspace_id,
                            sequence,
                        },
                        outcome: CommandOutcome::RunAlreadyFinished { run_id, outcome },
                    },
                    false,
                )
            } else {
                // Only an executing prompt run has a boundary to steer at. A
                // queued run has not started: the client submits a new prompt
                // or cancels instead. Compaction runs take no user input.
                if status != "running" || parse_run_kind(&kind)? != RunKind::Prompt {
                    return Err(SessionRuntimeError::RunNotSteerable);
                }
                let pending: u16 = transaction.query_row(
                    "SELECT COUNT(*) FROM messages
                         WHERE run_id = ?1 AND steering = 1 AND state = 'queued'",
                    [run_id.to_string()],
                    |row| row.get(0),
                )?;
                if pending >= crate::runtime::MAX_PENDING_STEERING {
                    return Err(SessionRuntimeError::SteeringQueueFull);
                }
                let message_id =
                    MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
                let ordinal: u64 = transaction.query_row(
                    "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM messages WHERE session_id = ?1",
                    [session_id.to_string()],
                    |row| row.get(0),
                )?;
                let input_json = serde_json::to_string(&input)?;
                let text = crate::input::render_text(&input).trim().to_owned();
                transaction.execute(
                    "INSERT INTO messages(
                            id, session_id, run_id, ordinal, role, state, output, created_at_ms,
                            input_json, steering
                         ) VALUES (?1, ?2, ?3, ?4, 'user', 'queued', ?5, ?6, ?7, 1)",
                    params![
                        message_id.to_string(),
                        session_id.to_string(),
                        run_id.to_string(),
                        ordinal,
                        text,
                        now,
                        input_json,
                    ],
                )?;
                let message = load_message(&transaction, message_id)?;
                let event = append_event(
                    &transaction,
                    EventContext::for_run_ids(
                        store_id,
                        workspace_id,
                        session_id,
                        run_id,
                        Some(command_id),
                        now,
                    ),
                    SessionEvent::SteeringQueued { run_id, message },
                )?;
                (
                    CommandReceipt {
                        command_id,
                        committed_through: event.cursor,
                        outcome: CommandOutcome::SteeringQueued { run_id, message_id },
                    },
                    false,
                )
            }
        }
        SessionCommand::CancelRun { run_id } => {
            let (session_id, status, stored_outcome) = transaction
                .query_row(
                    "SELECT session_id, status, outcome_json FROM runs WHERE id = ?1",
                    [run_id.to_string()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .optional()?
                .ok_or(SessionRuntimeError::RunNotFound)?;
            let session_id = parse_id(&session_id)?;
            let workspace_id = session_workspace(&transaction, session_id)?;
            if let Some(outcome) = stored_outcome {
                let outcome = serde_json::from_str(&outcome)?;
                let sequence = workspace_sequence(&transaction, workspace_id)?;
                (
                    CommandReceipt {
                        command_id,
                        committed_through: EventCursor {
                            store_id,
                            workspace_id,
                            sequence,
                        },
                        outcome: CommandOutcome::RunAlreadyFinished { run_id, outcome },
                    },
                    false,
                )
            } else {
                transaction.execute(
                    "UPDATE runs SET cancel_requested = 1 WHERE id = ?1",
                    [run_id.to_string()],
                )?;
                let summary = load_session_summary(&transaction, session_id)?;
                let requested = append_event(
                    &transaction,
                    EventContext::for_run_ids(
                        store_id,
                        workspace_id,
                        session_id,
                        run_id,
                        Some(command_id),
                        now,
                    ),
                    SessionEvent::CancellationRequested {
                        session: Box::new(summary),
                        run_id,
                    },
                )?;
                let mut cursor = if status == "queued" {
                    let finished = finish_queued_run(
                        &transaction,
                        store_id,
                        workspace_id,
                        session_id,
                        run_id,
                        now,
                    )?;
                    // Cancelling the session's last queued prompt cascades to
                    // the auto-compaction running on its behalf: with nothing
                    // left to run after it, the summarization is pure cost. A
                    // manual compaction (auto_compaction = 0) is never
                    // cascaded — the user asked for it directly.
                    match cascade_auto_compaction_cancel(
                        &transaction,
                        store_id,
                        workspace_id,
                        session_id,
                        command_id,
                        now,
                    )? {
                        Some((compaction_run, event)) => {
                            cascade_cancels.push(compaction_run);
                            event.cursor
                        }
                        None => finished.cursor,
                    }
                } else {
                    // A running prompt may own an in-run compaction that is
                    // summarizing on its behalf right now; cancelling the
                    // prompt cancels that too, so neither waits on a stalled
                    // summarizer.
                    match cascade_in_run_compaction_cancel(
                        &transaction,
                        store_id,
                        workspace_id,
                        session_id,
                        run_id,
                        command_id,
                        now,
                    )? {
                        Some((compaction_run, event)) => {
                            cascade_cancels.push(compaction_run);
                            event.cursor
                        }
                        None => requested.cursor,
                    }
                };
                let owned =
                    cancel_owned_child_runs(&transaction, store_id, run_id, command_id, now)?;
                if let Some(child_cursor) = owned.committed_through {
                    cursor = child_cursor;
                }
                cascade_cancels.extend(owned.running);
                (
                    CommandReceipt {
                        command_id,
                        committed_through: cursor,
                        outcome: CommandOutcome::CancellationRequested { run_id },
                    },
                    status == "queued" || owned.settled_queued,
                )
            }
        }
        SessionCommand::RespondToolApproval {
            run_id,
            tool_call_id,
            decision,
        } => {
            let (call_run, state, resolution, provider_call_id, first_result_in_turn, arguments) =
                transaction
                    .query_row(
                        "SELECT current.run_id, current.state, current.approval_resolution,
                            current.provider_call_id,
                            NOT EXISTS(
                                SELECT 1 FROM tool_calls previous
                                WHERE previous.run_id = current.run_id
                                  AND previous.turn_ordinal = current.turn_ordinal
                                  AND previous.result IS NOT NULL
                            ),
                            current.arguments_json
                     FROM tool_calls current WHERE current.id = ?1",
                        [tool_call_id.to_string()],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, Option<String>>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, bool>(4)?,
                                row.get::<_, String>(5)?,
                            ))
                        },
                    )
                    .optional()?
                    .ok_or(SessionRuntimeError::ToolCallNotFound)?;
            if parse_id::<RunId>(&call_run)? != run_id {
                return Err(SessionRuntimeError::ToolCallNotFound);
            }
            let session_id: SessionId = transaction
                .query_row(
                    "SELECT session_id FROM runs WHERE id = ?1",
                    [run_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .map_err(|_| SessionRuntimeError::CONSTRAINT)
                .and_then(|session| parse_id(&session))?;
            let workspace_id = session_workspace(&transaction, session_id)?;
            if let Some(resolution) = resolution {
                // Idempotent: a second response returns the recorded outcome
                // without touching the call again.
                let resolution = parse_approval_resolution(&resolution)?;
                let sequence = workspace_sequence(&transaction, workspace_id)?;
                (
                    CommandReceipt {
                        command_id,
                        committed_through: EventCursor {
                            store_id,
                            workspace_id,
                            sequence,
                        },
                        outcome: CommandOutcome::ToolApprovalResolved {
                            tool_call_id,
                            resolution,
                        },
                    },
                    false,
                )
            } else {
                if state != "awaiting_approval" {
                    return Err(SessionRuntimeError::ApprovalNotPending);
                }
                // A session or workspace choice whose grant cannot be stored
                // still approves this call. Refusing the whole command is what
                // produced "approval grant is empty or exceeds the session
                // limit" and left the call waiting after the user had already
                // approved it. The grant is dropped; the call runs once.
                let mut grant_not_recorded = false;
                let resolution = match &decision {
                    ApprovalDecision::ApproveOnce => ApprovalResolution::ApprovedOnce,
                    ApprovalDecision::ApproveForSession { grant }
                    | ApprovalDecision::ApproveForWorkspace { grant }
                        if !session_grant_recordable(&transaction, session_id, grant)? =>
                    {
                        grant_not_recorded = true;
                        ApprovalResolution::ApprovedOnce
                    }
                    ApprovalDecision::ApproveForSession { .. } => {
                        ApprovalResolution::ApprovedForSession
                    }
                    ApprovalDecision::ApproveForWorkspace { .. } => {
                        ApprovalResolution::ApprovedForWorkspace
                    }
                    ApprovalDecision::Deny => ApprovalResolution::Denied,
                    ApprovalDecision::Answer { .. } => ApprovalResolution::Answered,
                };
                match &decision {
                    ApprovalDecision::ApproveOnce
                    | ApprovalDecision::ApproveForSession { .. }
                    | ApprovalDecision::ApproveForWorkspace { .. } => {
                        transaction.execute(
                            "UPDATE tool_calls
                                 SET state = 'requested', approval_resolution = ?2,
                                     resolved_at_ms = ?3
                                 WHERE id = ?1 AND state = 'awaiting_approval'",
                            params![
                                tool_call_id.to_string(),
                                approval_resolution_str(resolution),
                                now,
                            ],
                        )?;
                    }
                    ApprovalDecision::Deny => {
                        reserve_tool_result_capacity(
                            &transaction,
                            run_id,
                            &provider_call_id,
                            approval::USER_DENIED_RESULT,
                            first_result_in_turn,
                        )?;
                        transaction.execute(
                            "UPDATE tool_calls
                                 SET state = 'denied', result = ?2, is_error = 1,
                                     approval_resolution = ?3, resolved_at_ms = ?4,
                                     finished_at_ms = ?4
                                 WHERE id = ?1 AND state = 'awaiting_approval'",
                            params![
                                tool_call_id.to_string(),
                                approval::USER_DENIED_RESULT,
                                approval_resolution_str(resolution),
                                now,
                            ],
                        )?;
                    }
                    // An answer settles the call as completed: the rendered
                    // questions and answers are its result. Answering a call
                    // that asked nothing (or an empty answer set) declines.
                    ApprovalDecision::Answer { answers } => {
                        let preview = crate::tools::ask::parse(&arguments)
                            .map_err(|_| SessionRuntimeError::ApprovalNotPending)?;
                        let declined = answers.iter().all(|answer| answer.trim().is_empty());
                        let result = if declined {
                            approval::DECLINED_QUESTION_RESULT.to_owned()
                        } else {
                            crate::tools::ask::render_answers(&preview, answers)
                        };
                        reserve_tool_result_capacity(
                            &transaction,
                            run_id,
                            &provider_call_id,
                            &result,
                            first_result_in_turn,
                        )?;
                        transaction.execute(
                            "UPDATE tool_calls
                                 SET state = 'completed', result = ?2, is_error = 0,
                                     approval_resolution = ?3, resolved_at_ms = ?4,
                                     finished_at_ms = ?4
                                 WHERE id = ?1 AND state = 'awaiting_approval'",
                            params![
                                tool_call_id.to_string(),
                                result,
                                approval_resolution_str(resolution),
                                now,
                            ],
                        )?;
                    }
                }
                // Approve-for-workspace records the same session grant as
                // approve-for-session — the running session must proceed on
                // it immediately — and additionally schedules the promotion
                // below, outside this transaction. A grant that cannot be
                // stored was already folded into an once-approval above.
                if !grant_not_recorded
                    && let ApprovalDecision::ApproveForSession { grant }
                    | ApprovalDecision::ApproveForWorkspace { grant } = &decision
                {
                    let (kind, value) = session_grant_parts(grant);
                    transaction.execute(
                        "INSERT OR IGNORE INTO session_grants(
                                 session_id, kind, value, created_at_ms
                             ) VALUES (?1, ?2, ?3, ?4)",
                        params![session_id.to_string(), kind, value, now],
                    )?;
                }
                if !grant_not_recorded
                    && let ApprovalDecision::ApproveForWorkspace { grant } = &decision
                {
                    let workspace_path: String = transaction.query_row(
                        "SELECT path FROM workspaces WHERE id = ?1",
                        [workspace_id.to_string()],
                        |row| row.get(0),
                    )?;
                    let promotion = PendingGrantPromotion {
                        workspace_id,
                        workspace_path,
                        session_id,
                        run_id,
                        command_id,
                        grant: grant.clone(),
                    };
                    let pending_count: u32 = transaction.query_row(
                        "SELECT COUNT(*) FROM pending_workspace_grant_promotions",
                        [],
                        |row| row.get(0),
                    )?;
                    if pending_count >= MAX_PENDING_GRANT_PROMOTIONS {
                        return Err(SessionRuntimeError::Overloaded);
                    }
                    let promotion_json = serde_json::to_string(&promotion)?;
                    transaction.execute(
                        "INSERT INTO pending_workspace_grant_promotions(
                                 command_id, created_at_ms, promotion_json
                             ) VALUES (?1, ?2, ?3)",
                        params![command_id.to_string(), now, promotion_json],
                    )?;
                    grant_promotion_pending = true;
                }
                let tool_call = load_tool_call(&transaction, tool_call_id)?;
                let event = append_event(
                    &transaction,
                    EventContext::for_run_ids(
                        store_id,
                        workspace_id,
                        session_id,
                        run_id,
                        Some(command_id),
                        now,
                    ),
                    SessionEvent::ToolApprovalResolved {
                        tool_call,
                        resolution,
                        delegate: None,
                    },
                )?;
                (
                    CommandReceipt {
                        command_id,
                        committed_through: event.cursor,
                        outcome: CommandOutcome::ToolApprovalResolved {
                            tool_call_id,
                            resolution,
                        },
                    },
                    false,
                )
            }
        }
        SessionCommand::SetApprovalMode { session_id, mode } => {
            let workspace_id = session_workspace(&transaction, session_id)?;
            // A model-spawned child holds exactly the authority its parent
            // granted at spawn time. A client may lower it further but never
            // raise it: the parent's policy, not the client's, bounds what a
            // child can do to the workspace.
            let (owned, current): (bool, String) = transaction.query_row(
                "SELECT owner_run_id IS NOT NULL, approval_mode FROM sessions WHERE id = ?1",
                [session_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if owned && approval_rank(mode) > approval_rank(parse_approval_mode(&current)?) {
                return Err(SessionRuntimeError::ChildAuthorityEscalation);
            }
            transaction.execute(
                "UPDATE sessions SET approval_mode = ?2, updated_at_ms = ?3 WHERE id = ?1",
                params![session_id.to_string(), approval_mode_str(mode), now],
            )?;
            // The mode is read when each approval is evaluated, so it applies
            // to the next held call; the summary carries it to every client.
            let summary = load_session_summary(&transaction, session_id)?;
            let event = append_event(
                &transaction,
                EventContext::for_session(
                    store_id,
                    workspace_id,
                    session_id,
                    Some(command_id),
                    now,
                ),
                SessionEvent::SessionUpdated {
                    session: Box::new(summary),
                },
            )?;
            (
                CommandReceipt {
                    command_id,
                    committed_through: event.cursor,
                    outcome: CommandOutcome::ApprovalModeSet { session_id, mode },
                },
                false,
            )
        }
        SessionCommand::SetApprovalDelegate {
            session_id,
            delegate,
        } => {
            let workspace_id = session_workspace(&transaction, session_id)?;
            // The mode stays the ceiling, so no authority check: this only
            // chooses who answers inside it, and every value asks at least
            // as much of a human as the configured choice could.
            let updated = transaction.execute(
                "UPDATE sessions SET approval_delegate = ?2, updated_at_ms = ?3 WHERE id = ?1",
                params![
                    session_id.to_string(),
                    approval_delegate_column(delegate),
                    now
                ],
            )?;
            if updated != 1 {
                return Err(SessionRuntimeError::SessionNotFound);
            }
            // Read by the gate at each held call, so a running session's next
            // hold already honors it; the summary carries it to every client.
            let summary = load_session_summary(&transaction, session_id)?;
            let event = append_event(
                &transaction,
                EventContext::for_session(
                    store_id,
                    workspace_id,
                    session_id,
                    Some(command_id),
                    now,
                ),
                SessionEvent::SessionUpdated {
                    session: Box::new(summary),
                },
            )?;
            (
                CommandReceipt {
                    command_id,
                    committed_through: event.cursor,
                    outcome: CommandOutcome::ApprovalDelegateSet {
                        session_id,
                        delegate,
                    },
                },
                false,
            )
        }
        SessionCommand::SetSessionModel { session_id, model } => {
            validate_model_selection(&model)?;
            let workspace_id = session_workspace(&transaction, session_id)?;
            transaction.execute(
                "UPDATE sessions
                     SET context_tokens = CASE
                             WHEN model IS ?2 THEN context_tokens ELSE NULL
                         END,
                         context_occupancy_json = CASE
                             WHEN model IS ?2 AND max_output_tokens IS ?3
                                  AND organization IS ?4
                             THEN context_occupancy_json ELSE NULL
                         END,
                         model = ?2, max_output_tokens = ?3, organization = ?4,
                         updated_at_ms = ?5, model_is_fallback = ?6
                     WHERE id = ?1",
                params![
                    session_id.to_string(),
                    &model.model,
                    model.max_output_tokens,
                    &model.organization,
                    now,
                    model.model_is_fallback,
                ],
            )?;
            // The new selection is read at claim time (`claim_next_run`), so
            // it applies to the next run; an executing run keeps the
            // `ClaimedRun` model it started with.
            let summary = load_session_summary(&transaction, session_id)?;
            let event = append_event(
                &transaction,
                EventContext::for_session(
                    store_id,
                    workspace_id,
                    session_id,
                    Some(command_id),
                    now,
                ),
                SessionEvent::SessionUpdated {
                    session: Box::new(summary),
                },
            )?;
            (
                CommandReceipt {
                    command_id,
                    committed_through: event.cursor,
                    outcome: CommandOutcome::SessionModelSet { session_id, model },
                },
                false,
            )
        }
        SessionCommand::SetSessionProfile {
            session_id,
            profile,
        } => {
            let workspace_id = session_workspace(&transaction, session_id)?;
            transaction.execute(
                "UPDATE sessions SET profile = ?2, updated_at_ms = ?3 WHERE id = ?1",
                params![
                    session_id.to_string(),
                    (!profile.is_default()).then(|| profile.as_str().to_owned()),
                    now,
                ],
            )?;
            let summary = load_session_summary(&transaction, session_id)?;
            let event = append_event(
                &transaction,
                EventContext::for_session(
                    store_id,
                    workspace_id,
                    session_id,
                    Some(command_id),
                    now,
                ),
                SessionEvent::SessionUpdated {
                    session: Box::new(summary),
                },
            )?;
            (
                CommandReceipt {
                    command_id,
                    committed_through: event.cursor,
                    outcome: CommandOutcome::SessionProfileSet {
                        session_id,
                        profile,
                    },
                },
                false,
            )
        }
        SessionCommand::SetSessionEffort { session_id, effort } => {
            let workspace_id = session_workspace(&transaction, session_id)?;
            transaction.execute(
                "UPDATE sessions SET reasoning_effort = ?2, updated_at_ms = ?3 WHERE id = ?1",
                params![session_id.to_string(), reasoning_effort_column(effort), now,],
            )?;
            let summary = load_session_summary(&transaction, session_id)?;
            let event = append_event(
                &transaction,
                EventContext::for_session(
                    store_id,
                    workspace_id,
                    session_id,
                    Some(command_id),
                    now,
                ),
                SessionEvent::SessionUpdated {
                    session: Box::new(summary),
                },
            )?;
            (
                CommandReceipt {
                    command_id,
                    committed_through: event.cursor,
                    outcome: CommandOutcome::SessionEffortSet { session_id, effort },
                },
                false,
            )
        }
        SessionCommand::DeleteSession { session_id } => {
            let workspace_id = session_workspace(&transaction, session_id)?;
            let event = delete_idle_session(
                &transaction,
                store_id,
                workspace_id,
                session_id,
                command_id,
                now,
            )?;
            (
                CommandReceipt {
                    command_id,
                    committed_through: event.cursor,
                    outcome: CommandOutcome::SessionDeleted { session_id },
                },
                false,
            )
        }
        SessionCommand::PruneSessions { workspace_id } => {
            ensure_workspace(&transaction, workspace_id)?;
            // Idle sessions that never received a message: the residue left
            // by creating sessions without prompting them. Anything with a
            // run row (even a cancelled one) is history worth keeping.
            let mut statement = transaction.prepare(
                "SELECT id FROM sessions
                     WHERE workspace_id = ?1 AND status = 'idle'
                       AND active_run_id IS NULL AND preparing_run_id IS NULL
                       AND queued_prompts = 0
                       AND NOT EXISTS (
                           SELECT 1 FROM messages WHERE messages.session_id = sessions.id
                       )
                       AND NOT EXISTS (
                           SELECT 1 FROM runs WHERE runs.session_id = sessions.id
                       )
                     ORDER BY rowid",
            )?;
            let victims = statement
                .query_map([workspace_id.to_string()], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            drop(statement);
            let mut cursor = EventCursor {
                store_id,
                workspace_id,
                sequence: workspace_sequence(&transaction, workspace_id)?,
            };
            let mut deleted: u32 = 0;
            for victim in victims {
                let session_id: SessionId = parse_id(&victim)?;
                let event = delete_idle_session(
                    &transaction,
                    store_id,
                    workspace_id,
                    session_id,
                    command_id,
                    now,
                )?;
                cursor = event.cursor;
                deleted += 1;
            }
            (
                CommandReceipt {
                    command_id,
                    committed_through: cursor,
                    outcome: CommandOutcome::SessionsPruned {
                        workspace_id,
                        deleted,
                    },
                },
                false,
            )
        }
        SessionCommand::CompactSession { session_id } => {
            let workspace_id = session_workspace(&transaction, session_id)?;
            let (status, active_run, preparing_run, queued): (
                String,
                Option<String>,
                Option<String>,
                u16,
            ) = transaction
                .query_row(
                    "SELECT status, active_run_id, preparing_run_id, queued_prompts
                     FROM sessions WHERE id = ?1",
                    [session_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?
                .ok_or(SessionRuntimeError::SessionNotFound)?;
            // Compaction is valid only while the session is idle: a running
            // run keeps the context it started with, and a queued prompt
            // must not race the summarizer.
            if status != "idle" || active_run.is_some() || preparing_run.is_some() || queued > 0 {
                return Err(SessionRuntimeError::SessionActive);
            }
            let run_id = RunId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            // Internal runs persist no message rows; the ids are placeholders
            // satisfying the runs schema.
            let user_message_id =
                MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            let assistant_message_id =
                MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            transaction.execute(
                "INSERT INTO runs(
                        id, session_id, command_id, user_message_id, assistant_message_id,
                        status, kind, created_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, 'queued', 'compaction', ?6)",
                params![
                    run_id.to_string(),
                    session_id.to_string(),
                    command_id.to_string(),
                    user_message_id.to_string(),
                    assistant_message_id.to_string(),
                    now,
                ],
            )?;
            // The internal run flows through the ordinary queue accounting so
            // claiming it decrements like any prompt.
            transaction.execute(
                "UPDATE sessions
                     SET status = 'queued', queued_prompts = queued_prompts + 1,
                         updated_at_ms = ?2
                     WHERE id = ?1",
                params![session_id.to_string(), now],
            )?;
            let summary = load_session_summary(&transaction, session_id)?;
            let event = append_event(
                &transaction,
                EventContext::for_run_ids(
                    store_id,
                    workspace_id,
                    session_id,
                    run_id,
                    Some(command_id),
                    now,
                ),
                SessionEvent::SessionUpdated {
                    session: Box::new(summary),
                },
            )?;
            (
                CommandReceipt {
                    command_id,
                    committed_through: event.cursor,
                    outcome: CommandOutcome::CompactionQueued { session_id, run_id },
                },
                true,
            )
        }
        SessionCommand::RollbackCompaction { session_id } => {
            let workspace_id = session_workspace(&transaction, session_id)?;
            let (status, active_run, preparing_run, queued): (
                String,
                Option<String>,
                Option<String>,
                u16,
            ) = transaction
                .query_row(
                    "SELECT status, active_run_id, preparing_run_id, queued_prompts
                     FROM sessions WHERE id = ?1",
                    [session_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?
                .ok_or(SessionRuntimeError::SessionNotFound)?;
            // Same idle requirement as compaction: an active run keeps the
            // assembly it started with, and a queued prompt must not race the
            // marker change.
            if status != "idle" || active_run.is_some() || preparing_run.is_some() || queued > 0 {
                return Err(SessionRuntimeError::SessionActive);
            }
            let removed = transaction.execute(
                "DELETE FROM session_compactions
                     WHERE session_id = ?1 AND rowid = (
                         SELECT MAX(rowid) FROM session_compactions WHERE session_id = ?1
                     )",
                [session_id.to_string()],
            )?;
            if removed != 1 {
                return Err(SessionRuntimeError::NoCompactionToRollBack);
            }
            let remaining: u16 = transaction.query_row(
                "SELECT COUNT(*) FROM session_compactions WHERE session_id = ?1",
                [session_id.to_string()],
                |row| row.get(0),
            )?;
            // The assembly changed under the meter: the last measured turn
            // saw the discarded summary, so the session is unknown until the
            // next prompt turn measures the restored context.
            transaction.execute(
                "UPDATE sessions
                     SET context_tokens = NULL,
                         context_occupancy_json = NULL,
                         pending_context_overflow_basis_json = NULL,
                         updated_at_ms = ?2
                     WHERE id = ?1",
                params![session_id.to_string(), now],
            )?;
            let summary = load_session_summary(&transaction, session_id)?;
            let event = append_event(
                &transaction,
                EventContext::for_session(
                    store_id,
                    workspace_id,
                    session_id,
                    Some(command_id),
                    now,
                ),
                SessionEvent::SessionCompactionRolledBack {
                    session: Box::new(summary),
                    remaining,
                },
            )?;
            (
                CommandReceipt {
                    command_id,
                    committed_through: event.cursor,
                    outcome: CommandOutcome::CompactionRolledBack {
                        session_id,
                        remaining,
                    },
                },
                false,
            )
        }
    };
    let receipt_json = serde_json::to_string(&receipt)?;
    transaction.execute(
        "INSERT INTO commands(id, request_json, receipt_json) VALUES (?1, ?2, ?3)",
        params![command_id.to_string(), request_json, receipt_json],
    )?;
    transaction
        .prepare_cached(
            "UPDATE metadata SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)
             WHERE key = 'command_count'",
        )
        .and_then(|mut statement| statement.execute([]))?;
    transaction.commit()?;
    Ok(AppliedCommand {
        receipt,
        schedule,
        cascade_cancels,
        grant_promotion_pending,
        replayed: false,
    })
}

/// Appends replaceable liveness information for an active run. Activity is
/// Marks queued steering as applied (`complete`) and publishes the boundary
/// it entered at. The message is now model context for `turn_ordinal` and
/// every later request of the run.
pub(super) fn apply_steering_message(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    message_id: MessageId,
    turn_ordinal: u32,
    attachments: &[crate::input::ResolvedAttachment],
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let now = now_ms();
    let changed = transaction.execute(
        "UPDATE messages SET state = 'complete', turn_ordinal = ?3
             WHERE id = ?1 AND run_id = ?2 AND steering = 1 AND state = 'queued'",
        params![
            message_id.to_string(),
            identity.run_id.to_string(),
            turn_ordinal
        ],
    )?;
    if changed != 1 {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    // The bytes the model saw ride the same transaction as the state change,
    // exactly as a prompt's attachments ride `RunStarted`.
    if !attachments.is_empty() {
        store_message_attachments(
            &transaction,
            identity.session_id,
            &message_id.to_string(),
            attachments,
            now,
        )?;
    }
    let event = append_event(
        &transaction,
        EventContext::for_run(store_id, identity, now_ms()),
        SessionEvent::SteeringApplied {
            run_id: identity.run_id,
            message_id,
            turn_ordinal,
        },
    )?;
    transaction.commit()?;
    Ok(event)
}

/// Steering still queued when a run settles never reached the model: the
/// rows move to `cancelled` and each is published as superseded.
pub(super) fn supersede_pending_steering(
    transaction: &Connection,
    store_id: StoreId,
    identity: RunIdentity,
    now: u64,
    events: &mut Vec<SessionEventEnvelope>,
) -> Result<(), SessionRuntimeError> {
    let mut statement = transaction.prepare(
        "SELECT id FROM messages
             WHERE run_id = ?1 AND steering = 1 AND state = 'queued' ORDER BY ordinal",
    )?;
    let ids = statement
        .query_map([identity.run_id.to_string()], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for id in ids {
        let message_id = parse_id::<MessageId>(&id)?;
        transaction.execute(
            "UPDATE messages SET state = 'cancelled' WHERE id = ?1",
            [message_id.to_string()],
        )?;
        events.push(append_event(
            transaction,
            EventContext::for_run(store_id, identity, now),
            SessionEvent::SteeringSuperseded {
                run_id: identity.run_id,
                message_id,
            },
        )?);
    }
    Ok(())
}

pub(super) fn pending_steering_rows(
    connection: &Connection,
    run_id: RunId,
) -> Result<Vec<crate::runtime::SteeringMessage>, SessionRuntimeError> {
    let mut statement = connection.prepare_cached(
        "SELECT id, output, input_json FROM messages
             WHERE run_id = ?1 AND steering = 1 AND state = 'queued' ORDER BY ordinal",
    )?;
    let rows = statement
        .query_map([run_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|(id, text, input_json)| {
            // Rows written before `input_json` was recorded carry only their
            // rendered text; that text is the whole input.
            let input = match input_json {
                Some(json) => parse_input_parts(Some(&json))?,
                None => vec![qq_protocol::InputPart::Text { text }],
            };
            Ok(crate::runtime::SteeringMessage {
                message_id: parse_id(&id)?,
                input,
            })
        })
        .collect()
}

/// The model-selection rules shared by `CreateSession` and `SetSessionModel`:
/// a bounded `provider/model` route, a nonzero token budget, and a bounded
/// organization.
pub(super) fn validate_model_selection(model: &ModelSelection) -> Result<(), SessionRuntimeError> {
    if !model.model.as_ref().is_some_and(|value| {
        value.len() <= MAX_MODEL_SELECTION_BYTES
            && value
                .split_once('/')
                .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
    }) || model.max_output_tokens == Some(0)
        || model
            .organization
            .as_ref()
            .is_some_and(|value| value.len() > MAX_MODEL_SELECTION_BYTES)
    {
        return Err(SessionRuntimeError::InvalidModelSelection);
    }
    Ok(())
}

/// Deletes one idle session and every row it owns, then appends
/// `SessionDeleted`, all inside the caller's transaction.
///
/// Refused while the session has an active run. That guard also keeps the
/// runtime's in-memory maps clean without extra plumbing: cancellation
/// senders and pending approvals exist only for claimed (executing) runs and
/// are removed when the run finishes, so a deletable session can have none.
/// Owned history also stays until every owning ancestor run has settled: an
/// ancestor may still need the descendant's spend for its child receipt.
///
/// The session's rows in the `events` log are deliberately kept. Workspace
/// cursors promise a gapless `previous + 1` sequence to subscribers (and the
/// sequence counter lives on the workspace row, so deletion could never
/// regress it), which means deleting event rows would break every replay
/// that spans the deletion. Replaying the kept events is harmless:
/// `SessionDeleted` removes the child, and a following parent
/// `SessionUpdated` refreshes the live inclusive projection when needed.
pub(super) fn delete_idle_session(
    transaction: &Connection,
    store_id: StoreId,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    command_id: CommandId,
    now: u64,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let state = transaction
        .query_row(
            "SELECT status, active_run_id, preparing_run_id, queued_prompts,
                    EXISTS(
                        SELECT 1 FROM runs
                        WHERE session_id = sessions.id
                          AND status IN ('queued', 'running')
                    )
             FROM sessions WHERE id = ?1",
            [session_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, u16>(3)?,
                    row.get::<_, bool>(4)?,
                ))
            },
        )
        .optional()?
        .ok_or(SessionRuntimeError::SessionNotFound)?;
    let (status, active_run, preparing_run, queued_prompts, unfinished) = state;
    if status != "idle"
        || active_run.is_some()
        || preparing_run.is_some()
        || queued_prompts != 0
        || unfinished
    {
        return Err(SessionRuntimeError::SessionActive);
    }
    let owner_active: bool = transaction.query_row(
        "WITH RECURSIVE owners(run_id, depth) AS (
                 SELECT owner_run_id, 1 FROM sessions
                 WHERE id = ?1 AND owner_run_id IS NOT NULL
                 UNION ALL
                 SELECT ancestor.owner_run_id, owners.depth + 1
                 FROM owners JOIN runs owner ON owner.id = owners.run_id
                 JOIN sessions ancestor ON ancestor.id = owner.session_id
                 WHERE ancestor.owner_run_id IS NOT NULL AND owners.depth < ?2
             )
             SELECT EXISTS(
                 SELECT 1 FROM owners
                 LEFT JOIN runs owner ON owner.id = owners.run_id
                 LEFT JOIN sessions ancestor ON ancestor.id = owner.session_id
                 WHERE owner.id IS NULL OR ancestor.id IS NULL
                     OR owner.status NOT IN
                         ('completed', 'cancelled', 'failed', 'interrupted', 'budget_exhausted', 'paused')
                     OR (owners.depth = ?2 AND ancestor.owner_run_id IS NOT NULL)
             )",
        params![session_id.to_string(), MAX_CHILD_DEPTH],
        |row| row.get(0),
    )?;
    if owner_active {
        return Err(SessionRuntimeError::SessionActive);
    }
    let parent_id = session_parent(transaction, session_id)?;
    let session = session_id.to_string();
    // Children survive their parent as root sessions; their spawn ownership
    // ends with that parent because its run rows are deleted below.
    for statement in [
        "UPDATE sessions SET parent_id = NULL, owner_run_id = NULL WHERE parent_id = ?1",
        "DELETE FROM tool_spills WHERE session_id = ?1",
        "DELETE FROM message_attachments WHERE session_id = ?1",
        "DELETE FROM attachment_blobs WHERE session_id = ?1",
        "DELETE FROM tool_calls WHERE run_id IN (SELECT id FROM runs WHERE session_id = ?1)",
        "DELETE FROM model_turns WHERE run_id IN (SELECT id FROM runs WHERE session_id = ?1)",
        "DELETE FROM messages WHERE session_id = ?1",
        "DELETE FROM runs WHERE session_id = ?1",
        "DELETE FROM session_grants WHERE session_id = ?1",
        "DELETE FROM session_files WHERE session_id = ?1",
        "DELETE FROM session_compactions WHERE session_id = ?1",
        "DELETE FROM sessions WHERE id = ?1",
    ] {
        transaction.execute(statement, [&session])?;
    }
    let deleted = append_event(
        transaction,
        EventContext::for_session(store_id, workspace_id, session_id, Some(command_id), now),
        SessionEvent::SessionDeleted { session_id },
    )?;
    let Some(parent_id) = parent_id else {
        return Ok(deleted);
    };
    let session = load_session_summary(transaction, parent_id)?;
    append_event(
        transaction,
        EventContext::for_session(store_id, workspace_id, parent_id, Some(command_id), now),
        SessionEvent::SessionUpdated {
            session: Box::new(session),
        },
    )
}

pub(super) fn ensure_workspace(
    connection: &Connection,
    workspace_id: WorkspaceId,
) -> Result<(), SessionRuntimeError> {
    let found = connection
        .prepare_cached("SELECT 1 FROM workspaces WHERE id = ?1")
        .and_then(|mut statement| {
            statement
                .query_row([workspace_id.to_string()], |_| Ok(()))
                .optional()
        })?;
    found.ok_or(SessionRuntimeError::WorkspaceNotFound)
}

pub(super) fn session_workspace(
    connection: &Connection,
    session_id: SessionId,
) -> Result<WorkspaceId, SessionRuntimeError> {
    let workspace = connection
        .query_row(
            "SELECT workspace_id FROM sessions WHERE id = ?1",
            [session_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or(SessionRuntimeError::SessionNotFound)?;
    parse_id(&workspace)
}

pub(super) fn workspace_sequence(
    connection: &Connection,
    workspace_id: WorkspaceId,
) -> Result<u64, SessionRuntimeError> {
    connection
        .query_row(
            "SELECT next_sequence FROM workspaces WHERE id = ?1",
            [workspace_id.to_string()],
            |row| row.get(0),
        )
        .map_err(|_| SessionRuntimeError::CODEC)
}

pub(super) fn prompt_title(prompt: &str) -> String {
    let mut title = String::new();
    let mut characters = 0;
    let mut pending_space = false;
    let mut truncated = false;
    for character in prompt.chars() {
        if character.is_whitespace() {
            pending_space = !title.is_empty();
            continue;
        }
        if character.is_control()
            || matches!(
                character,
                '\u{061c}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            )
        {
            continue;
        }
        if pending_space {
            if characters + 1 >= 48 {
                truncated = true;
                break;
            }
            title.push(' ');
            characters += 1;
            pending_space = false;
        }
        if characters == 48 {
            truncated = true;
            break;
        }
        title.push(character);
        characters += 1;
    }
    if truncated {
        title.push_str("...");
    }
    if title.is_empty() {
        "New session".to_owned()
    } else {
        title
    }
}

/// The `(kind, value)` pair a session grant stores. The value is trimmed;
/// emptiness and the byte cap are [`session_grant_recordable`]'s concern.
fn session_grant_parts(grant: &ApprovalGrant) -> (&'static str, &str) {
    match grant {
        ApprovalGrant::Tool { name } => ("tool", name.trim()),
        ApprovalGrant::ShellPrefix { prefix } => ("shell_prefix", prefix.trim()),
        ApprovalGrant::Host { host } => ("host", host.trim()),
    }
}

/// Whether this grant can be inserted into `session_grants` for the session:
/// non-empty, within [`MAX_GRANT_BYTES`], and the session is under
/// [`MAX_SESSION_GRANTS`]. A grant that is already stored counts as
/// recordable, so re-approving it does not trip the cap.
fn session_grant_recordable(
    transaction: &rusqlite::Connection,
    session_id: SessionId,
    grant: &ApprovalGrant,
) -> Result<bool, SessionRuntimeError> {
    let (kind, value) = session_grant_parts(grant);
    if value.is_empty() || value.len() > MAX_GRANT_BYTES {
        return Ok(false);
    }
    let already_stored: bool = transaction.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM session_grants
             WHERE session_id = ?1 AND kind = ?2 AND value = ?3
         )",
        params![session_id.to_string(), kind, value],
        |row| row.get(0),
    )?;
    if already_stored {
        return Ok(true);
    }
    let grant_count: u32 = transaction.query_row(
        "SELECT COUNT(*) FROM session_grants WHERE session_id = ?1",
        [session_id.to_string()],
        |row| row.get(0),
    )?;
    Ok(grant_count < MAX_SESSION_GRANTS)
}
