use super::*;

pub(super) struct AppliedCommand {
    pub(super) side_launch: Option<side_questions::SideLaunch>,
    pub(super) receipt: CommandReceipt,
    pub(super) schedule: bool,
    pub(super) cascade_cancels: Vec<RunId>,
    pub(super) replayed: bool,
}

#[derive(Default)]
pub(super) struct CommandAdmission {
    pub(super) goal_work_present: bool,
    pub(super) pause_goal_only_if_active: bool,
    pub(super) goal_check_policy: Option<Result<(), GoalCheckPolicyError>>,
    pub(super) goal_policy: Option<Result<GoalPolicy, GoalPolicyError>>,
}

pub(super) struct CreatedChildRun {
    pub(super) session_id: SessionId,
    pub(super) run_id: RunId,
    pub(super) committed_through: EventCursor,
}

#[derive(Clone, Copy)]
pub(super) struct ChildRunParent {
    pub(super) workspace_id: WorkspaceId,
    pub(super) session_id: SessionId,
    pub(super) run_id: RunId,
    pub(super) tool_call_id: Option<ToolCallId>,
    pub(super) depth: u16,
    pub(super) root_run_id: RunId,
}

pub(super) struct ChildAdmission {
    pub(super) profile: AgentProfileId,
    pub(super) model: ModelSelection,
    pub(super) reasoning_effort: Option<qq_provider::ReasoningEffort>,
    pub(super) task: String,
    pub(super) limits: RunLimits,
    pub(super) approval_mode: ApprovalMode,
    pub(super) purpose: SessionPurpose,
    pub(super) detached: bool,
}

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
        detached,
    } = admission;
    if !matches!(approval_mode, ApprovalMode::ReadOnly | ApprovalMode::Auto) {
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
    let (parent_workspace, paid_owner) = transaction
        .query_row(
            "SELECT s.workspace_id,
                CASE WHEN r.goal_id IS NOT NULL OR r.goal_paid_owner_run_id IS NOT NULL THEN r.id END
             FROM sessions s JOIN runs r ON r.session_id = s.id
             WHERE s.id = ?1 AND r.id = ?2 AND r.status = 'running'
               AND r.cancel_requested = 0",
            params![parent_session_id.to_string(), parent_run_id.to_string()],
            |row| Ok((row.get::<_,String>(0)?,row.get::<_,Option<String>>(1)?)),
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
                reasoning_effort
             ) VALUES (?1, ?2, ?3, ?4, ?10, ?5, 'queued', 1, ?6, ?7, ?8, ?11, ?9, ?9, ?12, ?13, ?14, ?15, ?16,
                COALESCE(?17, (SELECT reasoning_effort FROM sessions WHERE id = ?3)))",
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
                status, created_at_ms, limits_json, origin_json, goal_paid_owner_run_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?7, ?8, ?9)",
        params![
            run_id.to_string(),
            session_id.to_string(),
            command_id.to_string(),
            user_message_id.to_string(),
            assistant_message_id.to_string(),
            now,
            limits_json,
            serde_json::to_string(&qq_protocol::RunOrigin::Delegated {
                owner_run_id: parent_run_id,
                purpose
            })?,
            paid_owner,
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

    if detached {
        deliveries::insert_child_delivery(
            &transaction,
            run_id,
            parent_session_id,
            parent_run_id,
            now,
        )?;
    }
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CommandOrigin {
    Client,
    RuntimeSettlement,
}

pub(super) fn execute_command(
    connection: &mut Connection,
    store_id: StoreId,
    command_id: CommandId,
    command: SessionCommand,
    canonical_workspace: Option<Result<String, SessionRuntimeError>>,
    admission: &CommandAdmission,
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
            schedule: matches!(
                &command,
                SessionCommand::SetGoal { .. }
                    | SessionCommand::GoalControl { .. }
                    | SessionCommand::CancelRun { .. }
                    | SessionCommand::AcknowledgeCheckInterruption { .. }
            ),
            replayed: true,
            side_launch: None,
            cascade_cancels: match &command {
                SessionCommand::CancelRun { run_id } => {
                    let mut signals = cancellation_signal_run_ids(connection, *run_id)?;
                    let captured = connection
                        .query_row(
                            "SELECT session_id,goal_id FROM runs WHERE id=?1",
                            [run_id.to_string()],
                            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
                        )
                        .optional()?;
                    if let Some((session, Some(goal))) = captured {
                        for run in goals::paid_obligations(
                            connection,
                            parse_id(&session)?,
                            Some(parse_id(&goal)?),
                        )? {
                            if run.cancelled && !signals.contains(&run.run) {
                                signals.push(run.run);
                            }
                        }
                    }
                    signals
                }
                SessionCommand::SetGoal { session_id, .. }
                | SessionCommand::GoalControl { session_id, .. } => {
                    goals::paid_obligations(connection, *session_id, None)?
                        .into_iter()
                        .filter(|run| run.cancelled)
                        .map(|run| run.run)
                        .collect()
                }
                _ => Vec::new(),
            },
        });
    }
    let command_count: u32 = connection
        .prepare_cached("SELECT value FROM metadata WHERE key = 'command_count'")
        .and_then(|mut statement| statement.query_row([], |row| row.get::<_, String>(0)))?
        .parse()
        .map_err(|_| SessionRuntimeError::CODEC)?;
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
    let mut cascade_cancels = Vec::new();
    let mut side_launch = None;
    let (receipt, schedule) = match command {
        SessionCommand::SetGoal {
            session_id,
            objective,
            check,
            check_approval,
            budget,
            expected,
            replace,
        } => {
            let previous = goals::load_live(&transaction, session_id)?;
            let mut goal = goals::set(
                &transaction,
                session_id,
                goals::Specification {
                    objective,
                    check,
                    budget: *budget,
                },
                expected,
                replace,
                now,
                match &admission.goal_policy {
                    Some(result) => result.clone()?,
                    None => GoalPolicy::default(),
                },
            )?;
            if let Some(check) = goal.check.as_ref()
                && load_tool_approval_policy(&transaction, session_id)?.mode == ApprovalMode::Auto
            {
                if !goals::auto_check_authority(&transaction, session_id)?
                    || admission.goal_check_policy.is_none()
                {
                    return Err(GoalCheckPolicyError::Unavailable.into());
                }
                goals::validate_check_authority(
                    &transaction,
                    session_id,
                    &check.command,
                    admission.goal_check_policy.as_ref(),
                )?;
                if goal.check_authorization == Some(qq_protocol::GoalCheckAuthorization::Pending) {
                    goal.check_authorization =
                        Some(qq_protocol::GoalCheckAuthorization::TaskScoped);
                    if matches!(
                        goal.status,
                        qq_protocol::GoalStatus::Paused {
                            reason: qq_protocol::GoalPauseReason::NeedsUser {
                                cause: qq_protocol::GoalNeedsUser::CheckApproval,
                            },
                        }
                    ) {
                        goal.status = qq_protocol::GoalStatus::Active;
                    }
                    goals::save(&transaction, &goal)?;
                }
            }
            if let Some(scope) = check_approval {
                let command = goal
                    .check
                    .as_ref()
                    .ok_or(SessionRuntimeError::GoalCheckMissing)?
                    .command
                    .clone();
                goals::approve_check(
                    &transaction,
                    session_id,
                    &mut goal,
                    scope,
                    &command,
                    admission.goal_check_policy.as_ref(),
                    now,
                )?;
                goals::save(&transaction, &goal)?;
            }
            if let Some(previous) = previous {
                cascade_cancels.extend(cancel_goal_work(
                    &transaction,
                    store_id,
                    session_id,
                    previous.goal_id,
                    Some(command_id),
                    now,
                    replace || goal.status != qq_protocol::GoalStatus::Active,
                )?);
                let payload: String = transaction.query_row(
                    "SELECT payload_json FROM session_goals WHERE goal_id=?1",
                    [goal.goal_id.to_string()],
                    |row| row.get(0),
                )?;
                goal = serde_json::from_str(&payload)?;
            }
            let workspace_id = session_workspace(&transaction, session_id)?;
            let event = append_event(
                &transaction,
                EventContext::for_session(
                    store_id,
                    workspace_id,
                    session_id,
                    Some(command_id),
                    now,
                ),
                SessionEvent::GoalUpdated {
                    goal: Box::new(goal.clone()),
                },
            )?;
            (
                CommandReceipt {
                    command_id,
                    committed_through: event.cursor,
                    outcome: CommandOutcome::GoalUpdated {
                        session_id,
                        goal: Box::new(goal),
                    },
                },
                true,
            )
        }
        SessionCommand::AcknowledgeCheckInterruption {
            workspace_id,
            run_id,
        } => {
            ensure_workspace(&transaction, workspace_id)?;
            let session: Option<String> = transaction.query_row(
                "SELECT r.session_id FROM runs r JOIN sessions s ON s.id=r.session_id WHERE s.workspace_id=?1 AND r.id=?2 AND r.check_acknowledgment_pending=1 AND r.status='interrupted'",
                params![workspace_id.to_string(),run_id.to_string()],|row|row.get(0),
            ).optional()?;
            let Some(session) = session else {
                return Err(SessionRuntimeError::CheckInterruptionMismatch);
            };
            let session_id = parse_id(&session)?;
            transaction.execute(
                "UPDATE runs SET check_acknowledgment_pending=0 WHERE id=?1",
                [run_id.to_string()],
            )?;
            if let Some(goal) = goals::load_live(&transaction, session_id)?
                && matches!(
                    goal.status,
                    qq_protocol::GoalStatus::Paused {
                        reason: qq_protocol::GoalPauseReason::NeedsUser {
                            cause: qq_protocol::GoalNeedsUser::CheckInterrupted
                        }
                    }
                )
            {
                let captured: (Option<String>, Option<u32>) = transaction.query_row(
                    "SELECT goal_id,goal_revision FROM runs WHERE id=?1",
                    [run_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                if captured.0.as_deref() == Some(goal.goal_id.to_string().as_str())
                    && captured.1 == Some(goal.revision)
                {
                    let (goal, _) = goals::control(
                        &transaction,
                        session_id,
                        qq_protocol::GoalIdentity {
                            goal_id: goal.goal_id,
                            revision: goal.revision,
                        },
                        qq_protocol::GoalControl::Pause,
                        now,
                        None,
                        None,
                    )?;
                    append_event(
                        &transaction,
                        EventContext::for_session(
                            store_id,
                            workspace_id,
                            session_id,
                            Some(command_id),
                            now,
                        ),
                        SessionEvent::GoalUpdated {
                            goal: Box::new(goal),
                        },
                    )?;
                }
            }
            let event = append_event(
                &transaction,
                EventContext::for_session(
                    store_id,
                    workspace_id,
                    session_id,
                    Some(command_id),
                    now,
                ),
                SessionEvent::CheckInterruptionChanged {
                    interruption: super::goal_check::interruption(&transaction, workspace_id)?,
                },
            )?;
            (
                CommandReceipt {
                    command_id,
                    committed_through: event.cursor,
                    outcome: CommandOutcome::CheckInterruptionAcknowledged {
                        workspace_id,
                        run_id,
                    },
                },
                true,
            )
        }
        SessionCommand::GoalControl {
            session_id,
            expected,
            action,
        } => {
            if matches!(action, qq_protocol::GoalControl::Resume)
                && goals::load_live(&transaction, session_id)?.is_some_and(|goal| {
                    goal.check_authorization
                        == Some(qq_protocol::GoalCheckAuthorization::TaskScoped)
                })
                && !goals::auto_check_authority(&transaction, session_id)?
            {
                return Err(GoalCheckPolicyError::Unavailable.into());
            }
            let stopped = if admission.pause_goal_only_if_active {
                goals::load_live(&transaction, session_id)?.filter(|goal| {
                    goal.goal_id == expected.goal_id
                        && goal.revision == expected.revision
                        && goal.status != qq_protocol::GoalStatus::Active
                })
            } else {
                None
            };
            let already_stopped = stopped.is_some();
            let (mut goal, clear) = match stopped {
                Some(goal) => (goal, false),
                None => goals::control(
                    &transaction,
                    session_id,
                    expected,
                    action,
                    now,
                    admission.goal_check_policy.as_ref(),
                    admission.goal_policy.as_ref(),
                )?,
            };
            if !already_stopped && (clear || goal.status != qq_protocol::GoalStatus::Active) {
                cascade_cancels.extend(cancel_goal_work(
                    &transaction,
                    store_id,
                    session_id,
                    goal.goal_id,
                    Some(command_id),
                    now,
                    true,
                )?);
                let payload: String = transaction.query_row(
                    "SELECT payload_json FROM session_goals WHERE goal_id=?1",
                    [goal.goal_id.to_string()],
                    |row| row.get(0),
                )?;
                goal = serde_json::from_str(&payload)?;
            }
            let workspace_id = session_workspace(&transaction, session_id)?;
            let (event_body, outcome) = if clear {
                (
                    SessionEvent::GoalCleared {
                        session_id,
                        goal_id: goal.goal_id,
                        revision: goal.revision,
                    },
                    CommandOutcome::GoalCleared {
                        session_id,
                        goal_id: goal.goal_id,
                        revision: goal.revision,
                    },
                )
            } else {
                (
                    SessionEvent::GoalUpdated {
                        goal: Box::new(goal.clone()),
                    },
                    CommandOutcome::GoalUpdated {
                        session_id,
                        goal: Box::new(goal),
                    },
                )
            };
            let event = append_event(
                &transaction,
                EventContext::for_session(
                    store_id,
                    workspace_id,
                    session_id,
                    Some(command_id),
                    now,
                ),
                event_body,
            )?;
            (
                CommandReceipt {
                    command_id,
                    committed_through: event.cursor,
                    outcome,
                },
                true,
            )
        }
        SessionCommand::CancelSideQuestion { side_question_id } => {
            let item = side_questions::find_side_snapshot(&transaction, side_question_id)?
                .ok_or(SessionRuntimeError::SideQuestionNotFound)?;
            if item.state == qq_protocol::SideQuestionState::Running {
                transaction.execute(
                    "UPDATE side_questions SET state = 'cancelled', finished_at_ms = ?2
                     WHERE id = ?1 AND state = 'running'",
                    params![side_question_id.to_string(), now],
                )?;
                let event =
                    side_questions::append_side_event(&transaction, store_id, side_question_id)?;
                (
                    CommandReceipt {
                        command_id,
                        committed_through: event.cursor,
                        outcome: CommandOutcome::SideQuestionCancelled { side_question_id },
                    },
                    false,
                )
            } else {
                let workspace_id = session_workspace(&transaction, item.session_id)?;
                let sequence = workspace_sequence(&transaction, workspace_id)?;
                (
                    CommandReceipt {
                        command_id,
                        committed_through: EventCursor {
                            store_id,
                            workspace_id,
                            sequence,
                        },
                        outcome: CommandOutcome::SideQuestionAlreadyFinished {
                            side_question_id,
                            state: item.state,
                        },
                    },
                    false,
                )
            }
        }
        SessionCommand::SubmitSideQuestion {
            session_id,
            question,
            new_thread,
        } => {
            let id = RunId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            let (request, messages) = side_questions::admit_side_question(
                &transaction,
                store_id,
                session_id,
                id,
                question.clone(),
                new_thread,
            )?;
            let snapshot = side_questions::load_side_snapshot(&transaction, id)?;
            let workspace = session_workspace(&transaction, session_id)?;
            let sequence = transaction.query_row(
                "SELECT next_sequence FROM workspaces WHERE id = ?1",
                [workspace.to_string()],
                |row| row.get(0),
            )?;
            side_launch = Some(side_questions::SideLaunch {
                session: session_id,
                id,
                question,
                request,
                messages,
            });
            (
                CommandReceipt {
                    command_id,
                    committed_through: EventCursor {
                        store_id,
                        workspace_id: workspace,
                        sequence,
                    },
                    outcome: CommandOutcome::SideQuestionSubmitted {
                        side_question_id: id,
                        thread_id: snapshot.thread_id,
                    },
                },
                false,
            )
        }
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
                        root_run_id, model_is_fallback, reasoning_effort
                     ) VALUES (?1, ?2, ?3, 'New session', 'idle', ?4, ?5, ?6, ?7, ?8, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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
            let prompt = crate::input::render_text(&input).trim().to_owned();
            runtime::validate_slash_prompt(&prompt)
                .map_err(SessionRuntimeError::InvalidSlashCommand)?;
            let prompt = prompt
                .strip_prefix("//")
                .map_or(prompt.clone(), |literal| format!("/{literal}"));
            if admission.goal_work_present {
                let workspace = session_workspace(&transaction, session_id)?;
                let mut unclaimed = transaction.prepare(
                    "SELECT r.id FROM runs r JOIN sessions s ON s.id=r.session_id
                 WHERE r.session_id=?1 AND r.status='queued' AND r.outcome_json IS NULL
                   AND s.preparing_run_id IS NOT r.id AND s.active_run_id IS NOT r.id
                   AND json_extract(r.origin_json,'$.type') IN ('goal','goal_check')
                 LIMIT ?2",
                )?;
                let runs = unclaimed
                    .query_map(
                        params![session_id.to_string(), MAX_PENDING_PROMPTS],
                        |row| row.get::<_, String>(0),
                    )?
                    .collect::<Result<Vec<_>, _>>()?;
                drop(unclaimed);
                for run in runs {
                    finish_queued_run(
                        &transaction,
                        store_id,
                        workspace,
                        session_id,
                        parse_id(&run)?,
                        now,
                    )?;
                }
            }
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
            let workspace_id = parse_id(&workspace_id)?;
            let run_id = RunId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            let message_id = MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            let assistant_message_id =
                MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            let ordinal: u64 = transaction.query_row(
                "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM messages WHERE session_id = ?1",
                [session_id.to_string()],
                |row| row.get(0),
            )?;
            let limits_json = if limits.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&limits)?)
            };
            transaction.execute(
                "INSERT INTO runs(
                        id, session_id, command_id, user_message_id, assistant_message_id,
                        status, created_at_ms, limits_json, input_json, correlation_json,
                        output_contract_json, origin_json
                     ) VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?7, ?8, ?9, ?10, ?11)",
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
                    serde_json::to_string(&qq_protocol::RunOrigin::UserPrompt {})?,
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
            let (session_id, status, stored_outcome, captured_goal, preparing) = transaction
                .query_row(
                    "SELECT session_id, status, outcome_json, goal_id,
                        EXISTS(SELECT 1 FROM sessions WHERE preparing_run_id=runs.id)
                     FROM runs WHERE id = ?1",
                    [run_id.to_string()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, bool>(4)?,
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
            } else if let Some(goal) = if origin == CommandOrigin::Client {
                goals::load_live(&transaction, session_id)?.filter(|goal| {
                    captured_goal.as_deref() == Some(goal.goal_id.to_string().as_str())
                        && goal.status == qq_protocol::GoalStatus::Active
                })
            } else {
                None
            } {
                let (goal, _) = goals::control(
                    &transaction,
                    session_id,
                    qq_protocol::GoalIdentity {
                        goal_id: goal.goal_id,
                        revision: goal.revision,
                    },
                    qq_protocol::GoalControl::Pause,
                    now,
                    None,
                    None,
                )?;
                cascade_cancels.extend(cancel_goal_work(
                    &transaction,
                    store_id,
                    session_id,
                    goal.goal_id,
                    Some(command_id),
                    now,
                    true,
                )?);
                let payload: String = transaction.query_row(
                    "SELECT payload_json FROM session_goals WHERE goal_id=?1",
                    [goal.goal_id.to_string()],
                    |row| row.get(0),
                )?;
                let event = append_event(
                    &transaction,
                    EventContext::for_session(
                        store_id,
                        workspace_id,
                        session_id,
                        Some(command_id),
                        now,
                    ),
                    SessionEvent::GoalUpdated {
                        goal: Box::new(serde_json::from_str(&payload)?),
                    },
                )?;
                (
                    CommandReceipt {
                        command_id,
                        committed_through: event.cursor,
                        outcome: CommandOutcome::CancellationRequested { run_id },
                    },
                    true,
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
                let mut cursor = if status == "queued" && !preparing {
                    let finished = finish_queued_run(
                        &transaction,
                        store_id,
                        workspace_id,
                        session_id,
                        run_id,
                        now,
                    )?;
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
                        None => {
                            finished
                                .last()
                                .ok_or(SessionRuntimeError::CONSTRAINT)?
                                .cursor
                        }
                    }
                } else {
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
                let own_mode = load_approval_mode(&transaction, session_id)?;
                let ancestors = load_ancestor_approval_modes(&transaction, session_id)?;
                let approving = matches!(decision, ApprovalDecision::ApproveOnce);
                let mut refusal = None;
                if approving && own_mode == ApprovalMode::ReadOnly {
                    refusal = Some(approval::AUTHORITY_CHANGED_RESULT.to_owned());
                } else if approving && !ancestors.is_empty() {
                    let (_, class) = load_pending_approval_class(
                        &transaction,
                        workspace_id,
                        session_id,
                        run_id,
                        tool_call_id,
                    )?;
                    for mode in &ancestors {
                        refusal = match approval::evaluate(*mode, &class) {
                            approval::PolicyDecision::Execute
                            | approval::PolicyDecision::AskUser { .. } => continue,
                            approval::PolicyDecision::RequireApproval
                                if *mode == ApprovalMode::Ask =>
                            {
                                continue;
                            }
                            approval::PolicyDecision::RequireApproval => {
                                Some(approval::PARENT_DENIED_RESULT.to_owned())
                            }
                            approval::PolicyDecision::Deny { reason } => {
                                Some(approval::deny_result(&reason))
                            }
                        };
                        break;
                    }
                }
                let revoked = refusal.is_some();
                let denial_message = refusal.as_deref().unwrap_or(approval::USER_DENIED_RESULT);
                let decision = if revoked {
                    ApprovalDecision::Deny
                } else {
                    decision
                };
                let resolution = match &decision {
                    ApprovalDecision::ApproveOnce => ApprovalResolution::ApprovedOnce,
                    ApprovalDecision::Deny => ApprovalResolution::Denied,
                    ApprovalDecision::Answer { .. } => ApprovalResolution::Answered,
                };
                match &decision {
                    ApprovalDecision::ApproveOnce => {
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
                            denial_message,
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
                                denial_message,
                                approval_resolution_str(resolution),
                                now,
                            ],
                        )?;
                    }
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
                       AND NOT EXISTS (
                           SELECT 1 FROM side_questions q
                           WHERE q.session_id = sessions.id AND q.state = 'running'
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
            if status != "idle" || active_run.is_some() || preparing_run.is_some() || queued > 0 {
                return Err(SessionRuntimeError::SessionActive);
            }
            let run_id = RunId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            let user_message_id =
                MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            let assistant_message_id =
                MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            transaction.execute(
                "INSERT INTO runs(
                        id, session_id, command_id, user_message_id, assistant_message_id,
                        status, kind, created_at_ms, origin_json
                     ) VALUES (?1, ?2, ?3, ?4, ?5, 'queued', 'compaction', ?6, ?7)",
                params![
                    run_id.to_string(),
                    session_id.to_string(),
                    command_id.to_string(),
                    user_message_id.to_string(),
                    assistant_message_id.to_string(),
                    now,
                    serde_json::to_string(&qq_protocol::RunOrigin::ManualCompaction {})?,
                ],
            )?;
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
        replayed: false,
        side_launch,
    })
}

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
                          AND (status IN ('queued', 'running') OR check_acknowledgment_pending=1)
                    ) OR EXISTS(
                        SELECT 1 FROM side_questions
                        WHERE session_id = sessions.id AND state = 'running'
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
    let paid_descendants_active: bool = transaction.query_row(
        "WITH RECURSIVE paid(run_id,depth) AS (
            SELECT id,0 FROM runs WHERE session_id=?1
            UNION ALL
            SELECT child.id,paid.depth+1 FROM paid JOIN runs child
              ON child.goal_paid_owner_run_id=paid.run_id WHERE paid.depth<?2
         ) SELECT EXISTS(
            SELECT 1 FROM paid JOIN runs r ON r.id=paid.run_id
            WHERE r.status IN ('queued','running') OR EXISTS(
                SELECT 1 FROM runs helper WHERE helper.kind='compaction'
                  AND helper.auto_compaction=1 AND helper.auto_compaction_for_run_id=r.id
                  AND helper.session_id=r.session_id AND helper.status IN ('queued','running')
            )
         )",
        params![session_id.to_string(), MAX_CHILD_DEPTH],
        |row| row.get(0),
    )?;
    if paid_descendants_active {
        return Err(SessionRuntimeError::SessionActive);
    }
    let parent_id = session_parent(transaction, session_id)?;
    let session = session_id.to_string();
    for statement in [
        "UPDATE runs SET goal_paid_owner_run_id=NULL WHERE goal_paid_owner_run_id IN (SELECT id FROM runs WHERE session_id=?1)",
        "UPDATE sessions SET parent_id = NULL, owner_run_id = NULL WHERE parent_id = ?1",
        "DELETE FROM tool_spills WHERE session_id = ?1",
        "DELETE FROM message_attachments WHERE session_id = ?1",
        "DELETE FROM attachment_blobs WHERE session_id = ?1",
        "DELETE FROM tool_calls WHERE run_id IN (SELECT id FROM runs WHERE session_id = ?1)",
        "DELETE FROM model_turns WHERE run_id IN (SELECT id FROM runs WHERE session_id = ?1)",
        "DELETE FROM child_deliveries WHERE parent_session_id = ?1",
        "DELETE FROM child_reports WHERE parent_session_id = ?1",
        "DELETE FROM messages WHERE session_id = ?1",
        "DELETE FROM runs WHERE session_id = ?1",
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

pub(super) fn cancel_goal_work(
    transaction: &Connection,
    store_id: StoreId,
    session: SessionId,
    goal: qq_protocol::GoalId,
    command: Option<CommandId>,
    now: u64,
    drain: bool,
) -> Result<Vec<RunId>, SessionRuntimeError> {
    let mut signals = Vec::new();
    for obligation in goals::paid_obligations(transaction, session, Some(goal))? {
        if !drain
            && (obligation.session != session
                || !obligation.queued
                || obligation.preparing
                || !obligation.autonomous)
        {
            continue;
        }
        let run = obligation.run;
        transaction.execute(
            "UPDATE runs SET cancel_requested=1 WHERE id=?1 AND outcome_json IS NULL",
            [run.to_string()],
        )?;
        append_event(
            transaction,
            EventContext::for_run_ids(
                store_id,
                obligation.workspace,
                obligation.session,
                run,
                command,
                now,
            ),
            SessionEvent::CancellationRequested {
                session: Box::new(load_session_summary(transaction, obligation.session)?),
                run_id: run,
            },
        )?;
        if obligation.queued && !obligation.preparing {
            finish_queued_run(
                transaction,
                store_id,
                obligation.workspace,
                obligation.session,
                run,
                now,
            )?;
        } else {
            signals.push(run);
        }
    }
    Ok(signals)
}
