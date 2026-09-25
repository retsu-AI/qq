//! Store and reducer tests that hold on every surface. They run under
//! `cargo test` natively and under `wasm-bindgen-test` on `wasm32`.

use std::sync::Arc;

use qq_protocol::{
    EventCursor, MessageId, MessageRole, MessageSnapshot, MessageState, RunId, RunOutcome,
    SessionEvent, SessionEventEnvelope, SessionId, SessionStatus, SessionSummary, StoreId,
    ToolCallId, ToolCallSnapshot, ToolCallState, WorkspaceId,
};
use serde_json::{Value, json};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen_test::wasm_bindgen_test as test;

use super::*;

/// Every `event_*` wire fixture, in cursor order. They describe one session
/// (`[3; 16]`) and one run (`[4; 16]`); the list is the replay input for the
/// golden below, so adding a fixture means regenerating the golden.
///
/// Pinned to the `v17/` directory on purpose: the golden checks the reducer's
/// projection of a *fixed* event stream, so it must not move when a newer
/// protocol version adds events or fields. `v17` is the oldest retained
/// wire-fixture version; if that directory is ever dropped, repoint this list
/// at the new oldest and regenerate `state_replay_v17.json` under a new name.
const EVENT_FIXTURES: [&str; 12] = [
    include_str!("../../../qq-protocol/tests/fixtures/v17/event_prompt_queued.json"),
    include_str!("../../../qq-protocol/tests/fixtures/v17/event_run_started.json"),
    include_str!("../../../qq-protocol/tests/fixtures/v17/event_steering_queued.json"),
    include_str!("../../../qq-protocol/tests/fixtures/v17/event_steering_applied.json"),
    include_str!("../../../qq-protocol/tests/fixtures/v17/event_steering_superseded.json"),
    include_str!("../../../qq-protocol/tests/fixtures/v17/event_run_interrupted.json"),
    include_str!("../../../qq-protocol/tests/fixtures/v17/event_run_finished_failed.json"),
    include_str!("../../../qq-protocol/tests/fixtures/v17/event_run_output_truncated.json"),
    include_str!(
        "../../../qq-protocol/tests/fixtures/v17/event_run_finished_output_truncated.json"
    ),
    include_str!(
        "../../../qq-protocol/tests/fixtures/v17/event_assistant_message_started_truncated.json"
    ),
    include_str!("../../../qq-protocol/tests/fixtures/v17/event_run_audit_started.json"),
    include_str!("../../../qq-protocol/tests/fixtures/v17/event_run_audit_completed.json"),
];

const REPLAY_GOLDEN: &str = include_str!("../../tests/fixtures/state_replay_v17.json");

fn context(models: &[ModelOption]) -> ReduceContext<'_> {
    ReduceContext {
        focused: None,
        attentive: false,
        workspace_id: Some(WorkspaceId::from_bytes([2; 16])),
        capabilities: None,
        models,
        caused_by_me: false,
    }
}

fn summary(id: SessionId) -> SessionSummary {
    SessionSummary {
        model_is_fallback: false,
        id,
        workspace_id: WorkspaceId::from_bytes([2; 16]),
        parent_id: None,
        spawned_by: None,
        purpose: qq_protocol::SessionPurpose::default(),
        title: format!("session {}", id.to_string().get(..4).unwrap_or_default()),
        status: SessionStatus::Idle,
        active_run_id: None,
        activity: None,
        queued_prompts: 0,
        model: Some("openai/gpt-test".to_owned()),
        profile: qq_protocol::AgentProfileId::default(),
        reasoning_effort: None,
        approval_mode: qq_protocol::ApprovalMode::default(),
        approval_delegate: None,
        jev_mode: None,
        correlation: qq_protocol::Correlation::default(),
        last_outcome: None,
        context_tokens: None,
        estimated_cost_usd_nanos: None,
        accounting: None,
        updated_at_ms: 1,
    }
}

fn envelope(sequence: u64, session_id: SessionId, event: SessionEvent) -> SessionEventEnvelope {
    SessionEventEnvelope {
        cursor: EventCursor {
            store_id: StoreId::from_bytes([1; 16]),
            workspace_id: WorkspaceId::from_bytes([2; 16]),
            sequence,
        },
        session_id,
        run_id: None,
        caused_by: None,
        occurred_at_ms: sequence,
        event,
    }
}

fn message(id: u8, session_id: SessionId, run_id: RunId, output: &str) -> MessageSnapshot {
    MessageSnapshot {
        id: MessageId::from_bytes([id; 16]),
        session_id,
        run_id,
        turn_ordinal: 0,
        role: MessageRole::Assistant,
        state: MessageState::Streaming,
        steering: false,
        truncated: false,
        output: output.to_owned(),
        refusal: String::new(),
        created_at_ms: 0,
    }
}

/// A deterministic view of everything the reducer owns, so a golden can
/// catch any change in what an event does to the store.
fn project(store: &SessionStore, effects: &[(u64, StateEffects)]) -> Value {
    let mut sessions: Vec<&SessionView> = store.values().collect();
    sessions.sort_by_key(|view| view.summary.id);
    let sessions: Vec<Value> = sessions
        .iter()
        .map(|view| {
            let mut runs: Vec<(&RunId, &RunStats)> = view.runs.iter().collect();
            runs.sort_by_key(|(id, _)| **id);
            let mut reasoning: Vec<(&RunId, &Reasoning)> = view.reasoning.iter().collect();
            reasoning.sort_by_key(|(id, _)| **id);
            json!({
                "id": view.summary.id.to_string(),
                "title": view.summary.title,
                "status": view.summary.status,
                "active_run_id": view.summary.active_run_id,
                "queued_prompts": view.summary.queued_prompts,
                "context_tokens": view.summary.context_tokens,
                "context_window": view.context_window,
                "last_outcome": view.summary.last_outcome,
                "activity": view.activity,
                "live": {
                    "tail": view.live.tail,
                    "active_tool": view.live.active_tool,
                    "awaiting_approval": view.live.awaiting_approval,
                },
                "messages": view.messages.as_ref().map(|messages| messages.iter().map(|m| json!({
                    "id": m.id.to_string(),
                    "run_id": m.run_id.to_string(),
                    "turn": m.turn_ordinal,
                    "role": m.role,
                    "state": m.state,
                    "output": m.output,
                    "refusal": m.refusal,
                    "truncated": m.truncated,
                })).collect::<Vec<_>>()),
                "tool_calls": view.tool_calls.as_ref().map(|calls| calls.iter().map(|c| json!({
                    "id": c.id.to_string(),
                    "name": c.name,
                    "state": c.state,
                })).collect::<Vec<_>>()),
                "runs": runs.iter().map(|(id, stats)| json!({
                    "id": id.to_string(),
                    "started_at_ms": stats.started_at_ms,
                    "first_token_at_ms": stats.first_token_at_ms,
                    "finished_at_ms": stats.finished_at_ms,
                    "outcome": stats.outcome,
                    "usage": stats.usage,
                    "tool_calls": stats.tool_calls,
                    "cost_usd_nanos": stats.cost_usd_nanos,
                    "turns": stats.turns,
                    "plan": stats.plan.as_ref().map(|(profile, digest)| json!([profile.as_str(), digest])),
                    "resolved_route": stats.resolved_route,
                    "live_cost_usd_nanos": stats.live_cost_usd_nanos,
                })).collect::<Vec<_>>(),
                "reasoning": reasoning.iter().map(|(id, r)| json!({
                    "run_id": id.to_string(),
                    "text": r.text,
                    "streaming": r.streaming,
                })).collect::<Vec<_>>(),
                "prompt_history": view.prompt_history,
                "unread": view.unread,
                "finished_unread": view.finished_unread,
                "need": view.need().map(|need| format!("{need:?}")),
                "group": format!("{:?}", view.group()),
            })
        })
        .collect();
    let effects: Vec<Value> = effects
        .iter()
        .map(|(sequence, effects)| {
            json!({
                "sequence": sequence,
                "effects": effects.iter().map(|effect| format!("{effect:?}")).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "thread_order": store.thread_order().iter().map(ToString::to_string).collect::<Vec<_>>(),
        "sessions": sessions,
        "effects": effects,
    })
}

#[test]
fn replaying_the_wire_fixtures_matches_the_golden_projection() {
    let mut envelopes: Vec<SessionEventEnvelope> = EVENT_FIXTURES
        .iter()
        .map(|text| serde_json::from_str(text).expect("fixture decodes"))
        .collect();
    envelopes.sort_by_key(|envelope| envelope.cursor.sequence);
    let models = [ModelOption {
        provider: "openai".to_owned(),
        model: "gpt-5.6".to_owned(),
        name: None,
        context_window: Some(400_000),
        reasoning_efforts: Vec::new(),
        selection: qq_protocol::ModelSelection {
            model_is_fallback: false,
            model: Some("openai/gpt-5.6".to_owned()),
            ..qq_protocol::ModelSelection::default()
        },
    }];
    let mut store = SessionStore::default();
    // The fixtures assume the session exists with a warm body, as after the
    // initial snapshot; the first event carries its summary.
    let SessionEvent::PromptQueued { session, .. } = &envelopes[0].event else {
        panic!("the first fixture in cursor order is the queued prompt");
    };
    store.upsert_summary((**session).clone(), &models, 0);
    store.warm_empty(session.id);

    let mut effects = Vec::new();
    for envelope in &envelopes {
        let reduced = store.reduce_event(envelope, context(&models));
        effects.push((envelope.cursor.sequence, reduced));
    }
    let projection = project(&store, &effects);
    let encoded = format!(
        "{}\n",
        serde_json::to_string_pretty(&projection).expect("projection encodes")
    );

    #[cfg(not(target_arch = "wasm32"))]
    if std::env::var_os("QQ_UPDATE_FIXTURES").is_some() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/state_replay_v17.json"
        );
        std::fs::write(path, &encoded).expect("golden written");
        return;
    }
    assert_eq!(
        encoded, REPLAY_GOLDEN,
        "reducer projection changed; rerun with QQ_UPDATE_FIXTURES=1 if intended"
    );
}

#[test]
fn the_live_tail_is_sanitized_by_the_surface_supplied_function() {
    fn shouty(character: char) -> Option<char> {
        (character != 'x').then(|| character.to_ascii_uppercase())
    }
    let session_id = SessionId::from_bytes([3; 16]);
    let run_id = RunId::from_bytes([4; 16]);
    let mut store = SessionStore::with_sanitizer(shouty);
    store.upsert_summary(summary(session_id), &[], 0);

    // Cold sessions still track the tail.
    store.reduce_event(
        &envelope(
            1,
            session_id,
            SessionEvent::AssistantMessageStarted {
                message: message(9, session_id, run_id, "ab\tx cd"),
            },
        ),
        context(&[]),
    );
    assert_eq!(store[&session_id].live.tail, "AB CD");
    assert!(!store[&session_id].is_warm());

    // The default drops controls and bidi overrides, keeps everything else.
    let mut plain = SessionStore::default();
    plain.upsert_summary(summary(session_id), &[], 0);
    plain.reduce_event(
        &envelope(
            1,
            session_id,
            SessionEvent::AssistantMessageStarted {
                message: message(9, session_id, run_id, "a\u{202e}b\u{7}c  d"),
            },
        ),
        context(&[]),
    );
    assert_eq!(plain[&session_id].live.tail, "abc d");
}

#[test]
fn a_reviewer_resolution_records_who_settled_the_call_and_evicts_with_it() {
    // DA6: the row can say "approved by jev" because the reducer keeps the
    // settlement beside the call's timing; a human once-approval says nothing;
    // the settlement leaves with the body like every other per-call datum.
    let session_id = SessionId::from_bytes([3; 16]);
    let run_id = RunId::from_bytes([4; 16]);
    let mut store = SessionStore::default();
    store.upsert_summary(summary(session_id), &[], 0);
    store.warm_empty(session_id);
    let call = |id: u8, state: ToolCallState| ToolCallSnapshot {
        id: ToolCallId::from_bytes([id; 16]),
        session_id,
        run_id,
        turn_ordinal: 0,
        call_ordinal: u16::from(id),
        provider_call_id: format!("call_{id}"),
        name: "shell".to_owned(),
        arguments: String::new(),
        state,
        result: None,
        is_error: false,
        display: None,
    };
    store.reduce_event(
        &envelope(
            1,
            session_id,
            SessionEvent::ToolApprovalResolved {
                tool_call: call(7, ToolCallState::Requested),
                resolution: qq_protocol::ApprovalResolution::ApprovedByReviewer,
                delegate: Some(qq_protocol::DelegateIdentity::Jev),
            },
        ),
        context(&[]),
    );
    store.reduce_event(
        &envelope(
            2,
            session_id,
            SessionEvent::ToolApprovalResolved {
                tool_call: call(8, ToolCallState::Requested),
                resolution: qq_protocol::ApprovalResolution::ApprovedOnce,
                delegate: None,
            },
        ),
        context(&[]),
    );
    let timing = &store[&session_id].tool_timing;
    let by_jev = timing[&ToolCallId::from_bytes([7; 16])].settled.unwrap();
    assert_eq!(by_jev.label().as_deref(), Some("approved by jev"));
    assert_eq!(by_jev.delegate, Some(qq_protocol::DelegateIdentity::Jev));
    let by_human = timing[&ToolCallId::from_bytes([8; 16])].settled.unwrap();
    assert_eq!(by_human.label(), None, "a once-approval needs no remark");
    // A reviewer resolution from a pre-28 server (no `delegate`) still reads
    // as the reviewer model's.
    let legacy = ApprovalSettlement {
        resolution: qq_protocol::ApprovalResolution::DeniedByReviewer,
        delegate: None,
    };
    assert_eq!(legacy.label().as_deref(), Some("denied by reviewer"));

    store.body_mut(&session_id).unwrap().evict_body();
    assert!(store[&session_id].tool_timing.is_empty());
}

#[test]
fn an_escalation_lands_on_the_pending_holds_preview_and_nowhere_else() {
    // DA6: the prompt says why a delegate passed. The reason is kept beside
    // the hold's preview only while the call is awaiting approval; an
    // escalation for a call that is not pending (a client already answered)
    // is ignored, and the preview leaves with the hold.
    let session_id = SessionId::from_bytes([3; 16]);
    let run_id = RunId::from_bytes([4; 16]);
    let mut store = SessionStore::default();
    store.upsert_summary(summary(session_id), &[], 0);
    store.warm_empty(session_id);
    let call = |state: ToolCallState| ToolCallSnapshot {
        id: ToolCallId::from_bytes([7; 16]),
        session_id,
        run_id,
        turn_ordinal: 0,
        call_ordinal: 0,
        provider_call_id: "call_0".to_owned(),
        name: "shell".to_owned(),
        arguments: String::new(),
        state,
        result: None,
        is_error: false,
        display: None,
    };
    store.reduce_event(
        &envelope(
            1,
            session_id,
            SessionEvent::ToolApprovalRequested {
                tool_call: call(ToolCallState::AwaitingApproval),
                shell: Some(Box::new(qq_protocol::ShellCommandPreview {
                    command: "git push".to_owned(),
                    cwd: None,
                    verdict: None,
                    reasons: Vec::new(),
                })),
                edit: None,
                question: None,
                fetch: None,
            },
        ),
        context(&[]),
    );
    store.reduce_event(
        &envelope(
            2,
            session_id,
            SessionEvent::ToolApprovalEscalated {
                tool_call_id: ToolCallId::from_bytes([7; 16]),
                delegate: Some(qq_protocol::DelegateIdentity::Jev),
                reason: "unsure".to_owned(),
            },
        ),
        context(&[]),
    );
    let preview = &store[&session_id].approval_previews[&ToolCallId::from_bytes([7; 16])];
    assert_eq!(
        preview.escalated,
        Some(ApprovalEscalation {
            delegate: Some(qq_protocol::DelegateIdentity::Jev),
            reason: "unsure".to_owned(),
        })
    );
    assert_eq!(
        preview.shell.as_ref().map(|shell| shell.command.as_str()),
        Some("git push"),
        "the escalation joins the preview rather than replacing it"
    );

    // The human answers; the hold and its preview are gone, and a late
    // escalation (the delegate's clock firing after the race was lost)
    // creates nothing.
    store.reduce_event(
        &envelope(
            3,
            session_id,
            SessionEvent::ToolApprovalResolved {
                tool_call: call(ToolCallState::Requested),
                resolution: qq_protocol::ApprovalResolution::ApprovedOnce,
                delegate: None,
            },
        ),
        context(&[]),
    );
    store.reduce_event(
        &envelope(
            4,
            session_id,
            SessionEvent::ToolApprovalEscalated {
                tool_call_id: ToolCallId::from_bytes([7; 16]),
                delegate: None,
                reason: "late".to_owned(),
            },
        ),
        context(&[]),
    );
    assert!(store[&session_id].approval_previews.is_empty());
}

#[test]
fn a_finished_idle_run_hands_the_oldest_draft_back_to_the_surface() {
    let session_id = SessionId::from_bytes([3; 16]);
    let run_id = RunId::from_bytes([4; 16]);
    let mut store = SessionStore::default();
    store.upsert_summary(summary(session_id), &[], 0);
    store.warm_empty(session_id);
    let view = store.get_mut(&session_id).unwrap();
    view.drafts.push_back("first".to_owned());
    view.drafts.push_back("second".to_owned());

    let mut idle = summary(session_id);
    idle.status = SessionStatus::Idle;
    let effects = store.reduce_event(
        &envelope(
            5,
            session_id,
            SessionEvent::RunFinished {
                session: Box::new(idle.clone()),
                run_id,
                outcome: RunOutcome::Completed,
                usage: None,
                context_tokens: None,
                final_output: None,
            },
        ),
        context(&[]),
    );
    assert!(effects.contains(&StateEffect::SubmitDraft {
        session_id,
        text: "first".to_owned(),
    }));
    assert_eq!(store[&session_id].drafts, ["second"]);
    // Unfocused: the finish is unread and the surface is asked for attention.
    assert_eq!(store[&session_id].unread, 1);
    assert!(store[&session_id].finished_unread);
    assert!(effects.iter().any(|effect| matches!(
        effect,
        StateEffect::Attention(Attention::RunFinished { .. })
    )));

    // A run that leaves more queued keeps the drafts waiting; a focused,
    // attentive surface gets neither unread nor attention.
    idle.queued_prompts = 1;
    let effects = store.reduce_event(
        &envelope(
            6,
            session_id,
            SessionEvent::RunFinished {
                session: Box::new(idle),
                run_id,
                outcome: RunOutcome::Completed,
                usage: None,
                context_tokens: None,
                final_output: None,
            },
        ),
        ReduceContext {
            focused: Some(session_id),
            attentive: true,
            ..context(&[])
        },
    );
    assert!(effects.is_empty());
    assert_eq!(store[&session_id].drafts, ["second"]);
    assert_eq!(store[&session_id].unread, 1);
}

#[test]
fn deleting_the_shown_session_refocuses_its_neighbour_and_fetches_a_cold_body() {
    let a = SessionId::from_bytes([0xa; 16]);
    let b = SessionId::from_bytes([0xb; 16]);
    let child = SessionId::from_bytes([0xc; 16]);
    let mut store = SessionStore::default();
    let mut older = summary(a);
    older.updated_at_ms = 1;
    let mut newer = summary(b);
    newer.updated_at_ms = 2;
    let mut kid = summary(child);
    kid.parent_id = Some(b);
    store.upsert_summary(older, &[], 0);
    store.upsert_summary(newer, &[], 0);
    store.upsert_summary(kid, &[], 0);
    store.warm_empty(b);
    let call = ToolCallSnapshot {
        id: ToolCallId::from_bytes([7; 16]),
        session_id: b,
        run_id: RunId::from_bytes([4; 16]),
        turn_ordinal: 0,
        call_ordinal: 0,
        provider_call_id: "call_0".to_owned(),
        name: "shell".to_owned(),
        arguments: String::new(),
        state: ToolCallState::Running,
        result: None,
        is_error: false,
        display: None,
    };
    store.upsert_tool_call(call);
    assert_eq!(store.thread_order(), [b, child, a]);

    let effects = store.reduce_event(
        &envelope(9, b, SessionEvent::SessionDeleted { session_id: b }),
        ReduceContext {
            focused: Some(b),
            ..context(&[])
        },
    );

    assert!(!store.contains_key(&b));
    // The child is detached to a root, as the server does.
    assert_eq!(store[&child].summary.parent_id, None);
    assert_eq!(
        effects,
        vec![
            StateEffect::SessionRemoved {
                session_id: b,
                tool_call_ids: vec![ToolCallId::from_bytes([7; 16])],
            },
            StateEffect::Refocus(Some(child)),
            StateEffect::RequestSnapshot(body_request(WorkspaceId::from_bytes([2; 16]), child)),
        ]
    );

    // Deleting an unshown session neither refocuses nor fetches.
    let effects = store.reduce_event(
        &envelope(10, a, SessionEvent::SessionDeleted { session_id: a }),
        ReduceContext {
            focused: Some(child),
            ..context(&[])
        },
    );
    assert_eq!(
        effects,
        vec![StateEffect::SessionRemoved {
            session_id: a,
            tool_call_ids: Vec::new(),
        }]
    );
    // Deleting the last shown session clears focus.
    let effects = store.reduce_event(
        &envelope(
            11,
            child,
            SessionEvent::SessionDeleted { session_id: child },
        ),
        ReduceContext {
            focused: Some(child),
            ..context(&[])
        },
    );
    assert!(effects.contains(&StateEffect::Refocus(None)));
    assert!(store.is_empty());
}

#[test]
fn warm_bodies_are_bounded_and_the_pinned_session_never_evicts() {
    let mut store = SessionStore::default();
    let ids: Vec<SessionId> = (0..=WARM_BODY_LIMIT as u8 + 1)
        .map(|byte| SessionId::from_bytes([byte + 0x10; 16]))
        .collect();
    for (clock, id) in ids.iter().enumerate() {
        store.upsert_summary(summary(*id), &[], 0);
        store.warm_empty(*id);
        store.mark_focused(*id, clock as u64 + 1);
    }
    // The oldest-focused session is pinned: it stays warm and one more of
    // the others goes cold to make room.
    store.evict_cold_bodies(Some(ids[0]));
    let warm: Vec<SessionId> = ids
        .iter()
        .copied()
        .filter(|id| store[id].is_warm())
        .collect();
    assert_eq!(warm.len(), WARM_BODY_LIMIT);
    assert!(store[&ids[0]].is_warm());
    assert!(!store[&ids[1]].is_warm());
    assert!(!store[&ids[2]].is_warm());
    assert!(store[&ids[3]].is_warm());
}

#[test]
fn the_output_truncation_notice_names_the_cap_when_capabilities_are_known() {
    let session_id = SessionId::from_bytes([3; 16]);
    let run_id = RunId::from_bytes([4; 16]);
    let mut store = SessionStore::default();
    store.upsert_summary(summary(session_id), &[], 0);
    let event = envelope(
        2,
        session_id,
        SessionEvent::RunOutputTruncated {
            run_id,
            turn_ordinal: 1,
            continuation: 2,
        },
    );
    let without = store.reduce_event(&event, context(&[]));
    assert_eq!(
        without,
        vec![StateEffect::Notice {
            session: Some(session_id),
            level: NoticeLevel::Info,
            text: "output limit reached; continuing (2)".to_owned(),
        }]
    );
    let mut capabilities: qq_protocol::ServerCapabilities = serde_json::from_str(include_str!(
        "../../../qq-protocol/tests/fixtures/v17/capabilities.json"
    ))
    .expect("capabilities fixture decodes");
    capabilities.limits.max_output_continuations = 4;
    let capabilities = Arc::new(capabilities);
    let with = store.reduce_event(
        &event,
        ReduceContext {
            capabilities: Some(&capabilities),
            ..context(&[])
        },
    );
    assert!(matches!(
        with.as_slice(),
        [StateEffect::Notice { text, .. }] if text == "output limit reached; continuing (2/4)"
    ));
}

/// Streaming deltas are body-only mutations: the derived tree index (sidebar
/// order) must survive them, or every frame during a run rebuilds it.
#[test]
fn streaming_deltas_keep_the_tree_index() {
    let models: Vec<ModelOption> = Vec::new();
    let mut store = SessionStore::default();
    let parent = SessionId::from_bytes([1; 16]);
    let child = SessionId::from_bytes([2; 16]);
    let run_id = RunId::from_bytes([3; 16]);
    store.upsert_summary(summary(parent), &models, 0);
    let mut child_summary = summary(child);
    child_summary.parent_id = Some(parent);
    store.upsert_summary(child_summary, &models, 0);
    store.warm_empty(child);
    let order_before: *const SessionId = store.thread_order().as_ptr();
    assert_eq!(store.thread_order(), &[parent, child]);

    let started = message(9, child, run_id, "");
    store.reduce_event(
        &envelope(
            1,
            child,
            SessionEvent::AssistantMessageStarted {
                message: started.clone(),
            },
        ),
        context(&models),
    );
    store.reduce_event(
        &envelope(
            2,
            child,
            SessionEvent::TextAppended {
                message_id: started.id,
                channel: qq_protocol::TextChannel::Output,
                text: "hello".to_owned(),
            },
        ),
        context(&models),
    );
    store.reduce_event(
        &envelope(
            3,
            child,
            SessionEvent::RunActivityChanged {
                run_id,
                activity: qq_protocol::RunActivity::WaitingForProvider,
            },
        ),
        context(&models),
    );
    // Same allocation: the index was never dropped. (A pointer comparison
    // can pass by luck when the allocator reuses the freed block, so the
    // rebuild counter below is the authoritative check.)
    assert_eq!(store.thread_order().as_ptr(), order_before);
    assert_eq!(
        store.index_rebuilds(),
        1,
        "streaming deltas must not invalidate the tree index"
    );
    assert_eq!(store.thread_order(), &[parent, child]);
    assert_eq!(
        store.get(&child).unwrap().messages.as_ref().unwrap()[0].output,
        "hello"
    );

    // A summary change still rebuilds it.
    let mut moved = summary(child);
    moved.updated_at_ms = 5;
    store.reduce_event(
        &envelope(
            4,
            child,
            SessionEvent::SessionUpdated {
                session: Box::new(moved),
            },
        ),
        context(&models),
    );
    assert_eq!(store.thread_order(), &[child, parent]);
}

/// `set_jev_mode` is observed like every other session field: the durable
/// `session_updated` carries the whole summary, so every surface on the
/// reducer sees the same mode without a dedicated event.
#[test]
fn session_updated_carries_the_jev_mode_to_every_surface() {
    let session_id = SessionId::from_bytes([7; 16]);
    let mut store = SessionStore::default();
    store.upsert_summary(summary(session_id), &[], 0);
    assert_eq!(store.get(&session_id).unwrap().summary.jev_mode, None);

    let mut switched = summary(session_id);
    switched.jev_mode = Some(qq_protocol::JevMode::Ultrajev);
    store.reduce_event(
        &envelope(
            1,
            session_id,
            SessionEvent::SessionUpdated {
                session: Box::new(switched),
            },
        ),
        context(&[]),
    );
    assert_eq!(
        store.get(&session_id).unwrap().summary.jev_mode,
        Some(qq_protocol::JevMode::Ultrajev)
    );

    store.reduce_event(
        &envelope(
            2,
            session_id,
            SessionEvent::SessionUpdated {
                session: Box::new(summary(session_id)),
            },
        ),
        context(&[]),
    );
    assert_eq!(store.get(&session_id).unwrap().summary.jev_mode, None);
}
