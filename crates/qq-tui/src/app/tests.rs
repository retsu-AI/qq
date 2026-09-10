use crossterm::event::MouseEvent;
use qq_client::state::{
    LIVE_TAIL_BYTES, MAX_LIVE_TOOL_OUTPUT_BYTES, SNAPSHOT_MESSAGE_LIMIT, WARM_BODY_LIMIT,
};
use qq_protocol::{
    MessageId, MessageRole, MessageSnapshot, MessageState, ModelDescriptor, RunActivity, RunId,
    RunOutcome, RunSnapshot, RunStatus, SessionEvent, SessionSnapshot, SessionStatus,
    SessionSummary, SnapshotRequest, TextChannel, TokenUsage, ToolCallId, ToolCallState,
    WorkspaceGrantOutcome,
};

use super::*;
use crate::{
    KeyChord,
    effect::{Effect, Effects},
    fixtures,
    input::SessionConfirm,
    viewport::View,
};

fn id<T>(byte: u8, constructor: impl FnOnce([u8; 16]) -> T) -> T {
    constructor([byte; 16])
}

fn snapshot() -> WorkspaceSnapshot {
    fixtures::workspace_snapshot()
}

#[test]
fn shift_enter_inserts_a_newline_without_submitting() {
    let mut app = App::new(TuiOptions::default());
    app.composer.text = "hello".to_owned();
    let (changed, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))
        .split();
    assert!(changed);
    assert!(requests.is_empty());
    assert_eq!(app.composer.text, "hello\n");
}

#[test]
fn alt_enter_and_ctrl_j_insert_newlines_without_submitting() {
    let mut app = App::new(TuiOptions::default());
    app.composer.text = "hello".to_owned();

    let (changed, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT))
        .split();
    assert!(changed);
    assert!(requests.is_empty());
    assert_eq!(app.composer.text, "hello\n");

    let (changed, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .split();
    assert!(changed);
    assert!(requests.is_empty());
    assert_eq!(app.composer.text, "hello\n\n");
}

#[test]
fn paste_preserves_newlines_in_the_composer() {
    let mut app = App::new(TuiOptions::default());
    let (changed, requests) = app
        .handle_terminal_event(Event::Paste("alpha\r\nbeta".to_owned()))
        .split();
    assert!(changed);
    assert!(requests.is_empty());
    assert_eq!(app.composer.text, "alpha\nbeta");

    // Three or more lines collapse to a placeholder; the submitted prompt
    // carries the real content with CRLF normalized.
    app.composer.clear();
    app.handle_terminal_event(Event::Paste("alpha\r\nbeta\ngamma".to_owned()));
    assert_eq!(app.composer.text, "[Pasted #1 3 lines]");
    assert_eq!(app.composer.expanded(), "alpha\nbeta\ngamma");
}

#[test]
fn submit_is_optimistic_but_restores_a_rejected_prompt() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    app.composer.text = "hello".to_owned();

    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();
    let ClientRequest::Command(request) = requests.into_iter().next().unwrap() else {
        panic!("expected command")
    };
    assert!(app.composer.text.is_empty());
    app.apply_client_update(ClientUpdate::CommandResult {
        command_id: request.command_id,
        result: Err(ClientFailure::new("offline")),
    });

    assert_eq!(app.composer.text, "hello");
    assert_eq!(app.status.as_deref(), Some("offline"));
}

#[test]
fn approval_prompt_captures_keys_and_sends_the_decision() {
    let mut app = App::new(TuiOptions::default());
    let initial = snapshot();
    let session_id = initial.focused.as_ref().unwrap().summary.id;
    app.apply_snapshot(initial);
    let tool_call = ToolCallSnapshot {
        run_id: id(4, RunId::from_bytes),
        call_ordinal: 1,
        provider_call_id: "call_0".to_owned(),
        arguments: r#"{"path":"note.txt"}"#.to_owned(),
        state: ToolCallState::AwaitingApproval,
        ..fixtures::tool_call(id(7, ToolCallId::from_bytes), session_id, "write_file")
    };
    app.sessions.upsert_tool_call(tool_call.clone());
    assert_eq!(
        app.pending_approval().map(|call| call.id),
        Some(tool_call.id)
    );

    // The prompt captures ordinary typing instead of the composer.
    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
        .split();
    assert!(requests.is_empty());
    assert!(app.composer.text.is_empty());

    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
        .split();
    let ClientRequest::Command(request) = requests.into_iter().next().unwrap() else {
        panic!("expected a command")
    };
    assert_eq!(
        request.command,
        SessionCommand::RespondToolApproval {
            run_id: tool_call.run_id,
            tool_call_id: tool_call.id,
            decision: ApprovalDecision::ApproveOnce,
        }
    );
    // Answered approvals stop prompting until the server responds.
    assert!(app.pending_approval().is_none());
    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
        .split();
    assert!(requests.is_empty());

    // A failed command re-opens the prompt so the user can answer again.
    app.apply_client_update(ClientUpdate::CommandResult {
        command_id: request.command_id,
        result: Err(ClientFailure::new("offline")),
    });
    assert!(app.pending_approval().is_some());

    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .split();
    let ClientRequest::Command(request) = requests.into_iter().next().unwrap() else {
        panic!("expected a command")
    };
    assert!(matches!(
        request.command,
        SessionCommand::RespondToolApproval {
            decision: ApprovalDecision::Deny,
            ..
        }
    ));
}

#[test]
fn approve_for_session_grants_shell_commands_as_prefixes() {
    let mut app = App::new(TuiOptions::default());
    let initial = snapshot();
    let session_id = initial.focused.as_ref().unwrap().summary.id;
    app.apply_snapshot(initial);
    app.sessions.upsert_tool_call(ToolCallSnapshot {
        run_id: id(4, RunId::from_bytes),
        call_ordinal: 1,
        provider_call_id: "call_0".to_owned(),
        arguments: r#"{"command":"cargo test --workspace","cwd":"crates"}"#.to_owned(),
        state: ToolCallState::AwaitingApproval,
        ..fixtures::tool_call(id(8, ToolCallId::from_bytes), session_id, "shell")
    });

    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
        .split();
    let ClientRequest::Command(request) = requests.into_iter().next().unwrap() else {
        panic!("expected a command")
    };
    assert!(matches!(
        request.command,
        SessionCommand::RespondToolApproval {
            decision: ApprovalDecision::ApproveForSession {
                grant: ApprovalGrant::ShellPrefix { prefix },
            },
            ..
        } if prefix == "cargo test --workspace"
    ));
}

#[test]
fn approve_for_workspace_sends_the_decision_and_surfaces_the_promotion() {
    let mut app = App::new(TuiOptions::default());
    let initial = snapshot();
    let session_id = initial.focused.as_ref().unwrap().summary.id;
    app.apply_snapshot(initial);
    app.sessions.upsert_tool_call(ToolCallSnapshot {
        run_id: id(4, RunId::from_bytes),
        call_ordinal: 1,
        provider_call_id: "call_0".to_owned(),
        arguments: r#"{"command":"cargo test --workspace"}"#.to_owned(),
        state: ToolCallState::AwaitingApproval,
        ..fixtures::tool_call(id(8, ToolCallId::from_bytes), session_id, "shell")
    });

    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .split();
    let ClientRequest::Command(request) = requests.into_iter().next().unwrap() else {
        panic!("expected a command")
    };
    assert!(matches!(
        request.command,
        SessionCommand::RespondToolApproval {
            decision: ApprovalDecision::ApproveForWorkspace {
                grant: ApprovalGrant::ShellPrefix { prefix },
            },
            ..
        } if prefix == "cargo test --workspace"
    ));

    let envelope = |sequence, outcome| SessionEventEnvelope {
        run_id: Some(id(4, RunId::from_bytes)),
        caused_by: Some(request.command_id),
        occurred_at_ms: sequence,
        ..fixtures::envelope(
            sequence,
            session_id,
            SessionEvent::WorkspaceGrantPromoted {
                grant: ApprovalGrant::ShellPrefix {
                    prefix: "cargo test --workspace".to_owned(),
                },
                outcome,
            },
        )
    };
    app.apply_live_event(envelope(
        2,
        WorkspaceGrantOutcome::Written {
            path: "/repo/.qq/config.ron".to_owned(),
        },
    ));
    assert_eq!(
        app.status.as_deref(),
        Some("grant written to /repo/.qq/config.ron")
    );
    app.apply_live_event(envelope(
        3,
        WorkspaceGrantOutcome::Failed {
            message: "denied by managed policy".to_owned(),
        },
    ));
    assert_eq!(
        app.status.as_deref(),
        Some("workspace grant not saved: denied by managed policy")
    );
}

#[test]
fn approval_previews_are_kept_only_while_the_approval_is_pending() {
    let mut app = App::new(TuiOptions::default());
    let initial = snapshot();
    let session_id = initial.focused.as_ref().unwrap().summary.id;
    app.apply_snapshot(initial);
    let run_id = id(4, RunId::from_bytes);
    let envelope = |sequence, event| SessionEventEnvelope {
        run_id: Some(run_id),
        occurred_at_ms: sequence,
        ..fixtures::envelope(sequence, session_id, event)
    };
    let mut tool_call = ToolCallSnapshot {
        run_id,
        call_ordinal: 1,
        provider_call_id: "call_0".to_owned(),
        arguments: r#"{"path":"note.txt"}"#.to_owned(),
        state: ToolCallState::AwaitingApproval,
        ..fixtures::tool_call(id(7, ToolCallId::from_bytes), session_id, "edit_file")
    };

    app.apply_live_event(envelope(
        2,
        SessionEvent::ToolApprovalRequested {
            tool_call: tool_call.clone(),
            shell: None,
            edit: Some(qq_protocol::EditPreview {
                path: "note.txt".to_owned(),
                diff: "-old\n+new".to_owned(),
            }),
        },
    ));
    assert_eq!(
        app.pending_approval_preview()
            .and_then(|preview| preview.edit.as_ref())
            .map(|edit| edit.diff.as_str()),
        Some("-old\n+new")
    );

    tool_call.state = ToolCallState::Running;
    app.apply_live_event(envelope(3, SessionEvent::ToolCallStarted { tool_call }));
    assert!(app.pending_approval_preview().is_none());
    assert!(app.sessions[&session_id].approval_previews.is_empty());
}

#[test]
fn slash_command_aliases_quit_and_open_sessions_without_submitting_prompts() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());

    for command in ["/sessions", "/resume"] {
        app.composer.text = command.to_owned();
        let (_, requests) = app
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .split();
        assert!(requests.is_empty());
        assert!(matches!(app.overlay, Some(Overlay::Sessions { .. })));
        app.overlay = None;
    }

    for command in ["/quit", "/exit"] {
        let mut app = App::new(TuiOptions::default());
        app.composer.text = command.to_owned();
        let effects = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(effects.requests().next().is_none());
        assert!(effects.iter().any(|effect| *effect == Effect::Quit));
    }
}

#[test]
fn compact_slash_command_sends_compact_session_for_the_focused_idle_session() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    let session_id = app.focused().unwrap();
    app.composer.text = "/compact".to_owned();

    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();

    let ClientRequest::Command(request) = requests.into_iter().next().unwrap() else {
        panic!("expected a command")
    };
    assert_eq!(
        request.command,
        SessionCommand::CompactSession { session_id }
    );
    assert!(app.composer.text.is_empty());
    assert_eq!(app.status.as_deref(), Some("compacting session..."));
    assert_eq!(
        app.visible_status(),
        Some(("compacting session...", NoticeLevel::Info))
    );
}

#[test]
fn notices_only_render_for_the_session_that_owns_them() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    let owner = app.focused().unwrap();
    let other = id(9, SessionId::from_bytes);

    app.set_error_for(Some(owner), "model request failed".to_owned());
    assert_eq!(
        app.visible_status(),
        Some(("model request failed", NoticeLevel::Error))
    );

    app.view = View::Transcript(Some(other));
    assert_eq!(app.visible_status(), None);

    app.view = View::Transcript(Some(owner));
    assert_eq!(
        app.visible_status(),
        Some(("model request failed", NoticeLevel::Error))
    );
}

#[test]
fn warning_notices_expire_but_error_notices_stick_until_dismissed() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    app.set_notice("temporary notice".to_owned(), NoticeLevel::Warning);
    for _ in 0..NOTICE_TICKS {
        app.advance_animation();
    }
    assert_eq!(app.visible_status(), None, "warning notice remained");
    assert!(!app.has_activity());

    // An error notice never expires on its own: a failure must stay
    // visible until the user acknowledges it (Esc) or replaces it.
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    app.set_notice("model request failed".to_owned(), NoticeLevel::Error);
    for _ in 0..NOTICE_TICKS * 4 {
        app.advance_animation();
    }
    assert_eq!(
        app.visible_status(),
        Some(("model request failed", NoticeLevel::Error))
    );
    let (changed, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .split();
    assert!(changed);
    assert!(requests.is_empty());
    assert_eq!(app.visible_status(), None);
}

#[test]
fn compact_refuses_while_the_focused_session_is_not_idle() {
    let mut app = App::new(TuiOptions::default());
    let mut initial = snapshot();
    initial.sessions[0].status = SessionStatus::Running;
    initial.focused.as_mut().unwrap().summary.status = SessionStatus::Running;
    app.apply_snapshot(initial);
    app.composer.text = "/compact".to_owned();

    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();

    assert!(requests.is_empty());
    assert_eq!(
        app.status.as_deref(),
        Some("compaction needs an idle session; wait or cancel first")
    );
}

#[test]
fn runtime_slash_invocations_are_submitted_as_prompts() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    app.composer.text = "/frobnicate the context".to_owned();

    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();

    assert_eq!(requests.len(), 1);
    let ClientRequest::Command(CommandRequest {
        command:
            SessionCommand::SubmitPrompt {
                session_id: _,
                input,
                limits: _,
                correlation: _,
            },
        ..
    }) = &requests[0]
    else {
        panic!("runtime slash invocation must use the ordinary prompt command")
    };
    assert_eq!(
        input.as_slice(),
        &[qq_protocol::InputPart::text("/frobnicate the context")]
    );
    assert!(app.composer.text.is_empty());
    assert_eq!(app.status, None);
}

#[test]
fn session_compacted_events_surface_the_shrink_in_the_status_line() {
    let mut app = App::new(TuiOptions::default());
    let initial = snapshot();
    let session = initial.focused.as_ref().unwrap().summary.clone();
    app.apply_snapshot(initial);

    app.apply_live_event(SessionEventEnvelope {
        occurred_at_ms: 2,
        ..fixtures::envelope(
            2,
            session.id,
            SessionEvent::SessionCompacted {
                session,
                summary: Some("intent: keep going".to_owned()),
                before_bytes: 3_250_586,
                after_bytes: 245_760,
            },
        )
    });

    assert_eq!(
        app.status.as_deref(),
        Some("compacted: 3.1 MiB -> 240.0 KiB; intent: keep going")
    );

    // A long excerpt is bounded to one line; an absent one adds nothing.
    let session = app.sessions[&app.focused().unwrap()].summary.clone();
    app.apply_live_event(SessionEventEnvelope {
        occurred_at_ms: 3,
        ..fixtures::envelope(
            3,
            session.id,
            SessionEvent::SessionCompacted {
                session: session.clone(),
                summary: Some("word ".repeat(80)),
                before_bytes: 2048,
                after_bytes: 1024,
            },
        )
    });
    let status = app.status.clone().unwrap();
    assert!(status.starts_with("compacted: 2.0 KiB -> 1.0 KiB; word word"));
    assert!(status.chars().count() < 140, "{status}");
    app.apply_live_event(SessionEventEnvelope {
        occurred_at_ms: 4,
        ..fixtures::envelope(
            4,
            session.id,
            SessionEvent::SessionCompacted {
                session,
                summary: None,
                before_bytes: 2048,
                after_bytes: 1024,
            },
        )
    });
    assert_eq!(app.status.as_deref(), Some("compacted: 2.0 KiB -> 1.0 KiB"));
}

#[test]
fn rollback_sends_for_an_idle_session_and_reports_the_receipt() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    let focused = app.focused().unwrap();
    app.composer.text = "/rollback".to_owned();
    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();
    let [ClientRequest::Command(request)] = requests.as_slice() else {
        panic!("expected one rollback command")
    };
    assert!(matches!(
        &request.command,
        SessionCommand::RollbackCompaction { session_id } if *session_id == focused
    ));
    app.apply_client_update(ClientUpdate::CommandResult {
        command_id: request.command_id,
        result: Ok(qq_protocol::CommandReceipt {
            command_id: request.command_id,
            outcome: CommandOutcome::CompactionRolledBack {
                session_id: focused,
                remaining: 0,
            },
            committed_through: fixtures::cursor(2),
        }),
    });
    assert_eq!(
        app.status.as_deref(),
        Some("compaction rolled back; full history restored")
    );

    // A server refusal (nothing to roll back) surfaces as the failure notice.
    let (_, requests) = app.execute(Command::RollbackCompaction).split();
    let [ClientRequest::Command(request)] = requests.as_slice() else {
        panic!("expected one rollback command")
    };
    app.apply_client_update(ClientUpdate::CommandResult {
        command_id: request.command_id,
        result: Err(crate::ClientFailure::new("no compaction to roll back")),
    });
    assert!(
        app.status
            .as_deref()
            .unwrap()
            .contains("no compaction to roll back")
    );

    // A running session is refused locally.
    let (mut running, _, _, _) = running_app();
    let (_, requests) = running.execute(Command::RollbackCompaction).split();
    assert!(requests.is_empty());
    assert!(running.status.as_deref().unwrap().contains("idle session"));
}

#[test]
fn new_slash_command_creates_a_root_session_with_the_selected_model() {
    let model = ModelSelection {
        model: Some("openai/gpt-test".to_owned()),
        max_output_tokens: Some(4_096),
        organization: None,
    };
    let mut app = App::new(TuiOptions {
        settings: Settings::default(),
        model: model.clone(),
        models: Vec::new(),
        themes: Vec::new(),
    });
    app.apply_snapshot(snapshot());
    app.composer.text = "/new".to_owned();

    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();

    assert!(matches!(
        &requests[0],
        ClientRequest::Command(CommandRequest {
            command: SessionCommand::CreateSession {
                parent_id: None,
                model: selected,
                ..
            },
            ..
        }) if selected == &model
    ));
}

#[test]
fn slash_autocomplete_filters_selects_and_executes_commands() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    app.composer.text = "/".to_owned();

    {
        let mut reserved = qq_protocol::RESERVED_CLIENT_SLASH_COMMANDS.to_vec();
        reserved.sort_unstable();
        let mut here: Vec<String> = app
            .filtered_slash_commands()
            .iter()
            .map(|command| command.name.to_string())
            .collect();
        here.sort_unstable();
        assert_eq!(here, reserved);
    }
    let last = qq_protocol::RESERVED_CLIENT_SLASH_COMMANDS.len() - 1;
    for _ in 0..20 {
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    }
    assert_eq!(app.slash_selected(usize::MAX), last);
    for _ in 0..20 {
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    }
    assert_eq!(app.slash_selected(usize::MAX), 0);
    // Down twice lands on /sessions (help, commands, sessions); Tab runs it.
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(app.composer.text.is_empty());
    assert!(matches!(app.overlay, Some(Overlay::Sessions { .. })));

    app.overlay = None;
    app.composer.text = "/qu".to_owned();
    app.slash.select(0);
    assert_eq!(
        app.filtered_slash_commands()[0].name,
        "/quit",
        "a command prefix should hide unrelated commands"
    );
    let effects = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.composer.text.is_empty());
    assert!(effects.iter().any(|effect| *effect == Effect::Quit));
}

#[test]
fn session_picker_searches_titles_and_focuses_the_match() {
    let mut initial = snapshot();
    let target = id(9, SessionId::from_bytes);
    initial.sessions[0].title = "Deploy API".to_owned();
    initial.focused.as_mut().unwrap().summary.title = "Deploy API".to_owned();
    initial.sessions.push(SessionSummary {
        title: "Fix Login Redirect".to_owned(),
        updated_at_ms: 2,
        ..fixtures::session_summary(target)
    });
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(initial);
    app.composer.text = "/sessions".to_owned();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    let (changed, requests) = app
        .handle_terminal_event(Event::Paste("LOGIN".to_owned()))
        .split();

    assert!(changed);
    assert!(requests.is_empty());
    assert_eq!(app.filtered_sessions(), [target]);
    assert_eq!(app.session_picker_selected(), Some(target));

    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();
    assert!(matches!(
        &requests[0],
        ClientRequest::Snapshot(SnapshotRequest {
            focused_session_id: Some(session_id),
            ..
        }) if *session_id == target
    ));
    assert!(app.overlay.is_none());
}

#[test]
fn session_picker_keeps_open_when_search_has_no_matches() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    app.open_sessions();
    app.handle_terminal_event(Event::Paste("missing".to_owned()));

    assert!(app.filtered_sessions().is_empty());
    let (changed, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();
    assert!(!changed);
    assert!(requests.is_empty());
    assert!(matches!(app.overlay, Some(Overlay::Sessions { .. })));
}

#[test]
fn session_picker_deletes_the_highlighted_session_after_a_confirm() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    let session_id = app.focused().unwrap();
    app.open_sessions();

    // The confirm gate: Ctrl-D asks, n keeps, y deletes.
    let (changed, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
        .split();
    assert!(changed);
    assert!(requests.is_empty());
    assert_eq!(
        app.session_picker_confirm(),
        Some(SessionConfirm::Delete(session_id))
    );
    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE))
        .split();
    assert!(requests.is_empty());
    assert_eq!(app.session_picker_confirm(), None);

    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
        .split();
    assert!(matches!(
        &requests[0],
        ClientRequest::Command(CommandRequest {
            command: SessionCommand::DeleteSession { session_id: target },
            ..
        }) if *target == session_id
    ));
    assert_eq!(app.session_picker_confirm(), None);
}

#[test]
fn session_picker_refuses_to_delete_a_session_with_an_active_run() {
    let mut initial = snapshot();
    let run_id = id(8, RunId::from_bytes);
    initial.sessions[0].active_run_id = Some(run_id);
    initial.focused.as_mut().unwrap().summary.active_run_id = Some(run_id);
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(initial);
    app.open_sessions();

    let (changed, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
        .split();

    assert!(changed);
    assert!(requests.is_empty());
    assert_eq!(app.session_picker_confirm(), None);
    assert_eq!(
        app.status.as_deref(),
        Some("cancel the active run before deleting")
    );
}

#[test]
fn session_picker_prunes_empty_sessions_after_a_confirm() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    let workspace_id = app.workspace_id.unwrap();
    // `/prune` opens the picker with the question armed; Ctrl-P no longer
    // carries a destructive meaning anywhere.
    app.composer.text = "/prune".to_owned();
    let (changed, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();
    assert!(changed);
    assert!(requests.is_empty());
    assert_eq!(app.session_picker_confirm(), Some(SessionConfirm::Prune));

    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
        .split();
    assert!(matches!(
        &requests[0],
        ClientRequest::Command(CommandRequest {
            command: SessionCommand::PruneSessions { workspace_id: target },
            ..
        }) if *target == workspace_id
    ));
}

#[test]
fn session_deleted_event_drops_state_and_refocuses_a_neighbor() {
    let mut initial = snapshot();
    let deleted = initial.sessions[0].id;
    let neighbor = id(9, SessionId::from_bytes);
    initial.sessions.push(SessionSummary {
        title: "Neighbor".to_owned(),
        updated_at_ms: 0,
        ..fixtures::session_summary(neighbor)
    });
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(initial);
    assert_eq!(app.focused(), Some(deleted));
    let tool_call_id = id(7, qq_protocol::ToolCallId::from_bytes);
    app.sessions
        .get_mut(&deleted)
        .unwrap()
        .tool_calls
        .as_mut()
        .unwrap()
        .push(ToolCallSnapshot {
            run_id: id(8, RunId::from_bytes),
            call_ordinal: 0,
            provider_call_id: "call_0".to_owned(),
            state: ToolCallState::Running,
            ..fixtures::tool_call(tool_call_id, deleted, "shell")
        });
    app.sessions
        .get_mut(&deleted)
        .unwrap()
        .live_tool_output
        .insert(tool_call_id, "output tail".to_owned());
    app.open_sessions();

    let effects = app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
        occurred_at_ms: 2,
        ..fixtures::envelope(
            2,
            deleted,
            SessionEvent::SessionDeleted {
                session_id: deleted,
            },
        )
    }));

    assert!(effects.redraws());
    assert!(!app.sessions.contains_key(&deleted));
    assert_eq!(app.focused(), Some(neighbor));
    assert_eq!(app.session_picker_selected(), Some(neighbor));
    // The refocus fetches the neighbor's transcript.
    let requests = effects.into_requests();
    assert!(matches!(
        requests.as_slice(),
        [ClientRequest::Snapshot(SnapshotRequest {
            focused_session_id: Some(session_id),
            ..
        })] if *session_id == neighbor
    ));
}

#[test]
fn session_deleted_event_clears_focus_when_no_session_remains() {
    let mut app = App::new(TuiOptions::default());
    let initial = snapshot();
    let deleted = initial.sessions[0].id;
    app.apply_snapshot(initial);
    assert_eq!(app.focused(), Some(deleted));

    let effects = app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
        occurred_at_ms: 2,
        ..fixtures::envelope(
            2,
            deleted,
            SessionEvent::SessionDeleted {
                session_id: deleted,
            },
        )
    }));

    assert!(app.sessions.is_empty());
    assert_eq!(app.focused(), None);
    assert!(effects.requests().next().is_none());
}

#[test]
fn session_updated_event_repoints_the_session_model() {
    let mut app = App::new(TuiOptions::default());
    let initial = snapshot();
    let session_id = initial.sessions[0].id;
    let mut updated = initial.sessions[0].clone();
    updated.model = Some("anthropic/claude-sonnet-5".to_owned());
    app.apply_snapshot(initial);

    app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
        occurred_at_ms: 2,
        ..fixtures::envelope(
            2,
            session_id,
            SessionEvent::SessionUpdated { session: updated },
        )
    }));

    assert_eq!(
        app.sessions[&session_id].summary.model.as_deref(),
        Some("anthropic/claude-sonnet-5")
    );
}

fn context_meter_app() -> App {
    let selection = ModelSelection {
        model: Some("openai/gpt-test".to_owned()),
        max_output_tokens: Some(4_096),
        organization: None,
    };
    App::new(TuiOptions {
        settings: Settings::default(),
        model: selection.clone(),
        models: vec![ModelOption {
            provider: "openai".to_owned(),
            model: "gpt-test".to_owned(),
            name: Some("GPT Test".to_owned()),
            context_window: Some(128_000),
            selection,
        }],
        themes: Vec::new(),
    })
}

#[test]
fn context_usage_uses_last_turn_tokens_live_updates_and_the_model_limit() {
    let mut app = context_meter_app();
    let mut initial = snapshot();
    let session_id = initial.focused.as_ref().unwrap().summary.id;
    // The snapshot rehydrates the meter from session-owned state, not the
    // run's multi-turn billing sum (12_500 here).
    initial.focused.as_mut().unwrap().runs.push(RunSnapshot {
        outcome: Some(RunOutcome::Completed),
        usage: Some(TokenUsage {
            input_tokens: 10_000,
            cache_read_input_tokens: 2_000,
            cache_write_input_tokens: 500,
            output_tokens: 1_000,
            reasoning_tokens: None,
        }),
        context_tokens: Some(9_000),
        estimated_cost_usd_nanos: Some(1),
        ..fixtures::run(id(7, RunId::from_bytes), session_id, RunStatus::Completed)
    });
    initial.focused.as_mut().unwrap().summary.context_tokens = Some(9_000);
    let mut summary = initial.focused.as_ref().unwrap().summary.clone();
    app.apply_snapshot(initial);

    assert_eq!(app.focused_context_usage(), Some((9_000, 128_000)));

    // A committed model turn moves the meter while the run is still going.
    app.apply_live_event(SessionEventEnvelope {
        run_id: Some(id(8, RunId::from_bytes)),
        occurred_at_ms: 2,
        ..fixtures::envelope(
            2,
            session_id,
            SessionEvent::SessionContextUpdated {
                run_id: id(8, RunId::from_bytes),
                context_tokens: Some(15_000),
            },
        )
    });
    assert_eq!(app.focused_context_usage(), Some((15_000, 128_000)));

    // RunFinished settles the meter on the final turn's figure even
    // though the run's summed usage is larger (24_000 here).
    summary.context_tokens = Some(18_000);
    // A persisted pre-v5 run audit event must not transiently repopulate
    // the authoritative session meter during replay.
    app.apply_live_event(SessionEventEnvelope {
        run_id: Some(id(8, RunId::from_bytes)),
        occurred_at_ms: 3,
        ..fixtures::envelope(
            3,
            session_id,
            SessionEvent::RunFinished {
                session: summary,
                run_id: id(8, RunId::from_bytes),
                outcome: RunOutcome::Completed,
                usage: Some(TokenUsage {
                    input_tokens: 20_000,
                    cache_read_input_tokens: 3_000,
                    cache_write_input_tokens: 1_000,
                    output_tokens: 2_000,
                    reasoning_tokens: None,
                }),
                context_tokens: Some(18_000),
            },
        )
    });

    assert_eq!(app.focused_context_usage(), Some((18_000, 128_000)));
}

#[test]
fn prompt_start_and_streaming_do_not_recalculate_session_context() {
    let mut app = context_meter_app();
    app.models[0].context_window = Some(272_000);
    let mut initial = snapshot();
    let session_id = initial.focused.as_ref().unwrap().summary.id;
    initial.focused.as_mut().unwrap().summary.context_tokens = Some(54_400);
    let run_id = id(8, RunId::from_bytes);
    let user_message_id = id(9, MessageId::from_bytes);
    let assistant_message_id = id(10, MessageId::from_bytes);
    let mut summary = initial.focused.as_ref().unwrap().summary.clone();
    app.apply_snapshot(initial);
    let envelope = |sequence, event| SessionEventEnvelope {
        run_id: Some(run_id),
        occurred_at_ms: sequence,
        ..fixtures::envelope(sequence, session_id, event)
    };

    summary.status = SessionStatus::Queued;
    summary.queued_prompts = 1;
    app.apply_live_event(envelope(
        2,
        SessionEvent::PromptQueued {
            session: summary.clone(),
            message: MessageSnapshot {
                run_id,
                turn_ordinal: 0,
                role: MessageRole::User,
                state: MessageState::Queued,
                created_at_ms: 2,
                ..fixtures::message(user_message_id, session_id, "question")
            },
            run: Box::new(fixtures::run(run_id, session_id, RunStatus::Queued)),
            queue_position: 1,
        },
    ));
    assert_eq!(app.focused_context_usage(), Some((54_400, 272_000)));

    summary.status = SessionStatus::Running;
    summary.active_run_id = Some(run_id);
    summary.queued_prompts = 0;
    app.apply_live_event(envelope(
        3,
        SessionEvent::RunStarted {
            session: summary,
            run_id,
            plan: None,
        },
    ));
    app.apply_live_event(envelope(
        4,
        SessionEvent::AssistantMessageStarted {
            message: MessageSnapshot {
                run_id,
                state: MessageState::Streaming,
                created_at_ms: 4,
                ..fixtures::message(assistant_message_id, session_id, "a")
            },
        },
    ));
    app.apply_live_event(envelope(
        5,
        SessionEvent::TextAppended {
            message_id: assistant_message_id,
            channel: qq_protocol::TextChannel::Output,
            text: "nswer".to_owned(),
        },
    ));
    assert_eq!(app.focused_context_usage(), Some((54_400, 272_000)));

    app.apply_live_event(envelope(
        6,
        SessionEvent::SessionContextUpdated {
            run_id,
            context_tokens: Some(13_600),
        },
    ));
    assert_eq!(app.focused_context_usage(), Some((13_600, 272_000)));
}

#[test]
fn legacy_cumulative_usage_is_not_presented_as_context_occupancy() {
    let mut app = context_meter_app();
    app.models[0].context_window = Some(272_000);
    let mut initial = snapshot();
    let session_id = initial.focused.as_ref().unwrap().summary.id;
    // A run persisted before context_tokens existed reports only its
    // cumulative billing usage. Four model turns around tools can easily
    // total 20% of the window even when the last request occupied 5%.
    initial.focused.as_mut().unwrap().runs.push(RunSnapshot {
        outcome: Some(RunOutcome::Completed),
        usage: Some(TokenUsage {
            input_tokens: 40_000,
            cache_read_input_tokens: 12_000,
            cache_write_input_tokens: 2_400,
            output_tokens: 4_000,
            reasoning_tokens: None,
        }),
        estimated_cost_usd_nanos: Some(1),
        ..fixtures::run(id(7, RunId::from_bytes), session_id, RunStatus::Completed)
    });
    app.apply_snapshot(initial);

    assert_eq!(app.focused_context_usage(), None);

    app.apply_live_event(SessionEventEnvelope {
        run_id: Some(id(8, RunId::from_bytes)),
        occurred_at_ms: 2,
        ..fixtures::envelope(
            2,
            session_id,
            SessionEvent::RunContextUpdated {
                run_id: id(8, RunId::from_bytes),
                context_tokens: 13_600,
            },
        )
    });
    assert_eq!(app.focused_context_usage(), None);

    app.apply_live_event(SessionEventEnvelope {
        run_id: Some(id(8, RunId::from_bytes)),
        occurred_at_ms: 3,
        ..fixtures::envelope(
            3,
            session_id,
            SessionEvent::SessionContextUpdated {
                run_id: id(8, RunId::from_bytes),
                context_tokens: Some(13_600),
            },
        )
    });
    assert_eq!(app.focused_context_usage(), Some((13_600, 272_000)));
}

#[test]
fn compaction_run_usage_does_not_become_session_context() {
    let mut app = context_meter_app();
    app.models[0].context_window = Some(272_000);
    let mut initial = snapshot();
    let session_id = initial.focused.as_ref().unwrap().summary.id;
    initial.focused.as_mut().unwrap().summary.context_tokens = Some(54_400);
    app.apply_snapshot(initial);
    assert_eq!(app.focused_context_usage(), Some((54_400, 272_000)));

    let mut compacted = app.sessions[&session_id].summary.clone();
    compacted.context_tokens = None;
    app.apply_live_event(SessionEventEnvelope {
        run_id: Some(id(8, RunId::from_bytes)),
        occurred_at_ms: 2,
        ..fixtures::envelope(
            2,
            session_id,
            SessionEvent::RunFinished {
                session: compacted.clone(),
                run_id: id(8, RunId::from_bytes),
                outcome: RunOutcome::Completed,
                usage: Some(TokenUsage {
                    input_tokens: 54_000,
                    cache_read_input_tokens: 6_000,
                    cache_write_input_tokens: 0,
                    output_tokens: 2_000,
                    reasoning_tokens: None,
                }),
                // This is the compaction request's pre-summary input, not
                // the session occupancy after the summary replaced it.
                context_tokens: Some(60_000),
            },
        )
    });
    assert_eq!(app.focused_context_usage(), None);

    app.apply_live_event(SessionEventEnvelope {
        run_id: Some(id(8, RunId::from_bytes)),
        occurred_at_ms: 3,
        ..fixtures::envelope(
            3,
            session_id,
            SessionEvent::SessionCompacted {
                session: compacted,
                summary: Some("short summary".to_owned()),
                before_bytes: 200_000,
                after_bytes: 1_000,
            },
        )
    });
    assert_eq!(app.focused_context_usage(), None);
    assert_eq!(
        app.sessions[&app.focused().unwrap()].context_window,
        Some(272_000)
    );
}

#[test]
fn discovered_models_refresh_existing_session_metadata() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    assert_eq!(app.focused_context_usage(), None);

    app.apply_client_update(ClientUpdate::Models {
        models: vec![ModelDescriptor {
            provider: "openai".to_owned(),
            model: "gpt-test".to_owned(),
            name: Some("GPT Test".to_owned()),
            context_window: Some(128_000),
            selection: ModelSelection {
                model: Some("openai/gpt-test".to_owned()),
                max_output_tokens: Some(4_096),
                organization: None,
            },
        }],
        selected: None,
    });

    let focused = app.focused().unwrap();
    assert_eq!(app.models.len(), 1);
    assert_eq!(app.sessions[&focused].context_window, Some(128_000));
}

#[test]
fn model_refresh_preserves_the_open_picker_selection_by_identity() {
    let selection = ModelSelection {
        model: Some("zeta/model-z".to_owned()),
        max_output_tokens: Some(4_096),
        organization: None,
    };
    let mut app = App::new(TuiOptions {
        settings: Settings::default(),
        model: selection.clone(),
        models: vec![ModelOption {
            provider: "zeta".to_owned(),
            model: "model-z".to_owned(),
            name: Some("Zeta".to_owned()),
            context_window: None,
            selection: selection.clone(),
        }],
        themes: Vec::new(),
    });
    app.apply_snapshot(snapshot());
    app.open_models();

    app.apply_client_update(ClientUpdate::Models {
        models: vec![
            ModelDescriptor {
                provider: "alpha".to_owned(),
                model: "model-a".to_owned(),
                name: Some("Alpha".to_owned()),
                context_window: Some(64_000),
                selection: ModelSelection {
                    model: Some("alpha/model-a".to_owned()),
                    max_output_tokens: Some(4_096),
                    organization: None,
                },
            },
            ModelDescriptor {
                provider: "zeta".to_owned(),
                model: "model-z".to_owned(),
                name: Some("Zeta".to_owned()),
                context_window: Some(128_000),
                selection: selection.clone(),
            },
        ],
        selected: Some(selection.clone()),
    });
    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();

    assert_eq!(app.model, selection);
    // A session is focused, so Enter applies the preserved selection to
    // it rather than creating a new session.
    let focused = app.focused().unwrap();
    assert!(matches!(
        &requests[0],
        ClientRequest::Command(CommandRequest {
            command: SessionCommand::SetSessionModel { session_id, model },
            ..
        }) if model == &selection && *session_id == focused
    ));
}

#[test]
fn first_focused_snapshot_can_arrive_after_the_workspace_snapshot() {
    let mut empty = snapshot();
    empty.sessions.clear();
    empty.focused = None;
    let mut app = App::new(TuiOptions::default());

    app.apply_snapshot(empty);
    app.apply_snapshot(snapshot());

    assert!(app.focused().is_some());
}

#[test]
fn model_picker_applies_to_the_focused_session_and_ctrl_n_creates() {
    let selection = ModelSelection {
        model: Some("anthropic/claude-sonnet-5".to_owned()),
        max_output_tokens: Some(8_192),
        organization: None,
    };
    let mut app = App::new(TuiOptions {
        settings: Settings::default(),
        model: ModelSelection::default(),
        models: vec![ModelOption {
            provider: "anthropic".to_owned(),
            model: "claude-sonnet-5".to_owned(),
            name: Some("Claude Sonnet 5".to_owned()),
            context_window: Some(200_000),
            selection: selection.clone(),
        }],
        themes: Vec::new(),
    });
    app.apply_snapshot(snapshot());
    app.composer.text = "/models".to_owned();

    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();
    assert!(requests.is_empty());
    assert!(matches!(app.overlay, Some(Overlay::Models(_))));
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
    assert_eq!(app.filtered_models(), vec![0]);

    // Enter with a focused session repoints that session's model and
    // remembers it as the client default for later /new creates.
    let focused = app.focused().unwrap();
    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();
    let ClientRequest::Command(request) = &requests[0] else {
        panic!("expected set-session-model command")
    };
    assert!(matches!(
        &request.command,
        SessionCommand::SetSessionModel { session_id, model }
            if model == &selection && *session_id == focused
    ));
    assert_eq!(app.model, selection);
    assert!(app.overlay.is_none());

    // Ctrl-N creates a fresh session with the selected model instead.
    app.open_models();
    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL))
        .split();
    let ClientRequest::Command(request) = &requests[0] else {
        panic!("expected create-session command")
    };
    assert!(matches!(
        &request.command,
        SessionCommand::CreateSession {
            parent_id: None,
            model,
            ..
        } if model == &selection
    ));
    assert_eq!(app.model, selection);
    assert!(app.overlay.is_none());
}

#[test]
fn model_picker_enter_without_a_focused_session_creates_one() {
    let selection = ModelSelection {
        model: Some("anthropic/claude-sonnet-5".to_owned()),
        max_output_tokens: Some(8_192),
        organization: None,
    };
    let mut app = App::new(TuiOptions {
        settings: Settings::default(),
        model: ModelSelection::default(),
        models: vec![ModelOption {
            provider: "anthropic".to_owned(),
            model: "claude-sonnet-5".to_owned(),
            name: Some("Claude Sonnet 5".to_owned()),
            context_window: Some(200_000),
            selection: selection.clone(),
        }],
        themes: Vec::new(),
    });
    let mut empty = snapshot();
    empty.sessions.clear();
    empty.focused = None;
    app.apply_snapshot(empty);
    assert!(app.focused().is_none());
    app.open_models();

    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();

    let ClientRequest::Command(request) = &requests[0] else {
        panic!("expected create-session command")
    };
    assert!(matches!(
        &request.command,
        SessionCommand::CreateSession {
            parent_id: None,
            model,
            ..
        } if model == &selection
    ));
    assert_eq!(app.model, selection);
    assert!(app.overlay.is_none());
}

#[test]
fn model_picker_selection_becomes_the_default_for_new_sessions() {
    let initial = ModelSelection {
        model: Some("openai/gpt-test".to_owned()),
        max_output_tokens: Some(4_096),
        organization: None,
    };
    let switched = ModelSelection {
        model: Some("anthropic/claude-sonnet-5".to_owned()),
        max_output_tokens: Some(8_192),
        organization: None,
    };
    let mut app = App::new(TuiOptions {
        settings: Settings::default(),
        model: initial,
        models: vec![ModelOption {
            provider: "anthropic".to_owned(),
            model: "claude-sonnet-5".to_owned(),
            name: Some("Claude Sonnet 5".to_owned()),
            context_window: Some(200_000),
            selection: switched.clone(),
        }],
        themes: Vec::new(),
    });
    app.apply_snapshot(snapshot());
    app.open_models();

    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();
    assert!(matches!(
        &requests[0],
        ClientRequest::Command(CommandRequest {
            command: SessionCommand::SetSessionModel { model, .. },
            ..
        }) if model == &switched
    ));
    assert_eq!(app.model, switched);

    let (_, requests) = app.execute(Command::NewRootSession).split();
    assert!(matches!(
        &requests[0],
        ClientRequest::Command(CommandRequest {
            command: SessionCommand::CreateSession { model, .. },
            ..
        }) if model == &switched
    ));
}

#[test]
fn new_inherits_the_focused_session_model_when_no_default_is_loaded() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    app.model = ModelSelection::default();
    app.composer.text = "/new".to_owned();

    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();

    assert!(matches!(
        &requests[0],
        ClientRequest::Command(CommandRequest {
            command: SessionCommand::CreateSession { model, .. },
            ..
        }) if model.model.as_deref() == Some("openai/gpt-test")
    ));
}

#[test]
fn create_without_a_default_or_focused_session_still_requires_a_model() {
    let mut initial = snapshot();
    initial.sessions.clear();
    initial.focused = None;
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(initial);

    let (_, requests) = app.execute(Command::NewRootSession).split();

    assert!(requests.is_empty());
    assert_eq!(
        app.status.as_deref(),
        Some("choose a model with /models before creating a session")
    );
}

#[test]
fn reset_preserves_an_in_flight_prompt_until_its_result() {
    let mut app = App::new(TuiOptions::default());
    let snapshot = snapshot();
    app.apply_snapshot(snapshot.clone());
    app.composer.text = "keep me".to_owned();
    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();
    let ClientRequest::Command(request) = requests.into_iter().next().unwrap() else {
        panic!("expected command")
    };

    app.apply_client_update(ClientUpdate::ResetSnapshot(snapshot));
    app.apply_client_update(ClientUpdate::CommandResult {
        command_id: request.command_id,
        result: Err(ClientFailure::new("server restarted")),
    });

    assert_eq!(app.composer.text, "keep me");
}

#[test]
fn durable_events_update_the_focused_transcript() {
    let mut app = App::new(TuiOptions::default());
    let snapshot = snapshot();
    let session_id = snapshot.focused.as_ref().unwrap().summary.id;
    app.apply_snapshot(snapshot);
    let run_id = id(4, RunId::from_bytes);
    let message_id = id(5, MessageId::from_bytes);
    let event = |sequence, event| SessionEventEnvelope {
        run_id: Some(run_id),
        occurred_at_ms: sequence,
        ..fixtures::envelope(sequence, session_id, event)
    };
    let message = MessageSnapshot {
        run_id,
        state: MessageState::Streaming,
        created_at_ms: 2,
        ..fixtures::message(message_id, session_id, "")
    };

    app.apply_live_event(event(2, SessionEvent::AssistantMessageStarted { message }));
    app.apply_live_event(event(
        3,
        SessionEvent::TextAppended {
            message_id,
            channel: qq_protocol::TextChannel::Output,
            text: "hello".to_owned(),
        },
    ));
    let tool_call_id = id(6, ToolCallId::from_bytes);
    let mut tool_call = ToolCallSnapshot {
        run_id,
        call_ordinal: 1,
        provider_call_id: "call-1".to_owned(),
        arguments: r#"{"path":"note.txt"}"#.to_owned(),
        state: ToolCallState::Requested,
        ..fixtures::tool_call(tool_call_id, session_id, "read_file")
    };
    app.apply_live_event(event(
        4,
        SessionEvent::ToolCallRequested {
            tool_call: tool_call.clone(),
        },
    ));
    tool_call.state = ToolCallState::Completed;
    tool_call.result = Some("contents".to_owned());
    app.apply_live_event(event(
        5,
        SessionEvent::ToolCallFinished {
            tool_call: tool_call.clone(),
        },
    ));

    assert_eq!(
        app.sessions[&session_id].messages.as_ref().unwrap()[0].output,
        "hello"
    );
    assert_eq!(
        app.sessions[&session_id].tool_calls.as_deref(),
        Some([tool_call].as_slice())
    );
}

#[test]
fn live_tool_output_keeps_a_bounded_tail_and_drops_on_terminal_states() {
    let mut app = App::new(TuiOptions::default());
    let initial = snapshot();
    let session_id = initial.focused.as_ref().unwrap().summary.id;
    app.apply_snapshot(initial.clone());
    let run_id = id(4, RunId::from_bytes);
    let tool_call_id = id(6, ToolCallId::from_bytes);
    let event = |sequence, event| SessionEventEnvelope {
        run_id: Some(run_id),
        occurred_at_ms: sequence,
        ..fixtures::envelope(sequence, session_id, event)
    };
    let delta = |sequence, chunk: &str| {
        event(
            sequence,
            SessionEvent::ToolCallOutputDelta {
                tool_call_id,
                chunk: chunk.to_owned(),
            },
        )
    };

    app.apply_live_event(delta(2, "hello "));
    app.apply_live_event(delta(3, "world\n"));
    assert_eq!(
        app.sessions[&session_id]
            .live_tool_output
            .get(&tool_call_id)
            .map(String::as_str),
        Some("hello world\n")
    );

    // Overflow drops the head — the tail is a live view, not a record —
    // and trimming lands on a character boundary even when the bound
    // falls inside a multi-byte character.
    app.apply_live_event(delta(4, &"€".repeat(2 * MAX_LIVE_TOOL_OUTPUT_BYTES / 3)));
    let buffer = app.sessions[&session_id]
        .live_tool_output
        .get(&tool_call_id)
        .unwrap();
    assert!(buffer.len() <= MAX_LIVE_TOOL_OUTPUT_BYTES);
    assert!(buffer.len() > MAX_LIVE_TOOL_OUTPUT_BYTES - 4);
    assert!(buffer.chars().all(|character| character == '€'));

    // A terminal state hands display over to the persisted result.
    app.apply_live_event(event(
        5,
        SessionEvent::ToolCallFinished {
            tool_call: ToolCallSnapshot {
                run_id,
                call_ordinal: 1,
                provider_call_id: "call-1".to_owned(),
                arguments: r#"{"command":"cargo build"}"#.to_owned(),
                result: Some("ok\n".to_owned()),
                ..fixtures::tool_call(tool_call_id, session_id, "shell")
            },
        },
    ));
    assert!(app.sessions[&session_id].live_tool_output.is_empty());

    // A session snapshot reload replaces live per-call state wholesale.
    app.apply_live_event(delta(6, "restarted\n"));
    assert!(!app.sessions[&session_id].live_tool_output.is_empty());
    let mut reloaded = initial;
    reloaded.cursor.sequence = 7;
    app.apply_snapshot(reloaded);
    assert!(app.sessions[&session_id].live_tool_output.is_empty());
}

#[test]
fn focused_snapshot_is_a_session_baseline_not_a_workspace_cursor() {
    let mut app = App::new(TuiOptions::default());
    let mut initial = snapshot();
    let session_id = initial.focused.as_ref().unwrap().summary.id;
    let run_id = id(4, RunId::from_bytes);
    let message_id = id(5, MessageId::from_bytes);
    initial
        .focused
        .as_mut()
        .unwrap()
        .messages
        .push(MessageSnapshot {
            run_id,
            state: MessageState::Streaming,
            created_at_ms: 2,
            ..fixtures::message(message_id, session_id, "")
        });
    app.apply_snapshot(initial.clone());

    let mut ahead = initial;
    ahead.cursor.sequence = 3;
    ahead.focused.as_mut().unwrap().messages[0].output = "ab".to_owned();
    app.apply_snapshot(ahead);
    let event = |sequence, text: &str| SessionEventEnvelope {
        run_id: Some(run_id),
        occurred_at_ms: sequence,
        ..fixtures::envelope(
            sequence,
            session_id,
            SessionEvent::TextAppended {
                message_id,
                channel: qq_protocol::TextChannel::Output,
                text: text.to_owned(),
            },
        )
    };

    app.apply_live_event(event(2, "a"));
    app.apply_live_event(event(3, "b"));
    app.apply_live_event(event(4, "c"));

    assert_eq!(app.last_sequence, 4);
    assert_eq!(
        app.sessions[&session_id].messages.as_ref().unwrap()[0].output,
        "abc"
    );
}

#[test]
fn stale_snapshot_cannot_change_the_selected_session() {
    let mut app = App::new(TuiOptions::default());
    let mut initial = snapshot();
    let old_focus = initial.focused.as_ref().unwrap().summary.id;
    let new_focus = id(9, SessionId::from_bytes);
    initial.sessions.push(SessionSummary {
        workspace_id: initial.workspace.id,
        title: "New focus".to_owned(),
        updated_at_ms: 2,
        ..fixtures::session_summary(new_focus)
    });
    app.apply_snapshot(initial.clone());
    app.focus_session(new_focus);

    assert!(!app.apply_snapshot(initial).redraws());
    assert_eq!(app.focused(), Some(new_focus));
    assert_ne!(app.focused(), Some(old_focus));
}

#[test]
fn focused_transcript_retains_only_the_snapshot_window() {
    let mut app = App::new(TuiOptions::default());
    let mut initial = snapshot();
    let session_id = initial.focused.as_ref().unwrap().summary.id;
    let run_id = id(4, RunId::from_bytes);
    let messages = &mut initial.focused.as_mut().unwrap().messages;
    for index in 0..usize::from(SNAPSHOT_MESSAGE_LIMIT) + 4 {
        messages.push(MessageSnapshot {
            id: MessageId::from_bytes((index as u128 + 1).to_be_bytes()),
            session_id,
            run_id,
            turn_ordinal: 0,
            role: MessageRole::Assistant,
            state: MessageState::Complete,
            steering: false,
            truncated: false,
            output: index.to_string(),
            refusal: String::new(),
            created_at_ms: index as u64,
        });
    }

    app.apply_snapshot(initial);
    let retained = app.sessions[&session_id].messages.as_ref().unwrap();
    assert_eq!(retained.len(), usize::from(SNAPSHOT_MESSAGE_LIMIT));
    assert_eq!(retained.first().unwrap().output, "4");

    app.sessions.push_message(MessageSnapshot {
        run_id,
        turn_ordinal: 0,
        created_at_ms: u64::MAX,
        ..fixtures::message(
            MessageId::from_bytes(u128::MAX.to_be_bytes()),
            session_id,
            "newest",
        )
    });
    let retained = app.sessions[&session_id].messages.as_ref().unwrap();
    assert_eq!(retained.len(), usize::from(SNAPSHOT_MESSAGE_LIMIT));
    assert_eq!(retained.last().unwrap().output, "newest");
}

#[test]
fn mid_run_queued_prompts_stay_after_the_streaming_runs_turn_messages() {
    let mut app = App::new(TuiOptions::default());
    let initial = snapshot();
    let session_id = initial.focused.as_ref().unwrap().summary.id;
    app.apply_snapshot(initial);
    let streaming_run = id(4, RunId::from_bytes);
    let queued_run = id(5, RunId::from_bytes);
    let message = |byte, run_id, turn_ordinal, role, state, output: &str| MessageSnapshot {
        run_id,
        turn_ordinal,
        role,
        state,
        created_at_ms: u64::from(byte),
        ..fixtures::message(id(byte, MessageId::from_bytes), session_id, output)
    };

    app.sessions.push_message(message(
        6,
        streaming_run,
        0,
        MessageRole::User,
        MessageState::Complete,
        "prompt one",
    ));
    app.sessions.push_message(message(
        7,
        streaming_run,
        1,
        MessageRole::Assistant,
        MessageState::Complete,
        "turn one",
    ));
    // A prompt queued mid-run arrives before the run's later per-turn
    // messages...
    app.sessions.push_message(message(
        8,
        queued_run,
        0,
        MessageRole::User,
        MessageState::Queued,
        "queued prompt",
    ));
    app.sessions.push_message(message(
        9,
        streaming_run,
        2,
        MessageRole::Assistant,
        MessageState::Streaming,
        "turn two",
    ));

    // ...yet the live list keeps the snapshot's run-first order: the
    // whole streaming run, then the queued prompt's run.
    let outputs = app.sessions[&session_id]
        .messages
        .as_ref()
        .unwrap()
        .iter()
        .map(|message| message.output.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        outputs,
        ["prompt one", "turn one", "turn two", "queued prompt"]
    );
}

#[test]
fn ctrl_o_cycles_tool_detail_and_yields_to_overlays() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    assert_eq!(app.tool_detail, ToolDetail::Rows);
    let ctrl_o = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);

    let (changed, requests) = app.handle_key(ctrl_o).split();
    assert!(changed);
    assert!(requests.is_empty());
    assert_eq!(app.tool_detail, ToolDetail::Folded);

    app.handle_key(ctrl_o);
    assert_eq!(app.tool_detail, ToolDetail::Rows);

    // Pickers own the keyboard; the toggle must not fire underneath them.
    app.open_session_picker_with("", None, None);
    app.handle_key(ctrl_o);
    assert_eq!(app.tool_detail, ToolDetail::Rows);
}

#[test]
fn page_keys_scroll_the_transcript_by_one_visible_page() {
    let mut app = App::new(TuiOptions::default());
    app.update_transcript_viewport(100, 12, false);

    let (changed, requests) = app
        .handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )))
        .split();

    assert!(changed);
    assert!(requests.is_empty());
    assert_eq!(app.transcript_scroll_offset(), 12);

    let (changed, requests) = app
        .handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::PageDown,
            KeyModifiers::NONE,
        )))
        .split();

    assert!(changed);
    assert!(requests.is_empty());
    assert_eq!(app.transcript_scroll_offset(), 0);
}

#[test]
fn mouse_wheel_scrolls_the_transcript_by_three_rows() {
    let mut app = App::new(TuiOptions::default());
    app.update_transcript_viewport(100, 12, false);

    let mouse = |kind| {
        Event::Mouse(MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })
    };
    let (changed, requests) = app
        .handle_terminal_event(mouse(MouseEventKind::ScrollUp))
        .split();

    assert!(changed);
    assert!(requests.is_empty());
    assert_eq!(app.transcript_scroll_offset(), 3);

    let (changed, requests) = app
        .handle_terminal_event(mouse(MouseEventKind::ScrollDown))
        .split();

    assert!(changed);
    assert!(requests.is_empty());
    assert_eq!(app.transcript_scroll_offset(), 0);
}

#[test]
fn streamed_rows_do_not_move_a_scrolled_transcript() {
    let mut app = App::new(TuiOptions::default());
    app.update_transcript_viewport(40, 10, false);
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::PageUp,
        KeyModifiers::NONE,
    )));

    app.update_transcript_viewport(45, 10, false);

    assert_eq!(app.transcript_scroll_offset(), 15);
}

#[test]
fn session_and_view_changes_return_the_transcript_to_the_live_tail() {
    let mut app = App::new(TuiOptions::default());
    app.view = View::Transcript(Some(SessionId::from_bytes([1; 16])));
    app.update_transcript_viewport(100, 10, false);
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::PageUp,
        KeyModifiers::NONE,
    )));

    app.view = View::Transcript(Some(SessionId::from_bytes([2; 16])));
    app.update_transcript_viewport(100, 10, false);

    assert_eq!(app.transcript_scroll_offset(), 0);

    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::PageUp,
        KeyModifiers::NONE,
    )));
    app.execute(Command::ShowAttention);
    app.update_transcript_viewport(100, 10, false);

    assert_eq!(app.transcript_scroll_offset(), 0);
}

#[test]
fn scrolling_clamps_at_the_oldest_row_and_the_live_tail() {
    let mut app = App::new(TuiOptions::default());
    app.update_transcript_viewport(25, 10, false);
    let page_up = Event::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    let page_down = Event::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));

    assert!(app.handle_terminal_event(page_up.clone()).split().0);
    assert!(app.handle_terminal_event(page_up.clone()).split().0);
    assert_eq!(app.transcript_scroll_offset(), 15);
    assert!(!app.handle_terminal_event(page_up).split().0);

    assert!(app.handle_terminal_event(page_down.clone()).split().0);
    assert!(app.handle_terminal_event(page_down.clone()).split().0);
    assert_eq!(app.transcript_scroll_offset(), 0);
    assert!(!app.handle_terminal_event(page_down).split().0);
}

#[test]
fn transcript_scroll_controls_are_ignored_by_overlays() {
    let mut app = App::new(TuiOptions::default());
    app.update_transcript_viewport(100, 10, false);
    app.open_model_picker_for_test();
    let wheel = Event::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    });
    let page = Event::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));

    assert!(!app.handle_terminal_event(wheel.clone()).split().0);
    assert!(!app.handle_terminal_event(page.clone()).split().0);
    assert_eq!(app.transcript_scroll_offset(), 0);

    app.overlay = None;
    app.open_session_picker_with("", app.focused(), None);
    assert!(!app.handle_terminal_event(wheel).split().0);
    assert!(!app.handle_terminal_event(page).split().0);
    assert_eq!(app.transcript_scroll_offset(), 0);
}

fn summary_named(byte: u8, title: &str) -> SessionSummary {
    SessionSummary {
        title: title.to_owned(),
        updated_at_ms: u64::from(byte),
        ..fixtures::session_summary(id(byte, SessionId::from_bytes))
    }
}

fn body_for(summary: &SessionSummary, output: &str) -> SessionSnapshot {
    SessionSnapshot {
        summary: summary.clone(),
        messages: vec![MessageSnapshot {
            id: id(summary.id.as_bytes()[0], MessageId::from_bytes),
            session_id: summary.id,
            run_id: id(0xaa, RunId::from_bytes),
            turn_ordinal: 1,
            role: MessageRole::Assistant,
            state: MessageState::Complete,
            steering: false,
            truncated: false,
            output: output.to_owned(),
            refusal: String::new(),
            created_at_ms: 1,
        }],
        runs: Vec::new(),
        tool_calls: Vec::new(),
        has_older_tool_calls: false,
        has_older_messages: false,
    }
}

#[test]
fn creating_a_session_adopts_it_without_a_snapshot_round_trip() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    let (_, requests) = app.execute(Command::NewRootSession).split();
    let [ClientRequest::Command(request)] = requests.as_slice() else {
        panic!("expected one create command, got {requests:?}");
    };
    let created = id(0x42, SessionId::from_bytes);
    let mut summary = summary_named(0x42, "New session");
    summary.updated_at_ms = 99;

    // The durable event arrives first (the SSE stream is usually ahead of
    // the HTTP receipt); focus moves and the body is already warm.
    let effects = app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
        caused_by: Some(request.command_id),
        ..fixtures::envelope(
            2,
            created,
            SessionEvent::SessionCreated {
                session: summary.clone(),
            },
        )
    }));
    assert!(effects.redraws());
    assert_eq!(app.focused(), Some(created));
    assert!(app.sessions[&created].is_warm());
    assert!(
        effects.requests().next().is_none(),
        "no snapshot after create"
    );

    // The receipt confirms without changing anything or requesting more.
    let effects = app.apply_client_update(ClientUpdate::CommandResult {
        command_id: request.command_id,
        result: Ok(qq_protocol::CommandReceipt {
            command_id: request.command_id,
            outcome: CommandOutcome::SessionCreated {
                session_id: created,
            },
            committed_through: fixtures::cursor(2),
        }),
    });
    assert_eq!(app.focused(), Some(created));
    assert!(effects.requests().next().is_none());
    // The previously focused session keeps its body warm.
    let previous = snapshot().sessions[0].id;
    assert!(app.sessions[&previous].is_warm());
}

#[test]
fn switching_to_a_warm_session_needs_no_request_and_a_cold_one_does() {
    let mut app = App::new(TuiOptions::default());
    let mut initial = snapshot();
    let warm = summary_named(0x51, "warm");
    let cold = summary_named(0x52, "cold");
    initial.sessions.push(warm.clone());
    initial.sessions.push(cold.clone());
    initial.included.push(body_for(&warm, "warm body"));
    app.apply_snapshot(initial);
    let first = snapshot().sessions[0].id;
    assert_eq!(app.focused(), Some(first));
    assert!(app.sessions[&warm.id].is_warm());
    assert!(!app.sessions[&cold.id].is_warm());

    let (changed, requests) = app.focus_session(warm.id).split();
    assert!(changed);
    assert!(requests.is_empty(), "warm switch must not request");
    assert_eq!(app.focused(), Some(warm.id));
    assert!(app.sessions[&first].is_warm(), "leaving does not evict");

    let (_, requests) = app.focus_session(cold.id).split();
    assert!(matches!(
        requests.as_slice(),
        [ClientRequest::Snapshot(SnapshotRequest {
            focused_session_id: Some(id),
            ..
        })] if *id == cold.id
    ));
    assert_eq!(app.focused(), Some(cold.id));
}

#[test]
fn warm_bodies_are_bounded_and_evict_least_recently_focused() {
    let mut app = App::new(TuiOptions::default());
    let mut initial = snapshot();
    let ids: Vec<SessionId> = (0x60..0x60 + (WARM_BODY_LIMIT as u8) + 2)
        .map(|byte| {
            let summary = summary_named(byte, "s");
            initial.sessions.push(summary.clone());
            summary.id
        })
        .collect();
    app.apply_snapshot(initial);
    // Focus each in turn, loading a body every time.
    for session_id in &ids {
        app.focus_session(*session_id);
        let summary = app.sessions[session_id].summary.clone();
        app.apply_snapshot(WorkspaceSnapshot {
            focused: Some(body_for(&summary, "body")),
            ..snapshot()
        });
    }
    let warm: Vec<_> = app.sessions.values().filter(|s| s.is_warm()).collect();
    assert_eq!(warm.len(), WARM_BODY_LIMIT);
    // The most recent WARM_BODY_LIMIT are warm; the earliest two are not.
    assert!(!app.sessions[&ids[0]].is_warm());
    assert!(!app.sessions[&ids[1]].is_warm());
    assert!(app.sessions[ids.last().unwrap()].is_warm());
    // Cold sessions keep their summary and status.
    assert_eq!(app.sessions[&ids[0]].summary.title, "s");
}

#[test]
fn live_status_tracks_cold_sessions_and_activity_seeds_from_snapshots() {
    let mut app = App::new(TuiOptions::default());
    let mut initial = snapshot();
    let mut child = summary_named(0x71, "child");
    child.parent_id = Some(initial.sessions[0].id);
    child.status = SessionStatus::Running;
    child.active_run_id = Some(id(0x72, RunId::from_bytes));
    child.activity = Some(RunActivity::Reasoning);
    initial.sessions.push(child.clone());
    app.apply_snapshot(initial);
    assert!(!app.sessions[&child.id].is_warm());
    assert_eq!(
        app.sessions[&child.id].activity,
        Some((id(0x72, RunId::from_bytes), RunActivity::Reasoning))
    );

    let mut sequence = 1;
    let mut event = |event: SessionEvent| {
        sequence += 1;
        ClientUpdate::Event(SessionEventEnvelope {
            run_id: child.active_run_id,
            occurred_at_ms: sequence,
            ..fixtures::envelope(sequence, child.id, event)
        })
    };
    let message = MessageSnapshot {
        run_id: child.active_run_id.unwrap(),
        state: MessageState::Streaming,
        ..fixtures::message(id(0x73, MessageId::from_bytes), child.id, "")
    };
    app.apply_client_update(event(SessionEvent::AssistantMessageStarted { message }));
    app.apply_client_update(event(SessionEvent::TextAppended {
        message_id: id(0x73, MessageId::from_bytes),
        channel: TextChannel::Output,
        text: "Reading   the\nrepository ".to_owned(),
    }));
    app.apply_client_update(event(SessionEvent::TextAppended {
        message_id: id(0x73, MessageId::from_bytes),
        channel: TextChannel::Output,
        text: "layout".to_owned(),
    }));
    let call = ToolCallSnapshot {
        run_id: child.active_run_id.unwrap(),
        call_ordinal: 0,
        provider_call_id: "c".to_owned(),
        state: ToolCallState::AwaitingApproval,
        ..fixtures::tool_call(id(0x74, ToolCallId::from_bytes), child.id, "search")
    };
    app.apply_client_update(event(SessionEvent::ToolApprovalRequested {
        tool_call: call.clone(),
        shell: None,
        edit: None,
    }));

    let live = &app.sessions[&child.id].live;
    assert_eq!(live.tail, "Reading the repository layout");
    assert_eq!(live.active_tool.as_deref(), Some("search"));
    assert_eq!(live.awaiting_approval.len(), 1);
    // Still cold: deltas did not create a body.
    assert!(!app.sessions[&child.id].is_warm());

    let finished = ToolCallSnapshot {
        state: ToolCallState::Completed,
        ..call
    };
    app.apply_client_update(event(SessionEvent::ToolCallFinished {
        tool_call: finished,
    }));
    let live = &app.sessions[&child.id].live;
    assert_eq!(live.active_tool, None);
    assert!(live.awaiting_approval.is_empty());

    // A long stream keeps the tail bounded.
    app.apply_client_update(event(SessionEvent::TextAppended {
        message_id: id(0x73, MessageId::from_bytes),
        channel: TextChannel::Output,
        text: "x".repeat(LIVE_TAIL_BYTES * 3),
    }));
    assert!(app.sessions[&child.id].live.tail.len() <= LIVE_TAIL_BYTES);
}

#[test]
fn agents_picker_lists_only_the_focused_roots_subtree() {
    let mut app = App::new(TuiOptions::default());
    let mut initial = snapshot();
    let root = initial.sessions[0].id;
    let mut child = summary_named(0x81, "child");
    child.parent_id = Some(root);
    let mut grandchild = summary_named(0x82, "grandchild");
    grandchild.parent_id = Some(child.id);
    let other_root = summary_named(0x83, "other root");
    initial
        .sessions
        .extend([child.clone(), grandchild.clone(), other_root.clone()]);
    app.apply_snapshot(initial);

    // From deep in the tree, /agents scopes to the whole root's subtree.
    app.focus_session(grandchild.id);
    app.execute(Command::OpenAgents);
    let mut listed = app.filtered_sessions();
    listed.sort();
    let mut expected = vec![root, child.id, grandchild.id];
    expected.sort();
    assert_eq!(listed, expected);
    assert!(!app.filtered_sessions().contains(&other_root.id));

    // /sessions lists everything.
    app.execute(Command::OpenSessions);
    assert_eq!(app.filtered_sessions().len(), 4);
}

/// A snapshot whose one session is mid-run, plus the envelope builder for
/// follow-up events on it.
fn running_app() -> (
    App,
    SessionId,
    RunId,
    impl FnMut(SessionEvent) -> ClientUpdate,
) {
    let mut app = App::new(TuiOptions::default());
    let mut initial = snapshot();
    let session_id = initial.sessions[0].id;
    let run_id = id(0x90, RunId::from_bytes);
    for summary in initial
        .sessions
        .iter_mut()
        .chain(initial.focused.iter_mut().map(|body| &mut body.summary))
    {
        summary.status = SessionStatus::Running;
        summary.active_run_id = Some(run_id);
    }
    app.apply_snapshot(initial);
    let mut sequence = 1;
    let event = move |event: SessionEvent| {
        sequence += 1;
        ClientUpdate::Event(SessionEventEnvelope {
            run_id: Some(run_id),
            occurred_at_ms: sequence,
            ..fixtures::envelope(sequence, session_id, event)
        })
    };
    (app, session_id, run_id, event)
}

#[test]
fn enter_during_a_run_queues_the_draft_and_it_submits_when_the_run_ends() {
    let (mut app, session_id, run_id, mut event) = running_app();
    app.composer.text = "follow up".to_owned();
    let (changed, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();
    assert!(changed);
    assert!(
        requests.is_empty(),
        "nothing is sent while the run is active"
    );
    assert!(app.composer.text.is_empty());
    assert_eq!(
        app.queued_drafts(session_id).collect::<Vec<_>>(),
        ["follow up"]
    );

    // Ctrl-Enter queues explicitly as well; drafts keep order.
    app.composer.text = "second".to_owned();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));
    assert_eq!(
        app.queued_drafts(session_id).collect::<Vec<_>>(),
        ["follow up", "second"]
    );

    // Alt-Up brings the newest back for editing.
    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
    assert_eq!(app.composer.text, "second");
    assert_eq!(
        app.queued_drafts(session_id).collect::<Vec<_>>(),
        ["follow up"]
    );
    app.composer.clear();

    // The run finishes idle: the oldest draft becomes the next run.
    let mut summary = app.sessions[&session_id].summary.clone();
    summary.status = SessionStatus::Idle;
    summary.active_run_id = None;
    let requests = app
        .apply_client_update(event(SessionEvent::RunFinished {
            session: summary,
            run_id,
            outcome: RunOutcome::Completed,
            usage: None,
            context_tokens: None,
        }))
        .into_requests();
    assert!(matches!(
        requests.as_slice(),
        [ClientRequest::Command(CommandRequest {
            command: SessionCommand::SubmitPrompt { input, .. },
            ..
        })] if input.as_slice() == [qq_protocol::InputPart::text("follow up")]
    ));
    assert!(app.queued_drafts(session_id).next().is_none());
}

#[test]
fn esc_twice_cancels_the_active_run_but_once_only_arms() {
    let (mut app, session_id, run_id, _) = running_app();
    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    let (changed, requests) = app.handle_key(esc).split();
    assert!(changed);
    assert!(requests.is_empty());
    assert!(app.status.as_deref().unwrap().contains("Esc again"));

    // Typing disarms.
    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    let (_, requests) = app.handle_key(esc).split();
    assert!(requests.is_empty(), "disarmed by intervening input");

    let (_, requests) = app.handle_key(esc).split();
    assert!(matches!(
        requests.as_slice(),
        [ClientRequest::Command(CommandRequest {
            command: SessionCommand::CancelRun { run_id: cancelled },
            ..
        })] if *cancelled == run_id
    ));
    assert!(app.sessions[&session_id].summary.active_run_id.is_some());

    // Too slow: the arm expires.
    let (_, _) = app.handle_key(esc).split();
    for _ in 0..=ESC_CANCEL_TICKS {
        app.advance_animation();
    }
    let (_, requests) = app.handle_key(esc).split();
    assert!(requests.is_empty());
}

#[test]
fn steer_falls_back_to_queueing_until_the_server_advertises_it() {
    let (mut app, session_id, _, _) = running_app();
    app.composer.text = "go left".to_owned();
    let (_, requests) = app.execute(Command::SteerRun).split();
    assert!(requests.is_empty());
    assert_eq!(
        app.queued_drafts(session_id).collect::<Vec<_>>(),
        ["go left"]
    );
    assert!(
        app.status
            .as_deref()
            .unwrap()
            .contains("does not support steering")
    );

    // A server that steers at boundaries but cannot interrupt: Alt-S
    // queues with its own explanation rather than sending a plain steer
    // the user did not ask for.
    app.apply_client_update(ClientUpdate::Capabilities(std::sync::Arc::new(
        fixtures::capabilities(SteeringCapabilities {
            boundary: true,
            interrupt: false,
            max_pending_per_run: 4,
        }),
    )));
    app.composer.text = "stop".to_owned();
    let (_, requests) = app.handle_key(alt('s')).split();
    assert!(requests.is_empty());
    assert_eq!(
        app.queued_drafts(session_id).collect::<Vec<_>>(),
        ["go left", "stop"]
    );
    assert!(
        app.status
            .as_deref()
            .unwrap()
            .contains("does not support interrupting")
    );
}

fn steering_app() -> (
    App,
    SessionId,
    RunId,
    impl FnMut(SessionEvent) -> ClientUpdate,
) {
    let (mut app, session_id, run_id, event) = running_app();
    app.apply_client_update(ClientUpdate::Capabilities(std::sync::Arc::new(
        fixtures::steering_capabilities(),
    )));
    (app, session_id, run_id, event)
}

#[test]
fn enter_steers_the_active_run_when_the_server_advertises_it() {
    let (mut app, session_id, run_id, _) = steering_app();
    app.composer.text = "also check the tests".to_owned();

    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();
    let [ClientRequest::Command(request)] = requests.as_slice() else {
        panic!("expected one steer command, got {requests:?}");
    };
    assert_eq!(
        request.command,
        SessionCommand::SteerRun {
            run_id,
            input: vec![qq_protocol::InputPart::text("also check the tests")],
            interrupt: false,
        }
    );
    // Nothing is queued locally, the composer is clear, and the text
    // shows as pending until the server's steering row replaces it.
    assert_eq!(app.queued_drafts(session_id).count(), 0);
    assert!(app.composer.text.is_empty());
    assert_eq!(
        app.pending_prompts(session_id).collect::<Vec<_>>(),
        ["also check the tests"]
    );
}

#[test]
fn alt_s_interrupts_and_steers_and_disarms_esc() {
    let (mut app, _, run_id, _) = steering_app();
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.esc_armed_at.is_some());
    app.composer.text = "wrong file".to_owned();

    let (_, requests) = app.handle_key(alt('s')).split();
    let [ClientRequest::Command(request)] = requests.as_slice() else {
        panic!("expected one steer command, got {requests:?}");
    };
    assert_eq!(
        request.command,
        SessionCommand::SteerRun {
            run_id,
            input: vec![qq_protocol::InputPart::text("wrong file")],
            interrupt: true,
        }
    );
    assert!(app.esc_armed_at.is_none());
}

#[test]
fn steer_with_an_empty_draft_or_no_run_sends_nothing() {
    let (mut app, _, _, _) = steering_app();
    let (changed, requests) = app.execute(Command::InterruptRun).split();
    assert!(!changed);
    assert!(requests.is_empty());

    let mut idle = App::new(TuiOptions::default());
    idle.apply_snapshot(snapshot());
    idle.apply_client_update(ClientUpdate::Capabilities(std::sync::Arc::new(
        fixtures::steering_capabilities(),
    )));
    idle.composer.text = "hello".to_owned();
    let (_, requests) = idle.execute(Command::SteerRun).split();
    assert!(requests.is_empty());
    assert!(app.status.is_none());
    assert!(idle.status.as_deref().unwrap().contains("no active run"));
    assert_eq!(idle.composer.text, "hello");

    // Enter on an idle session still submits a new prompt.
    let (_, requests) = idle
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();
    let [ClientRequest::Command(request)] = requests.as_slice() else {
        panic!("expected one submit, got {requests:?}");
    };
    assert!(matches!(
        request.command,
        SessionCommand::SubmitPrompt { .. }
    ));
}

#[test]
fn refused_or_late_steering_returns_the_draft_to_the_composer() {
    let (mut app, session_id, run_id, _) = steering_app();
    app.composer.text = "first".to_owned();
    let (_, requests) = app.execute(Command::SteerRun).split();
    let [ClientRequest::Command(request)] = requests.as_slice() else {
        panic!("expected one steer command");
    };
    app.apply_client_update(ClientUpdate::CommandResult {
        command_id: request.command_id,
        result: Err(ClientFailure::new("too many pending steering messages")),
    });
    assert_eq!(app.composer.text, "first");
    assert_eq!(app.pending_prompts(session_id).count(), 0);
    assert_eq!(
        app.status.as_deref(),
        Some("too many pending steering messages")
    );

    // The run finished before the steer landed: the receipt is a success
    // that applied nothing, so the text comes back with a warning.
    let (_, requests) = app.execute(Command::SteerRun).split();
    let [ClientRequest::Command(request)] = requests.as_slice() else {
        panic!("expected one steer command");
    };
    app.apply_client_update(ClientUpdate::CommandResult {
        command_id: request.command_id,
        result: Ok(qq_protocol::CommandReceipt {
            command_id: request.command_id,
            outcome: CommandOutcome::RunAlreadyFinished {
                run_id,
                outcome: qq_protocol::RunOutcome::Completed,
            },
            committed_through: fixtures::cursor(9),
        }),
    });
    assert_eq!(app.composer.text, "first");
    assert!(app.status.as_deref().unwrap().contains("draft restored"));

    // A steer that was recorded clears the pending row; the transcript
    // row arrives through `steering_queued` like any other message.
    let (_, requests) = app.execute(Command::SteerRun).split();
    let [ClientRequest::Command(request)] = requests.as_slice() else {
        panic!("expected one steer command");
    };
    app.apply_client_update(ClientUpdate::CommandResult {
        command_id: request.command_id,
        result: Ok(qq_protocol::CommandReceipt {
            command_id: request.command_id,
            outcome: CommandOutcome::SteeringQueued {
                run_id,
                message_id: id(0x77, qq_protocol::MessageId::from_bytes),
            },
            committed_through: fixtures::cursor(10),
        }),
    });
    assert!(app.composer.text.is_empty());
    assert_eq!(app.pending_prompts(session_id).count(), 0);
}

fn alt(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::ALT)
}

/// Two warm root sessions; the first is focused. Returns their ids.
fn two_session_app() -> (App, SessionId, SessionId) {
    let mut app = App::new(TuiOptions::default());
    let mut initial = snapshot();
    let other = summary_named(0x42, "other");
    initial.sessions.push(other.clone());
    initial.included.push(body_for(&other, "other body"));
    let first = initial.sessions[0].id;
    app.apply_snapshot(initial);
    assert!(app.sessions[&other.id].is_warm());
    (app, first, other.id)
}

#[test]
fn the_shown_session_is_pinned_warm_while_others_evict() {
    let mut app = App::new(TuiOptions::default());
    let mut initial = snapshot();
    let ids: Vec<SessionId> = (0x60..0x60 + (WARM_BODY_LIMIT as u8) + 2)
        .map(|byte| {
            let summary = summary_named(byte, "s");
            initial.sessions.push(summary.clone());
            summary.id
        })
        .collect();
    app.apply_snapshot(initial);
    for session_id in &ids {
        app.focus_session(*session_id);
        let summary = app.sessions[session_id].summary.clone();
        app.apply_snapshot(WorkspaceSnapshot {
            focused: Some(body_for(&summary, "body")),
            ..snapshot()
        });
    }
    assert!(app.sessions[ids.last().unwrap()].is_warm(), "shown: pinned");
    assert!(!app.sessions[&ids[0]].is_warm(), "oldest unshown evicts");
    let warm = app.sessions.values().filter(|s| s.is_warm()).count();
    assert_eq!(warm, WARM_BODY_LIMIT);
}

#[test]
fn a_body_fetched_for_a_session_the_user_left_is_dropped_without_moving_focus() {
    let (mut app, first, _) = two_session_app();
    let cold = summary_named(0x77, "cold");
    app.apply_snapshot(WorkspaceSnapshot {
        sessions: vec![cold.clone()],
        focused: None,
        ..snapshot()
    });
    let (_, requests) = app.focus_session(cold.id).split();
    assert_eq!(requests.len(), 1, "cold body is requested");
    // The user moves back before the body arrives.
    app.focus_session(first);
    assert_eq!(app.focused(), Some(first));

    let installed = app
        .apply_snapshot(WorkspaceSnapshot {
            focused: Some(body_for(&cold, "arrived")),
            ..snapshot()
        })
        .redraws();
    assert!(!installed, "a late body for an unshown session is stale");
    assert_eq!(
        app.focused(),
        Some(first),
        "focus stays where the user put it"
    );

    // Likewise for a session that was never shown.
    let gone = summary_named(0x78, "gone");
    app.apply_snapshot(WorkspaceSnapshot {
        sessions: vec![gone.clone()],
        focused: None,
        ..snapshot()
    });
    assert!(
        !app.apply_snapshot(WorkspaceSnapshot {
            focused: Some(body_for(&gone, "late")),
            ..snapshot()
        })
        .redraws()
    );
}

#[test]
fn deleting_the_shown_session_moves_to_its_neighbour() {
    let (mut app, first, other) = two_session_app();
    assert_eq!(app.focused(), Some(first));
    let effects = app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
        occurred_at_ms: 2,
        ..fixtures::envelope(2, first, SessionEvent::SessionDeleted { session_id: first })
    }));
    assert!(!app.sessions.contains_key(&first));
    assert_eq!(app.focused(), Some(other));
    // The replacement is warm, so nothing is fetched.
    assert!(effects.requests().next().is_none());
}

#[test]
fn the_mouse_wheel_scrolls_the_transcript_from_anywhere_on_screen() {
    use crossterm::event::MouseEvent;
    let (mut app, _, _) = two_session_app();
    app.update_transcript_viewport(200, 40, false);
    let mouse = |kind, column: u16, row: u16| {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    };
    assert!(
        app.handle_terminal_event(mouse(MouseEventKind::ScrollUp, 10, 5))
            .redraws()
    );
    assert_eq!(app.transcript_scroll_offset(), MOUSE_SCROLL_ROWS);
    // Over the chrome the wheel still moves the transcript.
    app.handle_terminal_event(mouse(MouseEventKind::ScrollUp, 10, 0));
    assert_eq!(app.transcript_scroll_offset(), 2 * MOUSE_SCROLL_ROWS);
    app.handle_terminal_event(mouse(MouseEventKind::ScrollDown, 150, 39));
    assert_eq!(app.transcript_scroll_offset(), MOUSE_SCROLL_ROWS);
}

fn themed_app() -> App {
    let mut app = App::new(TuiOptions {
        settings: Settings::default(),
        model: ModelSelection::default(),
        models: Vec::new(),
        themes: vec![
            crate::Theme::qq(),
            crate::Theme::from_roles("rose-pine", [crate::ThemeColor::Rgb(0xe0, 0xde, 0xf4); 8]),
            crate::Theme::from_roles("mono", [crate::ThemeColor::White; 8]),
        ],
    });
    app.apply_snapshot(snapshot());
    app
}

#[test]
fn the_theme_picker_previews_live_and_esc_restores() {
    let mut app = themed_app();
    assert_eq!(app.theme().name, "qq");
    app.composer.text = "/theme".to_owned();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.mode(), Mode::Themes);
    let generation = app.theme_generation;

    // Down previews the next theme immediately.
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.theme().name, "rose-pine");
    assert_eq!(app.theme_generation, generation + 1);
    // Typing filters and the highlighted theme follows the filter.
    app.handle_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
    assert_eq!(app.filtered_themes().len(), 1);
    assert_eq!(app.theme().name, "mono");
    // Esc puts the original back.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.mode(), Mode::Compose);
    assert_eq!(app.theme().name, "qq");

    // Enter keeps the preview and tells the user how to persist it.
    app.execute(Command::OpenThemes);
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.theme().name, "rose-pine");
    let (status, _) = app.visible_status().expect("info notice");
    assert!(status.contains("theme: \"rose-pine\""), "{status}");
}

#[test]
fn a_single_theme_makes_the_picker_a_notice_instead() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    assert_eq!(app.themes.len(), 1, "the compiled theme is always present");
    app.execute(Command::OpenThemes);
    assert_eq!(app.mode(), Mode::Compose);
    assert!(
        app.visible_status()
            .unwrap()
            .0
            .contains("themes/<name>.ron")
    );
}

#[test]
fn attention_is_requested_only_while_the_terminal_is_unfocused() {
    let (mut app, session_id, run_id, mut event) = running_app();
    let finish = |run_id| SessionEvent::RunFinished {
        session: SessionSummary {
            status: SessionStatus::Idle,
            active_run_id: None,
            ..summary_named(2, "Deploy")
        },
        run_id,
        outcome: RunOutcome::Completed,
        usage: None,
        context_tokens: None,
    };
    let attention = |effects: Effects| {
        effects.into_iter().find_map(|effect| match effect {
            Effect::Attention(attention) => Some(attention),
            _ => None,
        })
    };
    // Focused: nothing to report.
    let effects = app.apply_client_update(event(finish(run_id)));
    assert_eq!(attention(effects), None);

    // Unfocused: a finished run asks for attention with the title.
    app.handle_terminal_event(Event::FocusLost);
    let effects = app.apply_client_update(event(finish(run_id)));
    assert_eq!(
        attention(effects),
        Some(Attention::RunFinished {
            session_title: "Deploy".to_owned()
        })
    );

    // An approval request while unfocused also asks.
    let effects = app.apply_client_update(event(SessionEvent::ToolApprovalRequested {
        tool_call: ToolCallSnapshot {
            run_id,
            call_ordinal: 0,
            provider_call_id: "call".to_owned(),
            state: ToolCallState::AwaitingApproval,
            ..fixtures::tool_call(
                id(0x51, qq_protocol::ToolCallId::from_bytes),
                session_id,
                "shell",
            )
        },
        shell: None,
        edit: None,
    }));
    assert!(matches!(
        attention(effects),
        Some(Attention::ApprovalRequested { .. })
    ));
    // Focused again: silent.
    app.handle_terminal_event(Event::FocusGained);
    let effects = app.apply_client_update(event(finish(run_id)));
    assert_eq!(attention(effects), None);
    assert_eq!(
        Attention::ApprovalRequested {
            session_title: "Deploy".to_owned()
        }
        .summary(),
        "qq: Deploy needs approval"
    );
}

#[test]
fn a_rejected_model_change_or_deletion_is_attributed_to_its_session_not_the_focused_one() {
    let (mut app, first, other) = two_session_app();
    assert_eq!(app.focused(), Some(first));

    // Change `other`'s model while `first` stays focused.
    let request = |effects: Effects| {
        let requests = effects.into_requests();
        let [ClientRequest::Command(request)] = requests.as_slice() else {
            panic!("expected one command")
        };
        request.clone()
    };
    let set = request(app.set_session_model(
        other,
        ModelSelection {
            model: Some("openai/gpt-x".to_owned()),
            ..ModelSelection::default()
        },
    ));
    app.apply_client_update(ClientUpdate::CommandResult {
        command_id: set.command_id,
        result: Err(ClientFailure::new("model rejected")),
    });
    // The failure belongs to `other`, so the focused session shows nothing.
    assert_eq!(
        app.visible_status(),
        None,
        "notice must not leak onto `first`"
    );
    app.focus_session(other);
    assert!(matches!(
        app.visible_status(),
        Some((text, NoticeLevel::Error)) if text.contains("model rejected")
    ));

    // Same for a deletion refused by the server.
    app.focus_session(first);
    let delete = request(app.delete_session(other));
    app.apply_client_update(ClientUpdate::CommandResult {
        command_id: delete.command_id,
        result: Err(ClientFailure::new("delete refused")),
    });
    assert!(
        app.visible_status()
            .is_none_or(|(text, _)| !text.contains("delete refused")),
        "deletion failure must not appear on the focused session"
    );
    app.focus_session(other);
    assert!(matches!(
        app.visible_status(),
        Some((text, NoticeLevel::Error)) if text.contains("delete refused")
    ));

    // Prune is workspace-wide, so its failure lands on the focused session.
    let prune = request(app.prune_sessions());
    app.apply_client_update(ClientUpdate::CommandResult {
        command_id: prune.command_id,
        result: Err(ClientFailure::new("prune refused")),
    });
    assert!(matches!(
        app.visible_status(),
        Some((text, NoticeLevel::Error)) if text.contains("prune refused")
    ));
}

#[test]
fn the_reducer_returns_notices_and_attention_as_effects_instead_of_mutating_them() {
    let (mut app, session_id, run_id, _) = running_app();
    app.handle_terminal_event(Event::FocusLost);
    let mut summary = app.sessions[&session_id].summary.clone();
    summary.status = SessionStatus::Idle;
    summary.active_run_id = None;
    let envelope = SessionEventEnvelope {
        run_id: Some(run_id),
        ..fixtures::envelope(
            2,
            session_id,
            SessionEvent::RunFinished {
                session: summary,
                run_id,
                outcome: RunOutcome::Failed {
                    failure: qq_protocol::RunFailure {
                        message: "provider exploded".to_owned(),
                        kind: qq_protocol::RunFailureKind::Server,
                    },
                },
                usage: None,
                context_tokens: None,
            },
        )
    };

    let effects = app.sessions.reduce_event(
        &envelope,
        qq_client::state::ReduceContext {
            focused: app.focused(),
            attentive: false,
            workspace_id: app.workspace_id,
            capabilities: None,
            models: &app.models,
            caused_by_me: false,
        },
    );

    // Pure: the reducer changed the model but left notice state alone.
    assert_eq!(
        app.sessions[&session_id].summary.status,
        SessionStatus::Idle
    );
    assert_eq!(app.visible_status(), None);
    assert!(effects.iter().any(|effect| matches!(
        effect,
        qq_client::state::StateEffect::Notice { session: Some(id), level: NoticeLevel::Error, text }
            if *id == session_id && text == "provider exploded"
    )));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        qq_client::state::StateEffect::Attention(Attention::RunFinished { .. })
    )));

    // The app applies the notice to its status line and passes attention on
    // to the loop.
    let effects = app.reduce_event(&envelope);
    assert!(matches!(
        app.visible_status(),
        Some((text, NoticeLevel::Error)) if text == "provider exploded"
    ));
    assert!(
        effects
            .into_iter()
            .any(|effect| matches!(effect, Effect::Attention(Attention::RunFinished { .. })))
    );
}

#[test]
fn background_streaming_for_an_unshown_session_does_not_redraw_when_the_sidebar_is_hidden() {
    let (mut app, first, other) = two_session_app();
    assert_eq!(app.focused(), Some(first));
    app.sidebar = Sidebar::Hidden;
    app.handle_terminal_event(Event::Resize(100, 30));
    let run_id = id(0x60, RunId::from_bytes);
    let message_id = id(0x61, MessageId::from_bytes);
    let mut sequence = 1;
    let mut event = |event: SessionEvent| {
        sequence += 1;
        ClientUpdate::Event(SessionEventEnvelope {
            run_id: Some(run_id),
            ..fixtures::envelope(sequence, other, event)
        })
    };
    // Structural changes always redraw: focus, chrome, or attention may move.
    let started = app.apply_client_update(event(SessionEvent::AssistantMessageStarted {
        message: MessageSnapshot {
            run_id,
            state: MessageState::Streaming,
            ..fixtures::message(message_id, other, "")
        },
    }));
    assert!(started.redraws());

    // A text delta for a session shown in no pane, with the sidebar hidden,
    // changes nothing on screen.
    let delta = app.apply_client_update(event(SessionEvent::TextAppended {
        message_id,
        channel: TextChannel::Output,
        text: "more".to_owned(),
    }));
    assert!(!delta.redraws(), "nothing visible changed");
    // The model still advanced.
    assert!(
        app.sessions[&other]
            .messages
            .as_ref()
            .unwrap()
            .iter()
            .any(|message| message.output == "more")
    );

    // With the sidebar showing, the live tail is visible and the delta redraws.
    app.sidebar = Sidebar::Shown;
    let delta = app.apply_client_update(event(SessionEvent::TextAppended {
        message_id,
        channel: TextChannel::Output,
        text: " text".to_owned(),
    }));
    assert!(delta.redraws());

    // And a delta for the shown session redraws regardless.
    app.sidebar = Sidebar::Hidden;
    let shown = app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
        run_id: Some(run_id),
        ..fixtures::envelope(
            99,
            first,
            SessionEvent::TextAppended {
                message_id: id(0x62, MessageId::from_bytes),
                channel: TextChannel::Output,
                text: "x".to_owned(),
            },
        )
    }));
    assert!(shown.redraws());
}

#[test]
fn ctrl_k_opens_the_palette_and_enter_runs_the_highlighted_command() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
    assert!(matches!(
        app.overlay,
        Some(Overlay::Commands { help: false, .. })
    ));

    // Typing filters by title or slash name as a subsequence.
    for character in "thm".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
    }
    let (picker, _) = app.command_picker().unwrap();
    let titles: Vec<_> = picker.filtered().map(|(_, row)| row.spec.title).collect();
    assert!(titles.contains(&"choose a theme"), "{titles:?}");

    // Enter executes the highlighted command; the palette closes first.
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    app.execute(Command::OpenCommands);
    for character in "toggle tool".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
    }
    let before = app.tool_detail;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.overlay.is_none());
    assert_ne!(app.tool_detail, before);
}

#[test]
fn help_opens_from_question_mark_on_an_empty_composer_only() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
    assert!(matches!(
        app.overlay,
        Some(Overlay::Commands { help: true, .. })
    ));
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.overlay.is_none());

    app.composer.text = "what".to_owned();
    app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
    assert!(app.overlay.is_none());
    assert_eq!(app.composer.text, "what?");

    // F1 and /help open the same view.
    app.composer.clear();
    app.handle_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE));
    assert!(matches!(
        app.overlay,
        Some(Overlay::Commands { help: true, .. })
    ));
}

#[test]
fn every_default_chord_reaches_its_command_through_the_table() {
    use crate::commands::{COMMANDS, command_for_key};
    let settings = Settings::default();
    for spec in &COMMANDS {
        for chord in spec.chords {
            let parsed: KeyChord = chord.parse().unwrap();
            let key = parsed.to_event();
            assert_eq!(
                command_for_key(&settings, key),
                Some(spec.command),
                "{chord} should invoke {:?}",
                spec.command
            );
        }
    }
}

#[test]
fn shift_arrows_scroll_the_transcript_without_the_mouse() {
    let mut app = App::new(TuiOptions::default());
    app.update_transcript_viewport(100, 12, false);
    let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::SHIFT));
    assert!(app.handle_terminal_event(key(KeyCode::Up)).redraws());
    assert_eq!(app.transcript_scroll_offset(), MOUSE_SCROLL_ROWS);
    assert!(app.handle_terminal_event(key(KeyCode::Down)).redraws());
    assert_eq!(app.transcript_scroll_offset(), 0);
    // Plain Up still browses history / moves the caret, not the transcript.
    app.handle_terminal_event(Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)));
    assert_eq!(app.transcript_scroll_offset(), 0);
}

#[test]
fn workspace_views_toggle_and_esc_returns_to_the_session_they_replaced() {
    let (mut app, _, other) = two_session_app();
    app.focus_session(other);
    app.execute(Command::ShowAttention);
    assert_eq!(app.view, View::Attention);
    assert_eq!(app.focused(), None, "no session while a view is up");
    // Esc goes back to where the user was, not to the first session.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.focused(), Some(other));
    // The same command twice toggles.
    app.execute(Command::ShowChanges);
    assert_eq!(app.view, View::Changes);
    app.execute(Command::ShowChanges);
    assert_eq!(app.focused(), Some(other));
    // Switching between views keeps the original return point.
    app.execute(Command::ShowChanges);
    app.execute(Command::ShowAttention);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.focused(), Some(other));
}

fn capabilities_with_profiles() -> std::sync::Arc<qq_protocol::ServerCapabilities> {
    let mut capabilities = fixtures::steering_capabilities();
    capabilities.profiles = Some(vec![
        qq_protocol::AgentProfileSummary {
            id: qq_protocol::AgentProfileId::default(),
            model: Some("openai/gpt-test".to_owned()),
            approval_mode: ApprovalMode::Auto,
            pack: None,
        },
        qq_protocol::AgentProfileSummary {
            id: qq_protocol::AgentProfileId::new("reviewer").unwrap(),
            model: None,
            approval_mode: ApprovalMode::ReadOnly,
            pack: Some(qq_protocol::PackSummary {
                id: "review-kit".to_owned(),
                version: "1.0.0".to_owned(),
            }),
        },
    ]);
    std::sync::Arc::new(capabilities)
}

#[test]
fn profile_picker_waits_for_capabilities_and_lists_advertised_profiles() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    app.composer.text = "/profile".to_owned();
    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();
    assert!(requests.is_empty());
    assert!(app.overlay.is_none());
    assert!(
        app.status
            .as_deref()
            .unwrap()
            .contains("capabilities arrive")
    );

    app.apply_client_update(ClientUpdate::Capabilities(capabilities_with_profiles()));
    app.open_profiles();
    let Some(Overlay::Profiles(picker)) = &app.overlay else {
        panic!("expected the profile picker")
    };
    let names: Vec<&str> = picker.items().iter().map(|row| row.id.as_str()).collect();
    assert_eq!(names, ["default", "reviewer"]);
    assert_eq!(picker.items()[1].pack.as_deref(), Some("review-kit@1.0.0"));
    // The cursor starts on the profile in effect: the focused session's.
    assert_eq!(picker.cursor(), 0);
}

#[test]
fn profile_picker_sets_the_focused_idle_session_profile_and_refuses_running_ones() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    app.apply_client_update(ClientUpdate::Capabilities(capabilities_with_profiles()));
    let focused = app.focused().unwrap();
    app.open_profiles();
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();
    let [ClientRequest::Command(request)] = requests.as_slice() else {
        panic!("expected one set-session-profile command")
    };
    assert!(matches!(
        &request.command,
        SessionCommand::SetSessionProfile { session_id, profile }
            if *session_id == focused && profile.as_str() == "reviewer"
    ));
    assert!(app.overlay.is_none());
    // The choice also becomes the default for sessions created next.
    assert_eq!(app.profile.as_str(), "reviewer");

    // The receipt names the profile; the summary update carries it.
    app.apply_client_update(ClientUpdate::CommandResult {
        command_id: request.command_id,
        result: Ok(qq_protocol::CommandReceipt {
            command_id: request.command_id,
            outcome: CommandOutcome::SessionProfileSet {
                session_id: focused,
                profile: qq_protocol::AgentProfileId::new("reviewer").unwrap(),
            },
            committed_through: fixtures::cursor(2),
        }),
    });
    assert_eq!(
        app.status.as_deref(),
        Some("session profile set to reviewer")
    );

    // A running session cannot change profile; nothing is sent.
    let (mut running, _, _, _) = running_app();
    running.apply_client_update(ClientUpdate::Capabilities(capabilities_with_profiles()));
    running.open_profiles();
    running.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    let (_, requests) = running
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();
    assert!(requests.is_empty());
    assert!(
        running
            .status
            .as_deref()
            .unwrap()
            .contains("wait for the run to finish")
    );
}

#[test]
fn profile_chosen_without_a_focused_session_applies_to_the_next_create() {
    let selection = ModelSelection {
        model: Some("openai/gpt-test".to_owned()),
        max_output_tokens: Some(4_096),
        organization: None,
    };
    let mut app = App::new(TuiOptions {
        settings: Settings::default(),
        model: selection.clone(),
        models: Vec::new(),
        themes: Vec::new(),
    });
    let mut empty = snapshot();
    empty.sessions.clear();
    empty.focused = None;
    app.apply_snapshot(empty);
    app.apply_client_update(ClientUpdate::Capabilities(capabilities_with_profiles()));
    app.open_profiles();
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();
    assert!(requests.is_empty());
    assert_eq!(app.profile.as_str(), "reviewer");
    assert_eq!(
        app.status.as_deref(),
        Some("new sessions will use profile reviewer")
    );

    let (_, requests) = app.execute(Command::NewRootSession).split();
    let [ClientRequest::Command(request)] = requests.as_slice() else {
        panic!("expected one create-session command")
    };
    assert!(matches!(
        &request.command,
        SessionCommand::CreateSession { profile, .. } if profile.as_str() == "reviewer"
    ));
}

#[test]
fn a_refreshed_capability_document_updates_the_open_profile_picker() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    app.apply_client_update(ClientUpdate::Capabilities(capabilities_with_profiles()));
    app.open_profiles();
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));

    let mut refreshed = (*capabilities_with_profiles()).clone();
    refreshed
        .profiles
        .as_mut()
        .unwrap()
        .push(qq_protocol::AgentProfileSummary {
            id: qq_protocol::AgentProfileId::new("perf").unwrap(),
            model: None,
            approval_mode: ApprovalMode::Ask,
            pack: None,
        });
    app.apply_client_update(ClientUpdate::Capabilities(std::sync::Arc::new(refreshed)));
    let Some(Overlay::Profiles(picker)) = &app.overlay else {
        panic!("picker stays open across a refresh")
    };
    assert_eq!(picker.items().len(), 3);
    // The cursor follows the highlighted profile, not its position.
    assert_eq!(picker.current().unwrap().id.as_str(), "reviewer");
}

#[test]
fn approval_picker_sets_the_focused_session_mode_and_the_summary_update_lands() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    // Nothing until the capability document arrives.
    app.composer.text = "/approval".to_owned();
    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();
    assert!(requests.is_empty() && app.overlay.is_none());

    app.apply_client_update(ClientUpdate::Capabilities(capabilities_with_profiles()));
    let focused = app.focused().unwrap();
    app.open_approval_modes();
    let Some(Overlay::ApprovalModes(picker)) = &app.overlay else {
        panic!("expected the approval-mode picker")
    };
    let labels: Vec<&str> = picker.items().iter().map(|row| row.label).collect();
    assert_eq!(labels, ["read_only", "ask", "auto", "full"]);
    // The cursor starts on the session's current mode.
    assert_eq!(picker.current().unwrap().mode, ApprovalMode::Auto);

    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();
    let [ClientRequest::Command(request)] = requests.as_slice() else {
        panic!("expected one set-approval-mode command")
    };
    assert!(matches!(
        &request.command,
        SessionCommand::SetApprovalMode { session_id, mode: ApprovalMode::ReadOnly }
            if *session_id == focused
    ));
    assert!(app.overlay.is_none());
    assert_eq!(app.approval_mode, ApprovalMode::ReadOnly);

    app.apply_client_update(ClientUpdate::CommandResult {
        command_id: request.command_id,
        result: Ok(qq_protocol::CommandReceipt {
            command_id: request.command_id,
            outcome: CommandOutcome::ApprovalModeSet {
                session_id: focused,
                mode: ApprovalMode::ReadOnly,
            },
            committed_through: fixtures::cursor(2),
        }),
    });
    assert_eq!(
        app.status.as_deref(),
        Some("session approval mode set to read_only")
    );

    // The published summary is what the session state reads from.
    let mut summary = fixtures::session_summary(focused);
    summary.approval_mode = ApprovalMode::ReadOnly;
    app.apply_client_update(ClientUpdate::Event(fixtures::envelope(
        2,
        focused,
        SessionEvent::SessionUpdated { session: summary },
    )));
    assert_eq!(app.effective_approval_mode(), ApprovalMode::ReadOnly);

    // Picking the mode already in effect sends nothing.
    app.open_approval_modes();
    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();
    assert!(requests.is_empty());
}

#[test]
fn approval_mode_chosen_without_a_focused_session_applies_to_the_next_create() {
    let mut app = App::new(TuiOptions {
        settings: Settings::default(),
        model: ModelSelection {
            model: Some("openai/gpt-test".to_owned()),
            max_output_tokens: Some(4_096),
            organization: None,
        },
        models: Vec::new(),
        themes: Vec::new(),
    });
    let mut empty = snapshot();
    empty.sessions.clear();
    empty.focused = None;
    app.apply_snapshot(empty);
    app.apply_client_update(ClientUpdate::Capabilities(capabilities_with_profiles()));
    app.open_approval_modes();
    // The cursor starts on the default (`auto`); one down is `full`.
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();
    assert!(requests.is_empty());
    assert_eq!(app.approval_mode, ApprovalMode::Full);

    let (_, requests) = app.execute(Command::NewRootSession).split();
    let [ClientRequest::Command(request)] = requests.as_slice() else {
        panic!("expected one create-session command")
    };
    assert!(matches!(
        &request.command,
        SessionCommand::CreateSession {
            approval_mode: ApprovalMode::Full,
            ..
        }
    ));
}

fn capabilities_with_skills() -> std::sync::Arc<qq_protocol::ServerCapabilities> {
    let mut capabilities = fixtures::steering_capabilities();
    capabilities.workspace_tools = Some(qq_protocol::WorkspaceToolCapabilities {
        catalog_digest: qq_protocol::ContentHash::from_bytes([5; 32]),
        exposure: qq_protocol::ToolExposure::Full,
        hosts: Vec::new(),
        excluded_tools: 0,
        skills: qq_protocol::SkillCapabilities {
            digest: qq_protocol::ContentHash::from_bytes([6; 32]),
            indexed: 2,
            disclosed: 2,
            truncated: false,
            entries: vec![
                qq_protocol::SkillSummary {
                    name: "ship".to_owned(),
                    kind: qq_protocol::GuidanceKind::Command,
                    source: ".qq/commands/ship.md".to_owned(),
                    description: "Ship the current branch.".to_owned(),
                    disclosed: true,
                },
                qq_protocol::SkillSummary {
                    name: "qq-verify".to_owned(),
                    kind: qq_protocol::GuidanceKind::Skill,
                    source: ".qq/skills/qq-verify/SKILL.md".to_owned(),
                    description: "Run the gates.".to_owned(),
                    disclosed: true,
                },
            ],
        },
    });
    std::sync::Arc::new(capabilities)
}

#[test]
fn slash_completion_offers_workspace_commands_and_skills_after_client_commands() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    let session_id = app.focused().unwrap();
    // Before the document arrives only client commands complete.
    app.composer.text = "/".to_owned();
    assert_eq!(
        app.filtered_slash_commands().len(),
        qq_protocol::RESERVED_CLIENT_SLASH_COMMANDS.len()
    );

    app.apply_client_update(ClientUpdate::Capabilities(capabilities_with_skills()));
    let names: Vec<String> = app
        .filtered_slash_commands()
        .iter()
        .map(|entry| entry.name.to_string())
        .collect();
    assert_eq!(
        names.len(),
        qq_protocol::RESERVED_CLIENT_SLASH_COMMANDS.len() + 2
    );
    assert_eq!(&names[names.len() - 2..], ["/ship", "/qq-verify"]);

    // Accepting a workspace command leaves it in the composer for arguments.
    app.composer.text = "/shi".to_owned();
    app.slash.select(0);
    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .split();
    assert!(requests.is_empty());
    assert_eq!(app.composer.text, "/ship ");

    // Accepting a skill submits it as the prompt the runtime resolves.
    app.composer.replace("/qq-v".to_owned());
    app.slash.select(0);
    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();
    let [ClientRequest::Command(request)] = requests.as_slice() else {
        panic!("expected one submit-prompt command")
    };
    assert!(matches!(
        &request.command,
        SessionCommand::SubmitPrompt { session_id: id, input, .. }
            if *id == session_id && input[0] == qq_protocol::InputPart::text("/qq-verify")
    ));
    assert!(app.composer.text.is_empty());
}

#[test]
fn skills_picker_lists_indexed_guidance_and_accepts_like_completion() {
    let mut app = App::new(TuiOptions::default());
    app.apply_snapshot(snapshot());
    app.composer.text = "/skills".to_owned();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.overlay.is_none());
    assert!(
        app.status
            .as_deref()
            .unwrap()
            .contains("capabilities arrive")
    );

    app.apply_client_update(ClientUpdate::Capabilities(capabilities_with_skills()));
    app.open_skills();
    let Some(Overlay::Skills(picker)) = &app.overlay else {
        panic!("expected the skills picker")
    };
    assert_eq!(picker.items().len(), 2);
    // Search text covers the source and description too.
    app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
    let (_, requests) = app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .split();
    assert!(requests.is_empty());
    assert!(app.overlay.is_none());
    assert_eq!(app.composer.text, "/ship ");
}
