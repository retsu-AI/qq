use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RunKind {
    Prompt,
    Compaction,
}

pub(super) fn validate_run_limits(limits: &RunLimits) -> Result<(), SessionRuntimeError> {
    let zero = limits.max_duration_ms == Some(0)
        || limits.max_model_turns == Some(0)
        || limits.max_tool_calls == Some(0)
        || limits.max_total_tokens == Some(0)
        || limits.max_cost_usd_nanos == Some(0)
        || limits.max_input_tokens == Some(0)
        || limits.max_output_tokens == Some(0)
        || limits.max_tool_output_bytes == Some(0)
        || limits.max_children == Some(0)
        || limits.max_concurrent_children == Some(0);
    if zero {
        return Err(SessionRuntimeError::InvalidRunLimits);
    }
    if limits
        .max_children
        .is_some_and(|limit| limit > MAX_SPAWNED_CHILDREN_PER_RUN)
        || limits
            .max_concurrent_children
            .is_some_and(|limit| limit > MAX_CONCURRENT_CHILDREN_PER_RUN)
    {
        return Err(SessionRuntimeError::InvalidRunLimits);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RunIdentity {
    pub(super) workspace_id: WorkspaceId,
    pub(super) session_id: SessionId,
    pub(super) run_id: RunId,
    pub(super) command_id: CommandId,
    pub(super) kind: RunKind,
    pub(super) child: bool,
}

#[derive(Clone, Copy)]
pub(super) enum HistoryAdmissionFailure {
    MissingToolProjection(RunId),
    ContextTooLarge,
}

#[derive(Clone)]
pub(super) struct ClaimedRun {
    pub(super) identity: RunIdentity,
    pub(super) origin: Option<qq_protocol::RunOrigin>,
    pub(super) workspace: String,
    pub(super) depth: u16,
    pub(super) root_run_id: RunId,
    pub(super) purpose: SessionPurpose,
    pub(super) user_initiated: bool,
    pub(super) literal_slash: bool,
    pub(super) session_model: ModelSelection,
    pub(super) model: ModelSelection,
    pub(super) messages: Vec<Message>,
    pub(super) message_effects: ResultEffects,
    pub(super) history_admission_failure: Option<HistoryAdmissionFailure>,
    pub(super) context_compaction_attempted: u32,
    pub(super) context_compaction_failed: bool,
    pub(super) context_compaction_remaining: bool,
    pub(super) compaction_cutoff_ordinal: Option<u64>,
    pub(super) context_compaction_oversized_unit_bytes: Option<u64>,
    pub(super) in_run_turn_cutoff: Option<(RunId, u32)>,
    pub(super) context_overflow_basis: Option<ContextOccupancyBasis>,
    pub(super) context_occupancy: Option<ContextOccupancy>,
    pub(super) limits: RunLimits,
    pub(super) approval_mode: ApprovalMode,
    pub(super) input: Vec<InputPart>,
    pub(super) resolved_input: Option<Arc<crate::input::ResolvedInput>>,
    pub(super) profile: AgentProfileId,
    pub(super) reasoning_effort: Option<qq_provider::ReasoningEffort>,
    pub(super) cancel_requested: bool,
    pub(super) file_state: Vec<(String, String)>,
    pub(super) pending_steering: Vec<crate::runtime::SteeringMessage>,
    pub(super) output: Option<Arc<crate::output::CompiledOutputSchema>>,
}

impl ClaimedRun {
    pub(super) fn panic_settlement_claim(&self) -> Self {
        Self {
            identity: self.identity,
            origin: self.origin,
            workspace: String::new(),
            user_initiated: self.user_initiated,
            literal_slash: self.literal_slash,
            session_model: self.session_model.clone(),
            model: self.model.clone(),
            messages: Vec::new(),
            message_effects: Vec::new(),
            history_admission_failure: None,
            context_compaction_attempted: self.context_compaction_attempted,
            context_compaction_failed: self.context_compaction_failed,
            context_compaction_remaining: self.context_compaction_remaining,
            compaction_cutoff_ordinal: None,
            context_compaction_oversized_unit_bytes: None,
            in_run_turn_cutoff: None,
            context_overflow_basis: None,
            context_occupancy: None,
            limits: RunLimits::default(),
            input: Vec::new(),
            resolved_input: None,
            profile: self.profile.clone(),
            reasoning_effort: self.reasoning_effort,
            cancel_requested: false,
            file_state: Vec::new(),
            pending_steering: Vec::new(),
            approval_mode: self.approval_mode,
            depth: self.depth,
            root_run_id: self.root_run_id,
            purpose: self.purpose,
            output: None,
        }
    }
}

#[derive(Clone)]
pub(super) struct PreparedRunAudit {
    pub(super) prompt_identity: Arc<RunPromptIdentity>,
    pub(super) resolved_model: Arc<ResolvedModel>,
    pub(super) plan_identity: RunPlanIdentity,
    pub(super) plan_descriptor_json: Arc<str>,
    pub(super) context_shape: ContextRequestShape,
    pub(super) weight: PreparedRequestWeight,
    pub(super) static_prefix: PreparedStaticPrefix,
}

pub(super) const CONTEXT_OCCUPANCY_BASIS_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ContextRequestShape {
    pub(super) digest: ContentHash,
    pub(super) provider_identity: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ContextOccupancyBasis {
    pub(super) version: u16,
    pub(super) shape: ContentHash,
    pub(super) static_prefix: PreparedStaticPrefix,
    pub(super) request_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ContextOccupancy {
    pub(super) context_tokens: u64,
    pub(super) basis: ContextOccupancyBasis,
}

pub(super) fn context_request_shape(model: &ResolvedModel) -> ContextRequestShape {
    let provider = model
        .request_shape
        .filter(|provider| model.version.get() == 2 && provider.version.get() == 1);
    let mut digest = Sha256::new();
    match provider {
        Some(provider) => {
            context_shape_update(&mut digest, b"qq-context-request-shape-v1");
            digest.update(provider.version.get().to_be_bytes());
            context_shape_update(&mut digest, provider.digest.as_bytes());
        }
        None => context_shape_update(&mut digest, b"qq-context-route-shape-v1"),
    }
    context_shape_update(&mut digest, model.route.as_bytes());
    context_shape_update(&mut digest, model.provider_model.as_bytes());
    context_shape_update_optional(&mut digest, model.organization.as_deref());
    context_shape_update_optional(&mut digest, model.credential_profile.as_deref());
    digest.update(model.max_output_tokens.to_be_bytes());
    match model.context_window {
        Some(window) => {
            digest.update([1]);
            digest.update(window.to_be_bytes());
        }
        None => digest.update([0]),
    }
    digest.update([capability_byte(model.output_token_control)]);
    digest.update([capability_byte(model.generation.reasoning_effort)]);
    digest.update([capability_byte(model.prompt_cache.control)]);
    digest.update([
        u8::from(model.prompt_cache.cache_read_usage),
        u8::from(model.prompt_cache.cache_write_usage),
    ]);
    ContextRequestShape {
        digest: ContentHash::from_bytes(digest.finalize().into()),
        provider_identity: provider.is_some(),
    }
}

pub(super) fn context_occupancy_basis(
    shape: ContentHash,
    static_prefix: PreparedStaticPrefix,
    request_bytes: u64,
) -> ContextOccupancyBasis {
    ContextOccupancyBasis {
        version: CONTEXT_OCCUPANCY_BASIS_VERSION,
        shape,
        static_prefix,
        request_bytes,
    }
}

pub(super) fn compatible_context_tokens(
    occupancy: ContextOccupancy,
    shape: ContextRequestShape,
    static_prefix: PreparedStaticPrefix,
    request_bytes: u64,
) -> Option<u64> {
    let basis = occupancy.basis;
    (shape.provider_identity && repeats_context_basis(basis, shape, static_prefix)).then(|| {
        context::adjust_measured_tokens(
            occupancy.context_tokens,
            basis.request_bytes,
            request_bytes,
        )
    })
}

pub(super) fn repeats_context_basis(
    basis: ContextOccupancyBasis,
    shape: ContextRequestShape,
    static_prefix: PreparedStaticPrefix,
) -> bool {
    basis.version == CONTEXT_OCCUPANCY_BASIS_VERSION
        && basis.shape == shape.digest
        && basis.static_prefix == static_prefix
}

pub(super) fn context_shape_update(digest: &mut Sha256, value: &[u8]) {
    digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(value);
}

pub(super) fn context_shape_update_optional(digest: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            digest.update([1]);
            context_shape_update(digest, value.as_bytes());
        }
        None => digest.update([0]),
    }
}

pub(super) const fn capability_byte(capability: CapabilitySupport) -> u8 {
    match capability {
        CapabilitySupport::Native => 1,
        CapabilitySupport::Unsupported => 2,
    }
}

#[cfg(test)]
pub(super) fn test_prepared_audit(claimed: &ClaimedRun) -> PreparedRunAudit {
    test_prepared_audit_with_identity(claimed, true)
}

#[cfg(test)]
pub(super) fn test_prepared_audit_with_identity(
    claimed: &ClaimedRun,
    provider_identity: bool,
) -> PreparedRunAudit {
    let route = claimed
        .model
        .model
        .clone()
        .unwrap_or_else(|| "test/test-model".to_owned());
    let max_output_tokens = claimed.model.max_output_tokens.unwrap_or(256);
    let resolved_model = Arc::new(ResolvedModel {
        version: qq_protocol::ResolvedModelVersion::new(2).unwrap(),
        request_shape: provider_identity.then(|| qq_protocol::ProviderRequestShapeIdentity {
            version: qq_protocol::ProviderRequestShapeVersion::new(1).unwrap(),
            digest: ContentHash::from_bytes([1; 32]),
        }),
        route: route.clone(),
        provider_model: route,
        organization: claimed.model.organization.clone(),
        credential_profile: None,
        max_output_tokens,
        context_window: None,
        pricing: None,
        output_token_control: qq_protocol::CapabilitySupport::Native,
        generation: qq_protocol::GenerationCapabilities {
            reasoning_effort: qq_protocol::CapabilitySupport::Unsupported,
        },
        prompt_cache: qq_protocol::PromptCacheCapabilities {
            control: qq_protocol::CapabilitySupport::Unsupported,
            cache_read_usage: false,
            cache_write_usage: false,
        },
    });
    PreparedRunAudit {
        prompt_identity: Arc::new(RunPromptIdentity {
            version: qq_protocol::PromptVersion::new(1).unwrap(),
            instruction_hash: qq_protocol::InstructionHash::from_bytes([0; 32]),
            system_prompt_hash: Some(qq_protocol::ContentHash::from_bytes([0; 32])),
            tool_schema_hash: Some(qq_protocol::ContentHash::from_bytes([0; 32])),
            selected_guidance: None,
            catalog_digest: None,
            exposure: None,
            context_sources: Vec::new(),
        }),
        context_shape: context_request_shape(resolved_model.as_ref()),
        resolved_model,
        plan_identity: RunPlanIdentity {
            profile: AgentProfileId::default(),
            descriptor_version: crate::plan::DESCRIPTOR_VERSION,
            digest: qq_protocol::AgentPlanDigest::from_hash(ContentHash::from_bytes([0; 32])),
            credential_epoch: qq_protocol::CredentialEpoch::NONE,
        },
        plan_descriptor_json: Arc::from(
            serde_json::json!({"version": crate::plan::DESCRIPTOR_VERSION, "tool_output_projection": "strict_reserved_v1"}).to_string(),
        ),
        weight: PreparedRequestWeight {
            max_output_tokens,
            system_bytes: 0,
            tool_schema_bytes: 0,
            reducible_message_bytes: crate::measure_messages(&claimed.messages),
            irreducible_message_bytes: 0,
            compatible_input_tokens: None,
        },
        static_prefix: PreparedStaticPrefix::new(
            ContentHash::from_bytes([0; 32]),
            Some(ContentHash::from_bytes([0; 32])),
        ),
    }
}

pub(super) fn reserve_next_run(
    connection: &mut Connection,
    store_id: StoreId,
    depth: u16,
) -> Result<Option<ClaimedRun>, SessionRuntimeError> {
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    let reserved = reserve_next_run_recoverable(connection, store_id, depth);
    let restored = connection
        .pragma_update(None, "synchronous", "FULL")
        .map_err(|_| SessionRuntimeError::CONSTRAINT);
    match (reserved, restored) {
        (_, Err(error)) => Err(error),
        (result, Ok(())) => result,
    }
}

pub(super) fn reserve_next_run_recoverable(
    connection: &mut Connection,
    _store_id: StoreId,
    depth: u16,
) -> Result<Option<ClaimedRun>, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let restoring = commands::paths_overlap_sql("w.path", "rp.path");
    let row = transaction
        .query_row(
            &format!("SELECT r.id, r.session_id, r.command_id, r.user_message_id, r.kind,
                    s.workspace_id, w.path, s.model, s.max_output_tokens, s.organization,
                    r.context_compaction_attempted,
                    (SELECT c.request_json FROM commands c WHERE c.id = r.command_id),
                    s.pending_context_overflow_basis_json,
                    s.context_tokens, s.context_occupancy_json, r.limits_json,
                    r.input_json, s.profile, s.approval_mode, s.depth, s.root_run_id,
                    s.purpose, r.output_contract_json, s.reasoning_effort,
                    EXISTS(SELECT 1 FROM runs step
                           WHERE step.auto_compaction_for_run_id = r.id
                             AND step.status IN ('failed', 'paused')),
                    COALESCE((SELECT cutoff_ordinal FROM session_compactions
                              WHERE session_id = s.id ORDER BY rowid DESC LIMIT 1), 0)
                      < COALESCE((SELECT MAX(ordinal) FROM messages
                                  WHERE session_id = s.id
                                    AND role = 'user' AND steering = 0
                                    AND state IN ('complete', 'cancelled', 'failed', 'interrupted')), 0),
                    s.model_is_fallback, r.origin_json
             FROM runs r
             JOIN sessions s ON s.id = r.session_id
             JOIN workspaces w ON w.id = s.workspace_id
             WHERE r.status = 'queued' AND s.active_run_id IS NULL
               AND s.preparing_run_id IS NULL
               AND s.depth = ?1
               AND NOT EXISTS(SELECT 1 FROM temp.restoring_paths rp WHERE {restoring})
             ORDER BY COALESCE((
                         SELECT MAX(previous.started_at_ms)
                         FROM runs previous
                         WHERE previous.session_id = r.session_id
                     ), 0),
                      r.created_at_ms, r.rowid
             LIMIT 1"),
            params![depth],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<u32>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, u32>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<String>>(12)?,
                    row.get::<_, Option<u64>>(13)?,
                    row.get::<_, Option<String>>(14)?,
                    row.get::<_, Option<String>>(15)?,
                    row.get::<_, Option<String>>(16)?,
                    row.get::<_, Option<String>>(17)?,
                    row.get::<_, String>(18)?,
                    row.get::<_, u16>(19)?,
                    row.get::<_, Option<String>>(20)?,
                    row.get::<_, String>(21)?,
                    row.get::<_, Option<String>>(22)?,
                    row.get::<_, Option<String>>(23)?,
                    row.get::<_, bool>(24)?,
                    row.get::<_, bool>(25)?,
                    row.get::<_, bool>(26)?,
                    row.get::<_, Option<String>>(27)?,
                ))
            },
        )
        .optional()?;
    let Some((
        run,
        session,
        command,
        user_message,
        kind,
        workspace,
        workspace_path,
        model,
        max_tokens,
        organization,
        context_compaction_attempted,
        command_request,
        pending_context_overflow_basis_json,
        context_tokens,
        context_occupancy_json,
        limits_json,
        input_json,
        profile,
        approval_mode,
        depth,
        root_run,
        purpose,
        output_contract_json,
        reasoning_effort,
        context_compaction_failed,
        context_compaction_remaining,
        model_is_fallback,
        origin_json,
    )) = row
    else {
        return Ok(None);
    };
    let output = match output_contract_json {
        None => None,
        Some(encoded) => {
            let contract = serde_json::from_str::<qq_protocol::OutputContract>(&encoded)?;
            Some(
                crate::output::CompiledOutputSchema::compile(&contract)
                    .map_err(|_| SessionRuntimeError::CONSTRAINT)?,
            )
        }
    };
    let purpose = match purpose.as_str() {
        "task" => SessionPurpose::Task,
        "audit" => SessionPurpose::Audit,
        _ => return Err(SessionRuntimeError::CONSTRAINT),
    };
    let limits = parse_run_limits(limits_json.as_deref())?;
    let input = parse_input_parts(input_json.as_deref())?;
    let profile = parse_profile(profile.as_deref())?;
    let reasoning_effort = parse_reasoning_effort(reasoning_effort.as_deref())?;
    let approval_mode = parse_approval_mode(&approval_mode)?;
    let run_id: RunId = parse_id(&run)?;
    let root_run_id = match root_run {
        Some(root) => parse_id(&root)?,
        None => run_id,
    };
    let session_id: SessionId = parse_id(&session)?;
    let command_id: CommandId = parse_id(&command)?;
    let user_message_id = parse_id::<MessageId>(&user_message)?;
    let (user_initiated, literal_slash) = match command_request {
        Some(request) => {
            let request = serde_json::from_str::<SessionCommand>(&request)?;
            let literal_slash = matches!(
                &request,
                SessionCommand::SubmitPrompt { input, .. }
                    if crate::input::render_text(input).trim().starts_with("//")
            );
            (true, literal_slash)
        }
        None => (false, false),
    };
    let kind = parse_run_kind(&kind)?;
    let workspace_id: WorkspaceId = parse_id(&workspace)?;
    let model = ModelSelection {
        model_is_fallback,
        model,
        max_output_tokens: max_tokens,
        organization,
    };
    let reserved = transaction.execute(
        "UPDATE sessions SET preparing_run_id = ?2
             WHERE id = ?1 AND active_run_id IS NULL AND preparing_run_id IS NULL",
        params![session, run],
    )?;
    if reserved != 1 {
        return Ok(None);
    }
    let mut history_admission_failure = None;
    let (messages, message_effects) = match kind {
        RunKind::Prompt => {
            let user_ordinal: u64 = transaction.query_row(
                "SELECT ordinal FROM messages WHERE id = ?1",
                [user_message_id.to_string()],
                |row| row.get(0),
            )?;
            let prompt: String = transaction.query_row(
                "SELECT output FROM messages WHERE id = ?1 AND state = 'queued'",
                [user_message_id.to_string()],
                |row| row.get(0),
            )?;
            let (mut context, effects) = match load_model_context_with_effects(
                &transaction,
                session_id,
                user_ordinal.saturating_sub(1),
            ) {
                Ok(context) => context,
                Err(SessionRuntimeError::ToolProjectionUnavailable { run }) => {
                    history_admission_failure = Some(
                        HistoryAdmissionFailure::MissingToolProjection(parse_id(&run)?),
                    );
                    (Vec::new(), Vec::new())
                }
                Err(SessionRuntimeError::ContextTooLarge) => {
                    history_admission_failure = Some(HistoryAdmissionFailure::ContextTooLarge);
                    (Vec::new(), Vec::new())
                }
                Err(error) => return Err(error),
            };
            context.push(Message::user(prompt));
            (context, effects)
        }
        RunKind::Compaction => {
            let (mut context, effects) =
                match load_model_context_with_effects(&transaction, session_id, u64::MAX) {
                    Ok(context) => context,
                    Err(SessionRuntimeError::ToolProjectionUnavailable { run }) => {
                        history_admission_failure = Some(
                            HistoryAdmissionFailure::MissingToolProjection(parse_id(&run)?),
                        );
                        (Vec::new(), Vec::new())
                    }
                    Err(SessionRuntimeError::ContextTooLarge) => {
                        history_admission_failure = Some(HistoryAdmissionFailure::ContextTooLarge);
                        (Vec::new(), Vec::new())
                    }
                    Err(error) => return Err(error),
                };
            context.push(Message::user(COMPACTION_INSTRUCTION.to_owned()));
            (context, effects)
        }
    };
    let decode_basis = |encoded: &str| {
        serde_json::from_str::<ContextOccupancyBasis>(encoded)
            .ok()
            .filter(|basis| basis.version == CONTEXT_OCCUPANCY_BASIS_VERSION)
    };
    let context_overflow_basis = if kind == RunKind::Prompt {
        pending_context_overflow_basis_json
            .as_deref()
            .and_then(decode_basis)
    } else {
        None
    };
    let context_occupancy = if kind == RunKind::Prompt {
        context_tokens
            .zip(context_occupancy_json.as_deref())
            .and_then(|(context_tokens, encoded)| {
                decode_basis(encoded).map(|basis| ContextOccupancy {
                    context_tokens,
                    basis,
                })
            })
    } else {
        None
    };
    let clear_context_occupancy =
        kind == RunKind::Prompt && context_occupancy_json.is_some() && context_occupancy.is_none();
    let clear_context_overflow = kind == RunKind::Prompt
        && pending_context_overflow_basis_json.is_some()
        && context_overflow_basis.is_none();
    if clear_context_occupancy || clear_context_overflow {
        transaction
            .execute(
                "UPDATE sessions
                 SET context_occupancy_json = CASE WHEN ?2 THEN NULL ELSE context_occupancy_json END,
                     pending_context_overflow_basis_json = CASE
                         WHEN ?3 THEN NULL ELSE pending_context_overflow_basis_json
                     END
                 WHERE id = ?1",
                params![
                    session_id.to_string(),
                    clear_context_occupancy,
                    clear_context_overflow,
                ],
            )
            ?;
    }
    let cancel_requested = run_cancel_requested(&transaction, run_id)?;
    let file_state = session_file_state_rows(&transaction, session_id)?;
    let pending_steering = pending_steering_rows(&transaction, run_id)?;
    let origin = origin_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?;
    transaction.commit()?;
    Ok(Some(ClaimedRun {
        origin,
        identity: RunIdentity {
            workspace_id,
            session_id,
            run_id,
            command_id,
            kind,
            child: depth > 0,
        },
        workspace: workspace_path,
        user_initiated,
        literal_slash,
        session_model: model.clone(),
        model,
        messages,
        message_effects,
        history_admission_failure,
        context_compaction_attempted,
        context_compaction_failed,
        context_compaction_remaining,
        compaction_cutoff_ordinal: None,
        context_compaction_oversized_unit_bytes: None,
        in_run_turn_cutoff: None,
        context_overflow_basis,
        context_occupancy,
        limits,
        input,
        resolved_input: None,
        profile,
        reasoning_effort,
        approval_mode,
        depth,
        root_run_id,
        cancel_requested,
        file_state,
        pending_steering,
        output,
        purpose,
    }))
}

pub(super) fn prepared_context_bytes(
    weight: PreparedRequestWeight,
) -> Result<i64, SessionRuntimeError> {
    i64::try_from(
        weight
            .system_bytes
            .saturating_add(weight.tool_schema_bytes)
            .saturating_add(weight.reducible_message_bytes)
            .saturating_add(weight.irreducible_message_bytes),
    )
    .map_err(|_| SessionRuntimeError::OutputTooLarge)
}

pub(super) type RunStart = Option<SessionEventEnvelope>;

pub(super) struct RunStartMetadata {
    pub(super) origin: Option<qq_protocol::RunOrigin>,
}

pub(super) fn start_reserved_run(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    audit: &PreparedRunAudit,
    metadata: RunStartMetadata,
    resolved_input: Option<&crate::input::ResolvedInput>,
) -> Result<RunStart, SessionRuntimeError> {
    let RunStartMetadata { origin } = metadata;
    let prompt_identity = serde_json::to_string(audit.prompt_identity.as_ref())?;
    let resolved_model = serde_json::to_string(audit.resolved_model.as_ref())?;
    let plan_identity = serde_json::to_string(&audit.plan_identity)?;
    let context_base_bytes = prepared_context_bytes(audit.weight)?;
    let now = now_ms();
    let transaction = store::begin_unit(connection)?;
    let compacting = identity.kind == RunKind::Compaction;
    let run_started = transaction.execute(
        "UPDATE runs
             SET status = 'running', started_at_ms = ?3,
                 prompt_identity_json = ?4, resolved_model_json = ?5,
                 context_base_bytes = ?6, context_increment_bytes = 0,
                 plan_identity_json = ?7, plan_descriptor_json = ?8,
                 activity = CASE WHEN ?9 THEN 'compacting' ELSE activity END
             WHERE id = ?1 AND session_id = ?2 AND status = 'queued'
               AND outcome_json IS NULL AND cancel_requested = 0",
        params![
            identity.run_id.to_string(),
            identity.session_id.to_string(),
            now,
            prompt_identity,
            resolved_model,
            context_base_bytes,
            plan_identity,
            audit.plan_descriptor_json.as_ref(),
            compacting,
        ],
    )?;
    if run_started != 1 {
        return Ok(None);
    }
    let session_started = transaction.execute(
        "UPDATE sessions
             SET active_run_id = ?2, preparing_run_id = NULL, status = 'running',
                 queued_prompts = queued_prompts - 1, updated_at_ms = ?3
             WHERE id = ?1 AND active_run_id IS NULL AND preparing_run_id = ?2
               AND queued_prompts > 0",
        params![
            identity.session_id.to_string(),
            identity.run_id.to_string(),
            now,
        ],
    )?;
    if session_started != 1 {
        return Ok(None);
    }
    transaction.execute(
        "UPDATE messages SET state = 'complete'
             WHERE run_id = ?1 AND role = 'user' AND state = 'queued'",
        [identity.run_id.to_string()],
    )?;
    if let Some(resolved) = resolved_input.filter(|resolved| !resolved.attachments.is_empty()) {
        let user_message_id: String = transaction.query_row(
            "SELECT user_message_id FROM runs WHERE id = ?1",
            [identity.run_id.to_string()],
            |row| row.get(0),
        )?;
        store_message_attachments(
            &transaction,
            identity.session_id,
            &user_message_id,
            &resolved.attachments,
            now,
        )?;
    }
    let summary = load_session_summary(&transaction, identity.session_id)?;
    let started = append_event(
        &transaction,
        EventContext::for_run(store_id, identity, now).uncaused(),
        SessionEvent::RunStarted {
            origin,
            session: Box::new(summary),
            run_id: identity.run_id,
            plan: Some(Box::new(audit.plan_identity.clone())),
        },
    )?;
    if compacting {
        append_event(
            &transaction,
            EventContext::for_run(store_id, identity, now).uncaused(),
            SessionEvent::RunActivityChanged {
                run_id: identity.run_id,
                activity: RunActivity::Compacting,
            },
        )?;
    }
    transaction.commit()?;
    Ok(Some(started))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CompactionProgress {
    pub(super) steps: u32,
    pub(super) failed: bool,
    pub(super) remaining: bool,
}

pub(super) fn reload_reserved_messages(
    connection: &mut Connection,
    identity: RunIdentity,
) -> Result<Option<(Vec<Message>, ResultEffects, CompactionProgress)>, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let row = transaction
        .query_row(
            "SELECT r.status, r.cancel_requested, r.context_compaction_attempted,
                    r.user_message_id, s.preparing_run_id, s.active_run_id,
                    EXISTS(SELECT 1 FROM runs step
                           WHERE step.auto_compaction_for_run_id = r.id
                             AND step.status IN ('failed', 'paused')),
                    COALESCE((SELECT cutoff_ordinal FROM session_compactions
                              WHERE session_id = s.id ORDER BY rowid DESC LIMIT 1), 0)
                      < COALESCE((SELECT MAX(ordinal) FROM messages
                                  WHERE session_id = s.id
                                    AND role = 'user' AND steering = 0
                                    AND state IN ('complete', 'cancelled', 'failed', 'interrupted')), 0)
             FROM runs r JOIN sessions s ON s.id = r.session_id
             WHERE r.id = ?1 AND r.session_id = ?2",
            params![identity.run_id.to_string(), identity.session_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, u32>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, bool>(6)?,
                    row.get::<_, bool>(7)?,
                ))
            },
        )
        .optional()?;
    let Some((status, cancelled, steps, user_message_id, preparing, active, failed, remaining)) =
        row
    else {
        return Ok(None);
    };
    let progress = CompactionProgress {
        steps,
        failed,
        remaining,
    };
    if status != "queued"
        || cancelled
        || preparing.as_deref() != Some(identity.run_id.to_string().as_str())
        || active.is_some()
    {
        return Ok(None);
    }
    let user_ordinal: u64 = transaction.query_row(
        "SELECT ordinal FROM messages WHERE id = ?1",
        [user_message_id.as_str()],
        |row| row.get(0),
    )?;
    let prompt: String = transaction.query_row(
        "SELECT output FROM messages WHERE id = ?1 AND state = 'queued'",
        [user_message_id.as_str()],
        |row| row.get(0),
    )?;
    let (mut messages, effects) = load_model_context_with_effects(
        &transaction,
        identity.session_id,
        user_ordinal.saturating_sub(1),
    )?;
    messages.push(Message::user(prompt));
    transaction.commit()?;
    Ok(Some((messages, effects, progress)))
}

pub(super) fn reserve_context_capacity(
    transaction: &Connection,
    run_id: RunId,
    additional: usize,
) -> Result<(), SessionRuntimeError> {
    if additional == 0 {
        return Ok(());
    }
    let additional = i64::try_from(additional).map_err(|_| SessionRuntimeError::OutputTooLarge)?;
    let maximum = i64::try_from(MAX_CONTEXT_BYTES).expect("context limit fits SQLite integer");
    let updated = transaction
        .prepare_cached(
            "UPDATE runs
             SET context_increment_bytes = context_increment_bytes + ?2
             WHERE id = ?1 AND status = 'running' AND context_base_bytes IS NOT NULL
               AND context_base_bytes + context_increment_bytes + ?2 <= ?3",
        )
        .and_then(|mut statement| {
            statement.execute(params![run_id.to_string(), additional, maximum])
        })?;
    if updated == 1 {
        return Ok(());
    }
    let active: bool = transaction
        .query_row(
            "SELECT status = 'running' AND context_base_bytes IS NOT NULL
             FROM runs WHERE id = ?1",
            [run_id.to_string()],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(false);
    if active {
        Err(SessionRuntimeError::OutputTooLarge)
    } else {
        Err(SessionRuntimeError::Unavailable)
    }
}

pub(super) fn run_context_tokens(
    connection: &Connection,
    run_id: RunId,
) -> Result<Option<u64>, SessionRuntimeError> {
    connection
        .query_row(
            "SELECT context_tokens FROM runs WHERE id = ?1",
            [run_id.to_string()],
            |row| row.get(0),
        )
        .map_err(|_| SessionRuntimeError::CODEC)
}

pub(super) fn run_cancel_requested(
    connection: &Connection,
    run_id: RunId,
) -> Result<bool, SessionRuntimeError> {
    connection
        .prepare_cached("SELECT cancel_requested FROM runs WHERE id = ?1")
        .and_then(|mut statement| {
            statement
                .query_row([run_id.to_string()], |row| row.get(0))
                .optional()
        })?
        .ok_or(SessionRuntimeError::RunNotFound)
}

pub(super) fn load_run(
    connection: &Connection,
    run_id: RunId,
) -> Result<RunSnapshot, SessionRuntimeError> {
    connection
        .query_row(
            "SELECT session_id, status, outcome_json, prompt_identity_json,
                    resolved_model_json, usage_json, context_tokens,
                    estimated_cost_usd_nanos, limits_json, plan_identity_json, correlation_json,
                    audit_json, final_output_json, origin_json
             FROM runs WHERE id = ?1",
            [run_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<u64>>(6)?,
                    row.get::<_, Option<u64>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<String>>(12)?,
                    row.get::<_, Option<String>>(13)?,
                ))
            },
        )
        .map_err(|_| SessionRuntimeError::CONSTRAINT)
        .and_then(
            |(
                session,
                status,
                outcome,
                prompt_identity,
                resolved_model,
                usage,
                context_tokens,
                cost,
                limits,
                plan_identity,
                correlation,
                audit,
                final_output,
                origin,
            )| {
                Ok(RunSnapshot {
                    origin: origin.as_deref().map(serde_json::from_str).transpose()?,
                    id: run_id,
                    session_id: parse_id(&session)?,
                    status: parse_run_status(&status)?,
                    outcome: outcome.as_deref().map(serde_json::from_str).transpose()?,
                    prompt_identity: prompt_identity
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()?
                        .map(Box::new),
                    resolved_model: resolved_model
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()?
                        .map(Box::new),
                    plan: plan_identity
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()?
                        .map(Box::new),
                    correlation: parse_correlation(correlation.as_deref())?,
                    usage: usage.as_deref().map(serde_json::from_str).transpose()?,
                    context_tokens,
                    estimated_cost_usd_nanos: cost,
                    limits: {
                        let limits = parse_run_limits(limits.as_deref())?;
                        (!limits.is_empty()).then(|| Box::new(limits))
                    },
                    audit: audit
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()?
                        .map(Box::new),
                    final_output: final_output
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()?
                        .map(Box::new),
                })
            },
        )
}
