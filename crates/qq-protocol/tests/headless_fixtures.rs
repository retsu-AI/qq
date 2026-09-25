//! Golden `qq run --format jsonl` streams for the current protocol version.
//!
//! Each `tests/fixtures/headless/v<PROTOCOL_VERSION>/<scenario>.jsonl` is a
//! complete trial stream exactly as the binary writes it: compact records,
//! one per line, `trial` first and `outcome` last. The test decodes every
//! line as a strict [`HeadlessRecord`], re-encodes it, and requires byte
//! equality, so a field rename, reorder, default change, or new tag fails
//! here before any supervisor notices. Set `QQ_UPDATE_FIXTURES=1` to rewrite
//! the files from the Rust values after an intentional protocol change (and
//! bump `PROTOCOL_VERSION`).
//!
//! Older directories are never rewritten: `historical_streams_still_decode`
//! proves that a stream a peer on that version recorded is still accepted,
//! which is the compatibility statement `headless-contract.md` makes.

use std::{fs, path::PathBuf};

use qq_protocol::{
    AgentProfileId, ApprovalMode, AuditOutcome, AuditRecord, BudgetExhaustion, BudgetLimitKind,
    ContentHash, Correlation, EventCursor, FinalOutput, HeadlessApproval, HeadlessOutcome,
    HeadlessRecord, HeadlessStatus, HeadlessTrial, InstructionHash, MessageId, MessageRole,
    MessageSnapshot, MessageState, ModelSelection, PROTOCOL_VERSION, PromptVersion, Question,
    QuestionPreview, RunActivity, RunFailure, RunFailureKind, RunId, RunOutcome, RunPromptIdentity,
    SessionEvent, SessionEventEnvelope, SessionId, SessionPurpose, SessionStatus, SessionSummary,
    StoreId, TextChannel, TokenUsage, ToolCallId, ToolCallSnapshot, ToolCallState, WorkspaceId,
};

const WORKSPACE: WorkspaceId = WorkspaceId::from_bytes([0xab; 16]);
const SESSION: SessionId = SessionId::from_bytes([0xac; 16]);
const RUN: RunId = RunId::from_bytes([0xad; 16]);

fn fixture_dir(version: u16) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/headless/v{version}"))
}

fn trial() -> HeadlessTrial {
    HeadlessTrial {
        qq_version: "0.1.0".to_owned(),
        qq_source_revision: "fixture-revision".to_owned(),
        protocol_version: PROTOCOL_VERSION,
        workspace_identity: ContentHash::from_bytes([0x44; 32]),
        model: ModelSelection {
            model_is_fallback: false,
            model: Some("anthropic/claude-sonnet-4-5".to_owned()),
            max_output_tokens: Some(32_000),
            organization: None,
        },
        profile: AgentProfileId::new("default").unwrap(),
        context_window: Some(200_000),
        pricing_provenance: Some("fixture".to_owned()),
        approval: HeadlessApproval::Auto,
        timeout_seconds: Some(60),
        max_turns: None,
        max_cost_usd_nanos: None,
        correlation: Correlation::default(),
        arm: None,
        output_schema_sha256: None,
        output_repair_turns: None,
        workspace_id: WORKSPACE,
        session_id: SESSION,
        run_id: RUN,
    }
}

fn summary(status: SessionStatus, active: bool) -> SessionSummary {
    SessionSummary {
        model_is_fallback: false,
        id: SESSION,
        workspace_id: WORKSPACE,
        parent_id: None,
        spawned_by: None,
        purpose: SessionPurpose::Task,
        title: "Fix the login redirect".to_owned(),
        status,
        active_run_id: active.then_some(RUN),
        activity: active.then_some(RunActivity::GeneratingResponse),
        queued_prompts: 0,
        model: Some("anthropic/claude-sonnet-4-5".to_owned()),
        profile: AgentProfileId::new("default").unwrap(),
        approval_mode: ApprovalMode::Auto,
        approval_delegate: None,
        reasoning_effort: None,
        correlation: Correlation::default(),
        context_tokens: None,
        accounting: None,
        estimated_cost_usd_nanos: None,
        updated_at_ms: 1_700_000_000_000,
        last_outcome: None,
    }
}

fn envelope(sequence: u64, event: SessionEvent) -> SessionEventEnvelope {
    SessionEventEnvelope {
        cursor: EventCursor {
            store_id: StoreId::from_bytes([0xaa; 16]),
            workspace_id: WORKSPACE,
            sequence,
        },
        session_id: SESSION,
        run_id: Some(RUN),
        caused_by: None,
        occurred_at_ms: 1_700_000_000_000 + sequence,
        event,
    }
}

fn prompt_identity() -> RunPromptIdentity {
    RunPromptIdentity {
        version: PromptVersion::new(7).unwrap(),
        instruction_hash: InstructionHash::from_bytes([0x11; 32]),
        system_prompt_hash: Some(ContentHash::from_bytes([0x22; 32])),
        tool_schema_hash: Some(ContentHash::from_bytes([0x33; 32])),
        selected_guidance: None,
        catalog_digest: None,
        exposure: None,
        context_sources: Vec::new(),
    }
}

fn usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 1_200,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        output_tokens: 80,
        reasoning_tokens: None,
    }
}

/// The events every completed stream shares: the run starts, the assistant
/// answers, the run finishes. Real streams carry more (tool calls, session
/// updates); the goldens pin the shapes, not a transcript.
fn run_events(outcome: RunOutcome, final_output: Option<Box<FinalOutput>>) -> Vec<HeadlessRecord> {
    let message = |state: MessageState, output: &str| MessageSnapshot {
        id: MessageId::from_bytes([0xae; 16]),
        session_id: SESSION,
        run_id: RUN,
        turn_ordinal: 1,
        role: MessageRole::Assistant,
        state,
        steering: false,
        truncated: false,
        output: output.to_owned(),
        refusal: String::new(),
        created_at_ms: 1_700_000_000_002,
    };
    let event = |sequence: u64, event: SessionEvent| HeadlessRecord::Event {
        envelope: Box::new(envelope(sequence, event)),
    };
    vec![
        event(
            1,
            SessionEvent::RunStarted {
                session: Box::new(summary(SessionStatus::Running, true)),
                run_id: RUN,
                plan: None,
            },
        ),
        event(
            2,
            SessionEvent::AssistantMessageStarted {
                message: message(MessageState::Streaming, ""),
            },
        ),
        event(
            3,
            SessionEvent::TextAppended {
                message_id: MessageId::from_bytes([0xae; 16]),
                channel: TextChannel::Output,
                text: "Done: the redirect now targets /dashboard.".to_owned(),
            },
        ),
        event(
            4,
            SessionEvent::ModelTurnCompleted {
                run_id: RUN,
                turn_ordinal: 1,
                model: ModelSelection {
                    model_is_fallback: false,
                    model: Some("anthropic/claude-sonnet-4-5".to_owned()),
                    max_output_tokens: Some(32_000),
                    organization: None,
                },
                usage: Some(usage()),
                estimated_cost_usd_nanos: Some(4_800_000),
            },
        ),
        event(
            5,
            SessionEvent::CheckpointStarted {
                run_id: RUN,
                correlation: "final:1".to_owned(),
                phase: qq_protocol::CheckpointPhase::FinalCandidate,
                tool_call_id: None,
            },
        ),
        event(
            6,
            SessionEvent::CheckpointReviewed {
                spend: Some(qq_protocol::CheckpointSpend {
                    usage: Some(qq_protocol::TokenUsage::default()),
                    estimated_cost_usd_nanos: Some(0),
                }),
                run_id: RUN,
                correlation: "final:1".to_owned(),
                phase: qq_protocol::CheckpointPhase::FinalCandidate,
                tool_call_id: None,
                outcome: qq_protocol::CheckpointOutcome::Supported,
                confidence_basis_points: Some(9_500),
                feedback: "JEV evidence support; not guaranteed correctness".to_owned(),
            },
        ),
        event(
            7,
            SessionEvent::RunFinished {
                session: Box::new(summary(SessionStatus::Idle, false)),
                run_id: RUN,
                outcome,
                usage: Some(usage()),
                context_tokens: Some(1_280),
                final_output,
            },
        ),
    ]
}

fn outcome(status: HeadlessStatus) -> HeadlessOutcome {
    HeadlessOutcome {
        status,
        exit_code: status.code(),
        message: None,
        usage: Some(usage()),
        estimated_cost_usd_nanos: Some(4_800_000),
        prompt_identity: Some(Box::new(prompt_identity())),
        audit: None,
        final_output: None,
    }
}

fn check(name: &str, stream: &[HeadlessRecord]) {
    let path = fixture_dir(PROTOCOL_VERSION).join(format!("{name}.jsonl"));
    let mut encoded = String::new();
    for record in stream {
        encoded.push_str(&serde_json::to_string(record).unwrap());
        encoded.push('\n');
    }
    if std::env::var_os("QQ_UPDATE_FIXTURES").is_some() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &encoded).unwrap();
    }
    let stored = fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!("{}: {error}; run with QQ_UPDATE_FIXTURES=1", path.display())
    });
    let decoded: Vec<HeadlessRecord> = stored
        .lines()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("{}: does not decode: {error}", path.display()))
        })
        .collect();
    assert_eq!(
        decoded,
        stream,
        "{}: decoded stream drifted",
        path.display()
    );
    assert_eq!(
        stored,
        encoded,
        "{}: encoding drifted; bump PROTOCOL_VERSION and rerun with QQ_UPDATE_FIXTURES=1",
        path.display()
    );
}

/// What every stream promises: one trailing `outcome` whose exit code
/// matches its status; and unless startup failed before a session existed
/// (`invalid_configuration`, where the outcome is the whole stream), one
/// leading `trial` with nothing but events between. Returns the trial.
fn assert_well_formed<'a>(
    path: &std::path::Path,
    stream: &'a [HeadlessRecord],
) -> Option<&'a HeadlessTrial> {
    let (last, before) = stream
        .split_last()
        .unwrap_or_else(|| panic!("{}: empty stream", path.display()));
    let HeadlessRecord::Outcome(outcome) = last else {
        panic!("{}: the last record must be the outcome", path.display());
    };
    assert!(
        outcome.is_well_formed(),
        "{}: exit_code disagrees with status",
        path.display()
    );
    let (first, middle) = match before.split_first() {
        Some(split) => split,
        None => {
            assert_eq!(
                outcome.status,
                HeadlessStatus::InvalidConfiguration,
                "{}: only a startup failure may omit the trial",
                path.display()
            );
            return None;
        }
    };
    let HeadlessRecord::Trial(trial) = first else {
        panic!("{}: the first record must be the trial", path.display());
    };
    let mut previous = None;
    for record in middle {
        let HeadlessRecord::Event { envelope } = record else {
            panic!(
                "{}: only events may sit between trial and outcome",
                path.display()
            );
        };
        assert_eq!(envelope.cursor.workspace_id, trial.workspace_id);
        assert!(
            previous.is_none_or(|sequence| envelope.cursor.sequence > sequence),
            "{}: cursors must be strictly increasing",
            path.display()
        );
        previous = Some(envelope.cursor.sequence);
    }
    Some(trial)
}

#[test]
fn current_version_streams_match_their_goldens() {
    assert_eq!(PROTOCOL_VERSION, 29);

    let stream = |trial: HeadlessTrial, events: Vec<HeadlessRecord>, outcome: HeadlessOutcome| {
        let mut stream = Vec::with_capacity(events.len() + 2);
        stream.push(HeadlessRecord::Trial(Box::new(trial)));
        stream.extend(events);
        stream.push(HeadlessRecord::Outcome(Box::new(outcome)));
        stream
    };

    // The default payload: no optional flag given, no optional field present.
    check(
        "completed",
        &stream(
            trial(),
            run_events(RunOutcome::Completed, None),
            outcome(HeadlessStatus::Completed),
        ),
    );

    // Every optional trial field the flags can set, plus an audited answer.
    check(
        "completed_with_every_option",
        &stream(
            HeadlessTrial {
                max_turns: Some(40),
                max_cost_usd_nanos: Some(2_000_000_000),
                correlation: Correlation::new(
                    [("job", "j-17"), ("thread", "t-1")]
                        .into_iter()
                        .map(|(key, value)| (key.to_owned(), value.to_owned()))
                        .collect(),
                )
                .unwrap(),
                arm: Some("B".to_owned()),
                ..trial()
            },
            run_events(RunOutcome::Completed, None),
            HeadlessOutcome {
                audit: Some(Box::new(AuditRecord {
                    outcome: AuditOutcome::Pass,
                    findings: Vec::new(),
                    revisions: 0,
                    usage: Some(TokenUsage {
                        input_tokens: 900,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                        output_tokens: 40,
                        reasoning_tokens: None,
                    }),
                    estimated_cost_usd_nanos: Some(1_200),
                })),
                ..outcome(HeadlessStatus::Completed)
            },
        ),
    );

    // `--output-schema`: the trial names the contract, the answer validated.
    let valid = Box::new(FinalOutput::Valid {
        value: serde_json::json!({"fixed": true, "files": ["src/auth.rs"]}),
        repair_turns: 1,
    });
    check(
        "completed_final_output_valid",
        &stream(
            HeadlessTrial {
                output_schema_sha256: Some(ContentHash::from_bytes([0x55; 32])),
                output_repair_turns: Some(2),
                ..trial()
            },
            run_events(RunOutcome::Completed, Some(valid.clone())),
            HeadlessOutcome {
                final_output: Some(valid),
                ..outcome(HeadlessStatus::Completed)
            },
        ),
    );

    // The answer never satisfied the contract: the run completed, the
    // invocation is `task_failed`, and the errors are on both records.
    let invalid = Box::new(FinalOutput::Invalid {
        errors: vec![
            "/: missing required property \"fixed\"".to_owned(),
            "/files/0: expected string, found number".to_owned(),
        ],
        repair_turns: 2,
    });
    check(
        "task_failed_final_output_invalid",
        &stream(
            HeadlessTrial {
                output_schema_sha256: Some(ContentHash::from_bytes([0x55; 32])),
                output_repair_turns: Some(2),
                ..trial()
            },
            run_events(RunOutcome::Completed, Some(invalid.clone())),
            HeadlessOutcome {
                message: Some(
                    "the final answer did not satisfy the output schema after 2 repair turn(s): \
                     /: missing required property \"fixed\"; /files/0: expected string, found number"
                        .to_owned(),
                ),
                final_output: Some(invalid),
                ..outcome(HeadlessStatus::Completed)
            }
            .with_status(HeadlessStatus::TaskFailed),
        ),
    );

    // The agent reported a failure.
    check(
        "task_failed",
        &stream(
            trial(),
            run_events(
                RunOutcome::Failed {
                    failure: RunFailure {
                        kind: RunFailureKind::ProviderContextExceeded,
                        message: "the provider rejected the request: context length exceeded"
                            .to_owned(),
                    },
                },
                None,
            ),
            HeadlessOutcome {
                message: Some(
                    "the provider rejected the request: context length exceeded".to_owned(),
                ),
                ..outcome(HeadlessStatus::TaskFailed)
            },
        ),
    );

    // Exit 3 is shared: the status field tells the two apart.
    check(
        "timed_out",
        &stream(
            trial(),
            run_events(
                RunOutcome::BudgetExhausted {
                    exhaustion: Box::new(BudgetExhaustion {
                        limit: BudgetLimitKind::Duration,
                        final_response: false,
                        message: "wall clock limit of 60s reached".to_owned(),
                    }),
                },
                None,
            ),
            HeadlessOutcome {
                message: Some("wall clock limit of 60s reached".to_owned()),
                ..outcome(HeadlessStatus::TimedOut)
            },
        ),
    );
    check(
        "budget_exhausted",
        &stream(
            HeadlessTrial {
                max_turns: Some(4),
                ..trial()
            },
            run_events(
                RunOutcome::BudgetExhausted {
                    exhaustion: Box::new(BudgetExhaustion {
                        limit: BudgetLimitKind::ModelTurns,
                        final_response: true,
                        message: "model turn limit of 4 reached".to_owned(),
                    }),
                },
                None,
            ),
            HeadlessOutcome {
                message: Some("model turn limit of 4 reached".to_owned()),
                ..outcome(HeadlessStatus::BudgetExhausted)
            },
        ),
    );

    // Cancellation: the interrupt lands before any answer.
    check(
        "interrupted",
        &stream(
            trial(),
            vec![
                run_events(RunOutcome::Completed, None).swap_remove(0),
                HeadlessRecord::Event {
                    envelope: Box::new(envelope(
                        2,
                        SessionEvent::RunFinished {
                            session: Box::new(summary(SessionStatus::Idle, false)),
                            run_id: RUN,
                            outcome: RunOutcome::Cancelled,
                            usage: None,
                            context_tokens: None,
                            final_output: None,
                        },
                    )),
                },
            ],
            HeadlessOutcome {
                message: Some("the run was cancelled by an interrupt".to_owned()),
                usage: None,
                estimated_cost_usd_nanos: None,
                prompt_identity: None,
                ..outcome(HeadlessStatus::Interrupted)
            },
        ),
    );

    // Protocol 21: the model asked the user and nobody was there. The
    // question is on the stream; the run is cancelled at that point.
    check(
        "needs_input",
        &stream(
            trial(),
            vec![
                run_events(RunOutcome::Completed, None).swap_remove(0),
                HeadlessRecord::Event {
                    envelope: Box::new(envelope(
                        2,
                        SessionEvent::ToolApprovalRequested {
                            tool_call: ToolCallSnapshot {
                                id: ToolCallId::from_bytes([0x33; 16]),
                                session_id: SESSION,
                                run_id: RUN,
                                turn_ordinal: 1,
                                call_ordinal: 1,
                                provider_call_id: "call_0".to_owned(),
                                name: "ask_user".to_owned(),
                                arguments: r#"{"questions":[{"prompt":"Which crate?","options":["qq-core","qq-tui"]}]}"#
                                    .to_owned(),
                                state: ToolCallState::AwaitingApproval,
                                result: None,
                                is_error: false,
                                display: None,
                            },
                            shell: None,
                            edit: None,
                            question: Some(Box::new(QuestionPreview {
                                questions: vec![Question {
                                    prompt: "Which crate?".to_owned(),
                                    options: vec!["qq-core".to_owned(), "qq-tui".to_owned()],
                                    free_text: false,
                                }],
                            })),
                            fetch: None,
                        },
                    )),
                },
                HeadlessRecord::Event {
                    envelope: Box::new(envelope(
                        3,
                        SessionEvent::RunFinished {
                            session: Box::new(summary(SessionStatus::Idle, false)),
                            run_id: RUN,
                            outcome: RunOutcome::Cancelled,
                            usage: None,
                            context_tokens: None,
                            final_output: None,
                        },
                    )),
                },
            ],
            HeadlessOutcome {
                message: Some("the model asked the user a question and no client could answer: Which crate?".to_owned()),
                usage: None,
                estimated_cost_usd_nanos: None,
                prompt_identity: None,
                ..outcome(HeadlessStatus::NeedsInput)
            },
        ),
    );

    // QQ itself failed after the run was accepted: the stream still closes.
    check(
        "harness_failure",
        &stream(
            trial(),
            vec![run_events(RunOutcome::Completed, None).swap_remove(0)],
            HeadlessOutcome {
                message: Some("the event stream ended without a terminal run event".to_owned()),
                usage: None,
                estimated_cost_usd_nanos: None,
                prompt_identity: None,
                ..outcome(HeadlessStatus::HarnessFailure)
            },
        ),
    );

    // Startup failed before a session existed: the outcome is the only record.
    check(
        "invalid_configuration",
        &[HeadlessRecord::Outcome(Box::new(HeadlessOutcome {
            message: Some(
                "--output-schema ./schema.json: $ref is not supported in an output schema"
                    .to_owned(),
            ),
            usage: None,
            estimated_cost_usd_nanos: None,
            prompt_identity: None,
            ..outcome(HeadlessStatus::InvalidConfiguration)
        }))],
    );

    for path in stream_paths(PROTOCOL_VERSION) {
        let stream = decode_stream(&path);
        if let Some(trial) = assert_well_formed(&path, &stream) {
            assert_eq!(trial.protocol_version, PROTOCOL_VERSION);
        }
    }
}

trait WithStatus {
    fn with_status(self, status: HeadlessStatus) -> Self;
}

impl WithStatus for HeadlessOutcome {
    fn with_status(mut self, status: HeadlessStatus) -> Self {
        self.status = status;
        self.exit_code = status.code();
        self
    }
}

fn stream_paths(version: u16) -> Vec<PathBuf> {
    let directory = fixture_dir(version);
    let mut paths: Vec<PathBuf> = fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("{}: {error}", directory.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "{}: no streams", directory.display());
    paths
}

fn decode_stream(path: &std::path::Path) -> Vec<HeadlessRecord> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|error| {
                panic!(
                    "{}: does not decode as version {PROTOCOL_VERSION}: {error}",
                    path.display()
                )
            })
        })
        .collect()
}

/// A stream recorded by a retained earlier version still decodes and still
/// obeys the framing rules. New fields are additive and optional, so an older
/// stream is a valid current stream with those fields absent.
#[test]
fn historical_streams_still_decode() {
    // Same retention rule as `wire_fixtures.rs`: every `v<N>/` directory
    // present with `N < PROTOCOL_VERSION` must decode, and `N - 1` must exist.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/headless");
    let mut versions: Vec<u16> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            entry
                .file_type()
                .unwrap()
                .is_dir()
                .then_some(entry.file_name())
        })
        .filter_map(|name| name.to_str()?.strip_prefix('v')?.parse::<u16>().ok())
        .filter(|version| *version < PROTOCOL_VERSION)
        .collect();
    versions.sort_unstable();
    assert_eq!(versions.last().copied(), Some(PROTOCOL_VERSION - 1));
    for version in versions {
        for path in stream_paths(version) {
            let stream = decode_stream(&path);
            if let Some(trial) = assert_well_formed(&path, &stream) {
                assert_eq!(trial.protocol_version, version);
            }
        }
    }
}

/// Every exit code the contract publishes, in one place a consumer can diff.
#[test]
fn the_exit_table_matches_the_contract() {
    let table: Vec<(&str, u8)> = HeadlessStatus::ALL
        .iter()
        .map(|status| (status.as_str(), status.code()))
        .collect();
    assert_eq!(
        table,
        [
            ("completed", 0),
            ("task_failed", 1),
            ("invalid_configuration", 2),
            ("timed_out", 3),
            ("budget_exhausted", 3),
            ("harness_failure", 4),
            ("needs_input", 5),
            ("interrupted", 130),
        ]
    );
}
