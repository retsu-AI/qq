//! Deterministic protocol fixtures shared by unit tests and the render
//! benchmark. Every builder returns a fully-populated value with neutral
//! defaults; callers override fields with struct-update syntax.
//!
//! Identifiers are derived from a single byte so tests can name them
//! (`session(2)`, `tool_call(7)`) and assert on them without ceremony.

use qq_protocol::{
    ApprovalMode, Correlation, EventCapabilities, EventCursor, LimitCapabilities, MessageId,
    MessageRole, MessageSnapshot, MessageState, RunId, RunSnapshot, RunStatus, ServerCapabilities,
    SessionEvent, SessionEventEnvelope, SessionId, SessionSnapshot, SessionStatus, SessionSummary,
    SteeringCapabilities, StoreId, ToolCallId, ToolCallSnapshot, ToolCallState, ToolCapabilities,
    WorkspaceId, WorkspaceSnapshot, WorkspaceSummary,
};

pub const WORKSPACE: WorkspaceId = WorkspaceId::from_bytes([1; 16]);
pub const STORE: StoreId = StoreId::from_bytes([3; 16]);
/// The session `workspace_snapshot` focuses.
pub const SESSION: SessionId = SessionId::from_bytes([2; 16]);

#[must_use]
pub const fn session_id(byte: u8) -> SessionId {
    SessionId::from_bytes([byte; 16])
}

#[must_use]
pub const fn run_id(byte: u8) -> RunId {
    RunId::from_bytes([byte; 16])
}

#[must_use]
pub const fn message_id(byte: u8) -> MessageId {
    MessageId::from_bytes([byte; 16])
}

#[must_use]
pub const fn tool_call_id(byte: u8) -> ToolCallId {
    ToolCallId::from_bytes([byte; 16])
}

#[must_use]
pub fn cursor(sequence: u64) -> EventCursor {
    EventCursor {
        store_id: STORE,
        workspace_id: WORKSPACE,
        sequence,
    }
}

/// An idle root session titled `Session` with a test model.
#[must_use]
pub fn session_summary(id: SessionId) -> SessionSummary {
    SessionSummary {
        model_is_fallback: false,
        id,
        workspace_id: WORKSPACE,
        parent_id: None,
        spawned_by: None,
        purpose: qq_protocol::SessionPurpose::Task,
        title: "Session".to_owned(),
        status: SessionStatus::Idle,
        active_run_id: None,
        activity: None,
        queued_prompts: 0,
        model: Some("openai/gpt-test".to_owned()),
        profile: qq_protocol::AgentProfileId::default(),
        approval_mode: ApprovalMode::Auto,
        reasoning_effort: None,
        correlation: Correlation::default(),
        context_tokens: None,
        accounting: None,
        estimated_cost_usd_nanos: Some(0),
        updated_at_ms: 1,
        last_outcome: None,
    }
}

/// A complete assistant message in turn 1 of `run_id(2)`.
#[must_use]
pub fn message(id: MessageId, session_id: SessionId, output: &str) -> MessageSnapshot {
    MessageSnapshot {
        id,
        session_id,
        run_id: run_id(2),
        turn_ordinal: 1,
        role: MessageRole::Assistant,
        state: MessageState::Complete,
        steering: false,
        truncated: false,
        output: output.to_owned(),
        refusal: String::new(),
        created_at_ms: 1,
    }
}

/// A completed tool call in turn 1 with an empty JSON argument object.
#[must_use]
pub fn tool_call(id: ToolCallId, session_id: SessionId, name: &str) -> ToolCallSnapshot {
    ToolCallSnapshot {
        id,
        session_id,
        run_id: run_id(2),
        turn_ordinal: 1,
        call_ordinal: u16::from(id.as_bytes()[0]),
        provider_call_id: format!("call-{}", id.as_bytes()[0]),
        name: name.to_owned(),
        arguments: "{}".to_owned(),
        state: ToolCallState::Completed,
        result: None,
        is_error: false,
        display: None,
    }
}

#[must_use]
pub fn run(id: RunId, session_id: SessionId, status: RunStatus) -> RunSnapshot {
    RunSnapshot {
        id,
        session_id,
        status,
        outcome: None,
        prompt_identity: None,
        resolved_model: None,
        plan: None,
        correlation: Correlation::default(),
        usage: None,
        context_tokens: None,
        estimated_cost_usd_nanos: None,
        limits: None,
        audit: None,
        final_output: None,
    }
}

/// A body for `summary` with no messages, runs, or tool calls.
#[must_use]
pub fn session_snapshot(summary: SessionSummary) -> SessionSnapshot {
    SessionSnapshot {
        summary,
        messages: Vec::new(),
        runs: Vec::new(),
        tool_calls: Vec::new(),
        has_older_tool_calls: false,
        has_older_messages: false,
    }
}

/// A one-session workspace at sequence 1 with `SESSION` focused and empty.
#[must_use]
pub fn workspace_snapshot() -> WorkspaceSnapshot {
    let summary = session_summary(SESSION);
    WorkspaceSnapshot {
        included: Vec::new(),
        cursor: cursor(1),
        workspace: WorkspaceSummary {
            id: WORKSPACE,
            path: "/workspace".to_owned(),
        },
        sessions: vec![summary.clone()],
        focused: Some(session_snapshot(summary)),
        has_older_sessions: false,
    }
}

#[must_use]
pub fn envelope(sequence: u64, session_id: SessionId, event: SessionEvent) -> SessionEventEnvelope {
    SessionEventEnvelope {
        cursor: cursor(sequence),
        session_id,
        run_id: None,
        caused_by: None,
        occurred_at_ms: 1,
        event,
    }
}

/// A capability document with steering and every approval mode enabled and
/// no profiles beyond `default`; tests that need profiles set `profiles`.
#[must_use]
pub fn capabilities(steering: SteeringCapabilities) -> ServerCapabilities {
    ServerCapabilities {
        version: qq_protocol::CAPABILITIES_VERSION,
        protocol_version: qq_protocol::PROTOCOL_VERSION,
        server_version: "test".to_owned(),
        input_parts: qq_protocol::InputPartKind::ALL.to_vec(),
        commands: qq_protocol::SessionCommandKind::ALL.to_vec(),
        steering,
        limits: LimitCapabilities {
            supported: qq_protocol::BudgetLimitKind::ALL.to_vec(),
            max_request_bytes: 1 << 20,
            max_event_bytes: 1 << 20,
            max_input_parts: 16,
            max_input_text_bytes: 64 * 1024,
            max_input_file_parts: 8,
            max_input_file_bytes: 1 << 20,
            max_pending_prompts: 8,
            max_children: 8,
            max_concurrent_children: 4,
            max_child_depth: 2,
            max_output_continuations: 3,
            max_descendants: 24,
            max_output_schema_bytes: 65_536,
            max_output_schema_depth: 32,
            max_output_schema_values: 4_096,
            max_output_repair_turns: 8,
            max_correlation_entries: 8,
        },
        approvals: [
            "approve_once",
            "approve_for_session",
            "approve_for_workspace",
            "deny",
            "answer",
        ]
        .map(str::to_owned)
        .to_vec(),
        approval_modes: vec![
            ApprovalMode::ReadOnly,
            ApprovalMode::Ask,
            ApprovalMode::Auto,
            ApprovalMode::Full,
        ],
        profiles: Some(Vec::new()),
        tools: ToolCapabilities {
            max_catalog_tools: 128,
            max_tool_schema_bytes: 32 * 1024,
            max_catalog_schema_bytes: 256 * 1024,
            full_exposure_tools: 32,
            full_exposure_schema_bytes: 64 * 1024,
            max_pinned_tools: 16,
            max_indexed_skills: 64,
            external_prefixes: vec!["mcp__".to_owned(), "embedded__".to_owned()],
        },
        workspace_tools: None,
        events: EventCapabilities {
            post_commit: true,
            replay_page: 256,
            max_subscriptions: 64,
            max_event_bytes: 1 << 20,
            retention_bounded: false,
        },
        delegation: None,
    }
}

/// Steering fully supported: the document most tests want.
#[must_use]
pub fn steering_capabilities() -> ServerCapabilities {
    capabilities(SteeringCapabilities {
        boundary: true,
        interrupt: true,
        max_pending_per_run: 4,
    })
}
