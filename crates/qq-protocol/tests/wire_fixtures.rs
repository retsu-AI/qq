//! Golden wire encodings for the current protocol version, plus decode-only
//! coverage of every retained historical version.
//!
//! Each fixture under `tests/fixtures/v<PROTOCOL_VERSION>/` is the exact JSON
//! a conforming peer sends or receives. The test decodes every fixture into
//! its Rust type, re-encodes it, and requires byte equality with the file, so
//! a field rename, reorder, or default change fails here before any client
//! notices. Set `QQ_UPDATE_FIXTURES=1` to rewrite the files from the Rust
//! values after an intentional protocol change (and bump `PROTOCOL_VERSION`).
//!
//! Older directories are never rewritten: `historical_fixtures_still_decode`
//! proves that a record a peer on that version produced is still accepted.

use std::{collections::BTreeMap, fs, path::PathBuf};

use qq_protocol::{
    AgentPlanDigest, AgentProfileId, AgentProfileSummary, ApprovalDecision, ApprovalGrant,
    ApprovalMode, BudgetExhaustion, BudgetLimitKind, CAPABILITIES_VERSION, CapabilitiesRequest,
    CapabilitySupport, CommandId, CommandOutcome, CommandReceipt, CommandRequest, ContentHash,
    Correlation, CredentialEpoch, EventCapabilities, EventCursor, GenerationCapabilities,
    InputPart, InputPartKind, InstructionHash, LimitCapabilities, MessageId, MessageRole,
    MessageSnapshot, MessageState, ModelSelection, PROTOCOL_VERSION, PackSummary,
    PromptCacheCapabilities, PromptVersion, ResolvedModel, ResolvedModelVersion, RunActivity,
    RunFailure, RunFailureKind, RunId, RunLimits, RunOutcome, RunPlanIdentity, RunPromptIdentity,
    RunSnapshot, RunStatus, ServerCapabilities, ServerInfo, SessionCommand, SessionCommandKind,
    SessionEvent, SessionEventEnvelope, SessionId, SessionStatus, SessionSummary,
    SkillCapabilities, SteeringCapabilities, StoreId, TokenUsage, ToolCallId, ToolCapabilities,
    ToolExposure, ToolHostSummary, WorkspaceId, WorkspaceToolCapabilities,
};
use serde::{Serialize, de::DeserializeOwned};

fn hash(byte: u8) -> ContentHash {
    ContentHash::from_bytes([byte; 32])
}

fn cursor(sequence: u64) -> EventCursor {
    EventCursor {
        store_id: StoreId::from_bytes([1; 16]),
        workspace_id: WorkspaceId::from_bytes([2; 16]),
        sequence,
    }
}

fn correlation(pairs: &[(&str, &str)]) -> Correlation {
    Correlation::new(
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect::<BTreeMap<_, _>>(),
    )
    .unwrap()
}

fn summary() -> SessionSummary {
    SessionSummary {
        id: SessionId::from_bytes([3; 16]),
        workspace_id: WorkspaceId::from_bytes([2; 16]),
        parent_id: None,
        spawned_by: None,
        purpose: qq_protocol::SessionPurpose::Task,
        title: "Fix the login redirect".to_owned(),
        status: SessionStatus::Running,
        active_run_id: Some(RunId::from_bytes([4; 16])),
        activity: Some(RunActivity::GeneratingResponse),
        queued_prompts: 1,
        model: Some("openai/gpt-5.6".to_owned()),
        profile: AgentProfileId::new("review").unwrap(),
        approval_mode: ApprovalMode::ReadOnly,
        correlation: correlation(&[("thread", "t-1")]),
        context_tokens: Some(1200),
        accounting: None,
        estimated_cost_usd_nanos: Some(42),
        updated_at_ms: 1_700_000_000_000,
        last_outcome: None,
    }
}

fn plan_identity() -> RunPlanIdentity {
    RunPlanIdentity {
        profile: AgentProfileId::new("review").unwrap(),
        descriptor_version: 2,
        digest: AgentPlanDigest::from_hash(hash(0xaa)),
        credential_epoch: CredentialEpoch::new(3),
    }
}

fn resolved_model() -> ResolvedModel {
    ResolvedModel {
        version: ResolvedModelVersion::new(2).unwrap(),
        request_shape: None,
        route: "openai/gpt-5.6".to_owned(),
        provider_model: "gpt-5.6".to_owned(),
        organization: None,
        credential_profile: None,
        max_output_tokens: 4096,
        context_window: Some(400_000),
        pricing: None,
        output_token_control: CapabilitySupport::Native,
        generation: GenerationCapabilities {
            reasoning_effort: CapabilitySupport::Native,
        },
        prompt_cache: PromptCacheCapabilities {
            control: CapabilitySupport::Unsupported,
            cache_read_usage: true,
            cache_write_usage: false,
        },
    }
}

fn message(byte: u8, steering: bool, state: MessageState) -> MessageSnapshot {
    MessageSnapshot {
        id: MessageId::from_bytes([byte; 16]),
        session_id: SessionId::from_bytes([3; 16]),
        run_id: RunId::from_bytes([4; 16]),
        turn_ordinal: 0,
        role: MessageRole::User,
        state,
        steering,
        truncated: false,
        output: if steering {
            "also check the tests".to_owned()
        } else {
            "Fix the login redirect\n@src/auth.rs".to_owned()
        },
        refusal: String::new(),
        created_at_ms: 1_700_000_000_000,
    }
}

fn envelope(sequence: u64, event: SessionEvent) -> SessionEventEnvelope {
    SessionEventEnvelope {
        cursor: cursor(sequence),
        session_id: SessionId::from_bytes([3; 16]),
        run_id: Some(RunId::from_bytes([4; 16])),
        caused_by: Some(CommandId::from_bytes([7; 16])),
        occurred_at_ms: 1_700_000_000_000,
        event,
    }
}

fn fixture_dir(version: u16) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/v{version}"))
}

fn check<T>(name: &str, value: &T)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let path = fixture_dir(PROTOCOL_VERSION).join(format!("{name}.json"));
    let encoded = serde_json::to_string_pretty(value).unwrap() + "\n";
    if std::env::var_os("QQ_UPDATE_FIXTURES").is_some() {
        fs::write(&path, &encoded).unwrap();
    }
    let stored = fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!("{}: {error}; run with QQ_UPDATE_FIXTURES=1", path.display())
    });
    let decoded: T = serde_json::from_str(&stored)
        .unwrap_or_else(|error| panic!("{}: does not decode: {error}", path.display()));
    assert_eq!(&decoded, value, "{}: decoded value drifted", path.display());
    assert_eq!(
        stored,
        encoded,
        "{}: encoding drifted; bump PROTOCOL_VERSION and rerun with QQ_UPDATE_FIXTURES=1",
        path.display()
    );
}

#[test]
fn current_version_commands_receipts_events_and_capabilities_match_their_goldens() {
    assert_eq!(PROTOCOL_VERSION, 18);
    let session_id = SessionId::from_bytes([3; 16]);
    let run_id = RunId::from_bytes([4; 16]);
    let command = |byte: u8, command: SessionCommand| CommandRequest {
        command_id: CommandId::from_bytes([byte; 16]),
        command,
    };

    check(
        "command_create_session",
        &command(
            0x10,
            SessionCommand::CreateSession {
                workspace_id: WorkspaceId::from_bytes([2; 16]),
                parent_id: None,
                model: ModelSelection {
                    model: Some("openai/gpt-5.6".to_owned()),
                    max_output_tokens: Some(4096),
                    organization: None,
                },
                approval_mode: ApprovalMode::Ask,
                profile: AgentProfileId::new("review").unwrap(),
                correlation: correlation(&[("channel", "c-9"), ("thread", "t-1")]),
            },
        ),
    );
    check(
        "command_create_session_minimal",
        &command(
            0x11,
            SessionCommand::CreateSession {
                workspace_id: WorkspaceId::from_bytes([2; 16]),
                parent_id: None,
                model: ModelSelection::default(),
                approval_mode: ApprovalMode::default(),
                profile: AgentProfileId::default(),
                correlation: Correlation::default(),
            },
        ),
    );
    check(
        "command_submit_prompt",
        &command(
            0x12,
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![
                    InputPart::text("Fix the login redirect"),
                    InputPart::WorkspaceFile {
                        path: "src/auth.rs".to_owned(),
                        expected_hash: Some(hash(0x11)),
                    },
                ],
                limits: RunLimits {
                    max_duration_ms: Some(600_000),
                    max_model_turns: Some(40),
                    max_tool_calls: None,
                    max_total_tokens: None,
                    max_cost_usd_nanos: Some(2_000_000_000),
                    max_input_tokens: None,
                    max_output_tokens: Some(50_000),
                    max_tool_output_bytes: Some(4_000_000),
                    max_children: Some(2),
                    max_concurrent_children: Some(1),
                },
                correlation: correlation(&[("job", "j-1")]),
            },
        ),
    );
    check(
        "command_steer_run",
        &command(
            0x13,
            SessionCommand::SteerRun {
                run_id,
                input: vec![InputPart::text("also check the tests")],
                interrupt: false,
            },
        ),
    );
    check(
        "command_steer_run_interrupt",
        &command(
            0x14,
            SessionCommand::SteerRun {
                run_id,
                input: vec![InputPart::text("stop, do it differently")],
                interrupt: true,
            },
        ),
    );
    check(
        "command_cancel_run",
        &command(0x15, SessionCommand::CancelRun { run_id }),
    );
    check(
        "command_respond_tool_approval",
        &command(
            0x16,
            SessionCommand::RespondToolApproval {
                run_id,
                tool_call_id: ToolCallId::from_bytes([5; 16]),
                decision: ApprovalDecision::ApproveForSession {
                    grant: ApprovalGrant::ShellPrefix {
                        prefix: "cargo test".to_owned(),
                    },
                },
            },
        ),
    );
    check(
        "command_set_session_profile",
        &command(
            0x17,
            SessionCommand::SetSessionProfile {
                session_id,
                profile: AgentProfileId::new("fast").unwrap(),
            },
        ),
    );

    let receipt = |byte: u8, sequence: u64, outcome: CommandOutcome| CommandReceipt {
        command_id: CommandId::from_bytes([byte; 16]),
        committed_through: cursor(sequence),
        outcome,
    };
    check(
        "receipt_prompt_queued",
        &receipt(
            0x12,
            10,
            CommandOutcome::PromptQueued {
                session_id,
                run_id,
                queue_position: 1,
            },
        ),
    );
    check(
        "receipt_steering_queued",
        &receipt(
            0x13,
            11,
            CommandOutcome::SteeringQueued {
                run_id,
                message_id: MessageId::from_bytes([9; 16]),
            },
        ),
    );
    check(
        "receipt_run_already_finished",
        &receipt(
            0x15,
            12,
            CommandOutcome::RunAlreadyFinished {
                run_id,
                outcome: RunOutcome::BudgetExhausted {
                    exhaustion: Box::new(BudgetExhaustion {
                        limit: BudgetLimitKind::ToolOutputBytes,
                        final_response: true,
                        message: "the run's 4000123 bytes of tool output exceeded its 4000000 byte budget".to_owned(),
                    }),
                },
            },
        ),
    );
    check(
        "receipt_session_profile_set",
        &receipt(
            0x17,
            13,
            CommandOutcome::SessionProfileSet {
                session_id,
                profile: AgentProfileId::new("fast").unwrap(),
            },
        ),
    );

    check(
        "event_prompt_queued",
        &envelope(
            10,
            SessionEvent::PromptQueued {
                session: summary(),
                message: message(0x20, false, MessageState::Queued),
                run: Box::new(RunSnapshot {
                    id: run_id,
                    session_id,
                    status: RunStatus::Queued,
                    outcome: None,
                    prompt_identity: None,
                    resolved_model: None,
                    plan: None,
                    correlation: correlation(&[("job", "j-1")]),
                    usage: None,
                    context_tokens: None,
                    estimated_cost_usd_nanos: None,
                    limits: Some(Box::new(RunLimits {
                        max_model_turns: Some(40),
                        ..RunLimits::default()
                    })),
                    audit: None,
                }),
                queue_position: 1,
            },
        ),
    );
    check(
        "event_run_started",
        &envelope(
            14,
            SessionEvent::RunStarted {
                session: summary(),
                run_id,
                plan: Some(Box::new(plan_identity())),
            },
        ),
    );
    check(
        "event_steering_queued",
        &envelope(
            15,
            SessionEvent::SteeringQueued {
                run_id,
                message: message(0x21, true, MessageState::Queued),
            },
        ),
    );
    check(
        "event_steering_applied",
        &envelope(
            16,
            SessionEvent::SteeringApplied {
                run_id,
                message_id: MessageId::from_bytes([0x21; 16]),
                turn_ordinal: 3,
            },
        ),
    );
    check(
        "event_steering_superseded",
        &envelope(
            17,
            SessionEvent::SteeringSuperseded {
                run_id,
                message_id: MessageId::from_bytes([0x22; 16]),
            },
        ),
    );
    check(
        "event_run_interrupted",
        &envelope(
            18,
            SessionEvent::RunInterrupted {
                run_id,
                turn_ordinal: 3,
            },
        ),
    );
    check(
        "event_run_finished_failed",
        &envelope(
            19,
            SessionEvent::RunFinished {
                session: summary(),
                run_id,
                outcome: RunOutcome::Failed {
                    failure: RunFailure {
                        kind: RunFailureKind::InvalidCommand,
                        message: "workspace file \"src/auth.rs\" changed: expected content hash 11…, found 22…".to_owned(),
                    },
                },
                usage: Some(TokenUsage {
                    input_tokens: 10,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    output_tokens: 4,
                    reasoning_tokens: None,
                }),
                context_tokens: Some(10),
            },
        ),
    );
    check(
        "event_run_output_truncated",
        &envelope(
            20,
            SessionEvent::RunOutputTruncated {
                run_id,
                turn_ordinal: 2,
                continuation: 1,
            },
        ),
    );
    check(
        "event_run_finished_output_truncated",
        &envelope(
            21,
            SessionEvent::RunFinished {
                session: summary(),
                run_id,
                outcome: RunOutcome::Failed {
                    failure: RunFailure {
                        kind: RunFailureKind::ProviderOutputTruncated,
                        message: "the provider stopped at its output token limit (16384 tokens) on 4 consecutive turns; the partial answer is in the transcript".to_owned(),
                    },
                },
                usage: None,
                context_tokens: None,
            },
        ),
    );
    check(
        "event_run_audit_started",
        &envelope(
            23,
            SessionEvent::RunAuditStarted {
                run_id,
                audit_session_id: SessionId::from_bytes([0x33; 16]),
            },
        ),
    );
    check(
        "event_run_audit_completed",
        &envelope(
            24,
            SessionEvent::RunAuditCompleted {
                run_id,
                audit: qq_protocol::AuditRecord {
                    outcome: qq_protocol::AuditOutcome::Revised,
                    findings: vec!["src/auth.rs still redirects to /login".to_owned()],
                    revisions: 0,
                    usage: Some(TokenUsage {
                        input_tokens: 900,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                        output_tokens: 40,
                        reasoning_tokens: None,
                    }),
                    estimated_cost_usd_nanos: Some(1_200),
                },
            },
        ),
    );
    check(
        "event_assistant_message_started_truncated",
        &envelope(
            22,
            SessionEvent::AssistantMessageStarted {
                message: MessageSnapshot {
                    role: MessageRole::Assistant,
                    truncated: true,
                    output: "The first part of a long answer".to_owned(),
                    ..message(0x21, false, MessageState::Complete)
                },
            },
        ),
    );
    check(
        "snapshot_run_with_plan_identity",
        &RunSnapshot {
            id: run_id,
            session_id,
            status: RunStatus::Completed,
            outcome: Some(RunOutcome::Completed),
            prompt_identity: Some(Box::new(RunPromptIdentity {
                version: PromptVersion::new(7).unwrap(),
                instruction_hash: InstructionHash::from_bytes([1; 32]),
                system_prompt_hash: Some(hash(2)),
                tool_schema_hash: Some(hash(3)),
                selected_guidance: None,
                catalog_digest: Some(hash(9)),
                exposure: Some(ToolExposure::Full),
                context_sources: Vec::new(),
            })),
            resolved_model: Some(Box::new(resolved_model())),
            plan: Some(Box::new(plan_identity())),
            correlation: Correlation::default(),
            usage: None,
            context_tokens: Some(1200),
            estimated_cost_usd_nanos: Some(42),
            limits: None,
            audit: None,
        },
    );

    check("capabilities_request", &CapabilitiesRequest::default());
    check(
        "capabilities_request_workspace",
        &CapabilitiesRequest {
            workspace_id: Some(WorkspaceId::from_bytes([2; 16])),
        },
    );
    check(
        "capabilities",
        &ServerCapabilities {
            version: CAPABILITIES_VERSION,
            protocol_version: PROTOCOL_VERSION,
            server_version: "0.1.0".to_owned(),
            input_parts: InputPartKind::ALL.to_vec(),
            commands: SessionCommandKind::ALL.to_vec(),
            steering: SteeringCapabilities {
                boundary: true,
                interrupt: true,
                max_pending_per_run: 4,
            },
            limits: LimitCapabilities {
                supported: BudgetLimitKind::ALL.to_vec(),
                max_request_bytes: 1_048_576,
                max_event_bytes: 1_048_576,
                max_input_parts: 32,
                max_input_text_bytes: 131_072,
                max_input_file_parts: 8,
                max_input_file_bytes: 262_144,
                max_pending_prompts: 16,
                max_children: 8,
                max_concurrent_children: 3,
                max_child_depth: 1,
                max_correlation_entries: 8,
                max_output_continuations: 3,
                max_descendants: 24,
            },
            approvals: vec![
                "approve_once".to_owned(),
                "approve_for_session".to_owned(),
                "approve_for_workspace".to_owned(),
                "deny".to_owned(),
            ],
            approval_modes: vec![
                ApprovalMode::ReadOnly,
                ApprovalMode::Ask,
                ApprovalMode::Auto,
                ApprovalMode::Full,
            ],
            profiles: Some(vec![
                AgentProfileSummary {
                    id: AgentProfileId::default(),
                    model: Some("openai/gpt-5.6".to_owned()),
                    approval_mode: ApprovalMode::Auto,
                    pack: None,
                },
                AgentProfileSummary {
                    id: AgentProfileId::new("review").unwrap(),
                    model: Some("anthropic/claude-x".to_owned()),
                    approval_mode: ApprovalMode::ReadOnly,
                    pack: Some(PackSummary {
                        id: "review-kit".to_owned(),
                        version: "1.2.0".to_owned(),
                    }),
                },
            ]),
            tools: ToolCapabilities {
                max_catalog_tools: 512,
                max_tool_schema_bytes: 16_384,
                max_catalog_schema_bytes: 1_048_576,
                full_exposure_tools: 24,
                full_exposure_schema_bytes: 32_768,
                max_pinned_tools: 32,
                max_indexed_skills: 64,
                external_prefixes: vec!["mcp__".to_owned(), "ext__".to_owned()],
            },
            workspace_tools: Some(WorkspaceToolCapabilities {
                catalog_digest: hash(0xcc),
                exposure: ToolExposure::Progressive,
                hosts: vec![ToolHostSummary {
                    name: "mcp".to_owned(),
                    generation: 3,
                    tool_count: 40,
                    ready: true,
                    message: None,
                }],
                excluded_tools: 1,
                skills: SkillCapabilities {
                    digest: hash(0xdd),
                    indexed: 2,
                    disclosed: 1,
                    truncated: false,
                    entries: vec![
                        qq_protocol::SkillSummary {
                            name: "deploy".to_owned(),
                            kind: qq_protocol::GuidanceKind::Command,
                            source: ".qq/commands/deploy.md".to_owned(),
                            description: "Ship the current branch.".to_owned(),
                            disclosed: true,
                        },
                        qq_protocol::SkillSummary {
                            name: "audit".to_owned(),
                            kind: qq_protocol::GuidanceKind::Skill,
                            source: "pack:review-kit/skills/audit/SKILL.md".to_owned(),
                            description: String::new(),
                            disclosed: false,
                        },
                    ],
                },
            }),
            events: EventCapabilities {
                post_commit: true,
                replay_page: 128,
                max_subscriptions: 64,
                max_event_bytes: 1_048_576,
                retention_bounded: false,
            },
            delegation: Some(qq_protocol::DelegationCapabilities {
                roster: qq_protocol::DelegationRoster {
                    roster: vec![
                        qq_protocol::DelegationRosterEntry {
                            route: "openai/gpt-5-mini".to_owned(),
                            role: qq_protocol::DelegationRole::Fast,
                            note: Some("lookups, breadth".to_owned()),
                            context_window: Some(400_000),
                            max_output_tokens: Some(16_384),
                            relative_cost_permille: Some(150),
                        },
                        qq_protocol::DelegationRosterEntry {
                            route: "openai/gpt-5.6".to_owned(),
                            role: qq_protocol::DelegationRole::Balanced,
                            note: None,
                            context_window: Some(400_000),
                            max_output_tokens: None,
                            relative_cost_permille: Some(1000),
                        },
                    ],
                    default_role: qq_protocol::DelegationRole::Balanced,
                    max_depth: 1,
                    write_children: false,
                },
                max_roster_entries: 8,
                max_depth_ceiling: 3,
            }),
        },
    );
    check(
        "server_info",
        &ServerInfo {
            protocol_version: PROTOCOL_VERSION,
            version: "0.1.0".to_owned(),
            pid: 4242,
            server_id: StoreId::from_bytes([1; 16]),
            display_name: "build-box".to_owned(),
        },
    );
}

#[test]
fn inbound_types_reject_unknown_fields_and_response_types_tolerate_them() {
    let base = fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
        "tests/fixtures/v{PROTOCOL_VERSION}/command_submit_prompt.json"
    )))
    .unwrap();
    let mut with_extra: serde_json::Value = serde_json::from_str(&base).unwrap();
    with_extra["command"]["future"] = serde_json::json!(true);
    assert!(serde_json::from_value::<CommandRequest>(with_extra.clone()).is_err());
    with_extra["command"]
        .as_object_mut()
        .unwrap()
        .remove("future");
    with_extra["command"]["input"][0]["future"] = serde_json::json!(true);
    assert!(serde_json::from_value::<CommandRequest>(with_extra.clone()).is_err());
    with_extra["command"]["input"][0]
        .as_object_mut()
        .unwrap()
        .remove("future");
    with_extra["command"]["limits"]["max_pizzas"] = serde_json::json!(1);
    assert!(serde_json::from_value::<CommandRequest>(with_extra).is_err());

    let capabilities =
        fs::read_to_string(fixture_dir(PROTOCOL_VERSION).join("capabilities.json")).unwrap();
    let mut newer: serde_json::Value = serde_json::from_str(&capabilities).unwrap();
    newer["future_section"] = serde_json::json!({"anything": 1});
    newer["limits"]["max_future"] = serde_json::json!(1);
    newer["steering"]["telepathy"] = serde_json::json!(false);
    newer["profiles"][0]["future"] = serde_json::json!(1);
    let decoded: ServerCapabilities = serde_json::from_value(newer).unwrap();
    assert_eq!(decoded.protocol_version, PROTOCOL_VERSION);

    // Events and snapshots stay strict: a server never sends what a client
    // cannot name, and both bump the version together.
    let started =
        fs::read_to_string(fixture_dir(PROTOCOL_VERSION).join("event_run_started.json")).unwrap();
    let mut event: serde_json::Value = serde_json::from_str(&started).unwrap();
    event["event"]["plan"]["future"] = serde_json::json!(1);
    assert!(serde_json::from_value::<SessionEventEnvelope>(event).is_err());
    // Historical envelopes without plan identity still decode.
    let mut legacy: serde_json::Value = serde_json::from_str(&started).unwrap();
    legacy["event"].as_object_mut().unwrap().remove("plan");
    legacy["event"]["session"]
        .as_object_mut()
        .unwrap()
        .remove("profile");
    legacy["event"]["session"]
        .as_object_mut()
        .unwrap()
        .remove("correlation");
    let decoded: SessionEventEnvelope = serde_json::from_value(legacy).unwrap();
    let SessionEvent::RunStarted { plan, session, .. } = decoded.event else {
        panic!("run started")
    };
    assert!(plan.is_none());
    assert!(session.profile.is_default());
    assert!(session.correlation.is_empty());
}

/// Every retained historical fixture directory still decodes into today's
/// types. This is decode-only: older encodings are not required to round-trip
/// byte-for-byte (a widened field or an added optional field changes the
/// current encoding without invalidating what an older peer produced).
#[test]
fn historical_fixtures_still_decode() {
    const RETAINED: &[u16] = &[17];
    for &version in RETAINED {
        assert!(version < PROTOCOL_VERSION);
        let directory = fixture_dir(version);
        let mut entries: Vec<PathBuf> = fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("{}: {error}", directory.display()))
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect();
        entries.sort();
        assert!(!entries.is_empty(), "{}: no fixtures", directory.display());
        for path in entries {
            let name = path.file_stem().unwrap().to_str().unwrap();
            let stored = fs::read_to_string(&path).unwrap();
            fn decode<T: DeserializeOwned>(path: &std::path::Path, stored: &str) -> T {
                serde_json::from_str(stored).unwrap_or_else(|error| {
                    panic!(
                        "{}: does not decode as version {PROTOCOL_VERSION}: {error}",
                        path.display()
                    )
                })
            }
            // The prefix selects the type exactly as the golden test named it.
            if name.starts_with("command_") {
                decode::<CommandRequest>(&path, &stored);
            } else if name.starts_with("receipt_") {
                decode::<CommandReceipt>(&path, &stored);
            } else if name.starts_with("event_") {
                decode::<SessionEventEnvelope>(&path, &stored);
            } else if name.starts_with("snapshot_") {
                decode::<RunSnapshot>(&path, &stored);
            } else if name.starts_with("capabilities_request") {
                decode::<CapabilitiesRequest>(&path, &stored);
            } else if name == "capabilities" {
                let decoded = decode::<ServerCapabilities>(&path, &stored);
                assert_eq!(decoded.protocol_version, version);
            } else if name == "server_info" {
                let decoded = decode::<ServerInfo>(&path, &stored);
                assert_eq!(decoded.protocol_version, version);
            } else {
                panic!(
                    "{}: unrecognized fixture prefix; extend this test",
                    path.display()
                );
            }
        }
    }
}

/// Version 18 widened the model-turn limit and ordinals to `u32`. A limit or
/// ordinal above the old `u16` range is accepted on the wire and round-trips;
/// a value above `u32` is rejected. No turns are executed here: this pins the
/// accepted range, not the runtime's ability to reach it.
#[test]
fn model_turn_limits_and_ordinals_accept_the_full_u32_range() {
    const ABOVE_U16: u32 = 65_536;

    let limits = RunLimits {
        max_model_turns: Some(ABOVE_U16),
        ..RunLimits::default()
    };
    let encoded = serde_json::to_string(&limits).unwrap();
    assert!(encoded.contains(&ABOVE_U16.to_string()));
    let decoded: RunLimits = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded.max_model_turns, Some(ABOVE_U16));

    let max: RunLimits =
        serde_json::from_str(&format!(r#"{{"max_model_turns":{}}}"#, u32::MAX)).unwrap();
    assert_eq!(max.max_model_turns, Some(u32::MAX));
    let too_wide = format!(r#"{{"max_model_turns":{}}}"#, u64::from(u32::MAX) + 1);
    assert!(serde_json::from_str::<RunLimits>(&too_wide).is_err());

    let mut message = message(0x21, false, MessageState::Complete);
    message.turn_ordinal = ABOVE_U16;
    let round_trip: MessageSnapshot =
        serde_json::from_str(&serde_json::to_string(&message).unwrap()).unwrap();
    assert_eq!(round_trip.turn_ordinal, ABOVE_U16);
}
