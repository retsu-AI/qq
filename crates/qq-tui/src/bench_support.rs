//! Deterministic fixtures and a driver for the render benchmark.
//!
//! Enabled by the `bench-support` feature so `benches/render.rs` can drive the
//! crate-private `App` and `FrameRenderer` without widening the public API.
//! Nothing here is stable and nothing here should be used outside benchmarks.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use qq_protocol::{
    MessageId, MessageSnapshot, MessageState, RunActivity, RunId, SessionEvent, SessionId,
    SessionSnapshot, SessionStatus, SessionSummary, TextChannel, ToolCallId, ToolCallSnapshot,
    WorkspaceSnapshot,
};

use crate::{ClientUpdate, TuiOptions, app::App, fixtures, view::FrameRenderer};

/// One TUI instance driven directly, without a terminal or client transport.
pub struct BenchHarness {
    app: App,
    renderer: FrameRenderer,
    size: (u16, u16),
    next_sequence: u64,
}

/// Prose long enough to wrap and exercise markdown, but short enough to keep
/// every message under the full-markdown cache threshold.
const PARAGRAPH: &str = "The renderer must keep steady-state frames cheap: completed \
messages are cached per width, so a frame with no new content should touch only the \
chrome and the row diff. `inline code`, **emphasis**, and a list:\n\n- first item\n- second \
item\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n";

impl BenchHarness {
    /// A workspace with `sessions` root sessions. The first is focused and has
    /// `messages` completed assistant messages loaded; the rest are summaries.
    #[must_use]
    pub fn new(size: (u16, u16), sessions: u8, messages: u8) -> Self {
        Self::with_options(size, sessions, messages, TuiOptions::default())
    }

    /// `new`, with the TUI options (themes, settings) supplied by the caller.
    #[must_use]
    pub fn with_options(size: (u16, u16), sessions: u8, messages: u8, options: TuiOptions) -> Self {
        assert!(sessions >= 1, "at least one session is required");
        let summaries: Vec<SessionSummary> = (0..sessions)
            .map(|index| summary(session_id(index), SessionStatus::Idle))
            .collect();
        let focused = SessionSnapshot {
            messages: (0..messages)
                .map(|index| {
                    let mut message = assistant_message(session_id(0), index, PARAGRAPH);
                    message.state = MessageState::Complete;
                    message
                })
                .collect(),
            ..fixtures::session_snapshot(summaries[0].clone())
        };
        let mut app = App::new(options);
        app.apply_client_update(ClientUpdate::Snapshot(WorkspaceSnapshot {
            sessions: summaries,
            focused: Some(focused),
            ..fixtures::workspace_snapshot()
        }));
        app.apply_client_update(ClientUpdate::Connection(crate::ConnectionState::Live));
        Self {
            app,
            renderer: FrameRenderer::default(),
            size,
            next_sequence: 2,
        }
    }

    /// Start a streaming assistant message in session `index` and return its
    /// id. Session 0 is the focused session.
    pub fn start_stream(&mut self, index: u8) -> MessageId {
        let message = assistant_message(session_id(index), 200 + index, "");
        let id = message.id;
        let run_id = message.run_id;
        self.apply(
            index,
            SessionEvent::RunStarted {
                session: summary(session_id(index), SessionStatus::Running),
                run_id,
                plan: None,
            },
        );
        self.apply(
            index,
            SessionEvent::RunActivityChanged {
                run_id,
                activity: RunActivity::GeneratingResponse,
            },
        );
        self.apply(index, SessionEvent::AssistantMessageStarted { message });
        id
    }

    /// Append `text` to a streaming message in session `index`.
    pub fn append(&mut self, index: u8, message_id: MessageId, text: &str) -> bool {
        self.apply(
            index,
            SessionEvent::TextAppended {
                message_id,
                channel: TextChannel::Output,
                text: text.to_owned(),
            },
        )
    }

    /// Type one character into the composer.
    pub fn keystroke(&mut self, character: char) -> bool {
        self.app
            .handle_terminal_event(Event::Key(KeyEvent::new(
                KeyCode::Char(character),
                KeyModifiers::NONE,
            )))
            .redraws()
    }

    /// Build and diff one frame, returning the terminal bytes it would emit.
    pub fn draw(&mut self) -> Vec<u8> {
        self.renderer
            .draw(&mut self.app, self.size)
            .expect("in-memory frame rendering cannot fail")
    }

    /// Draw every row regardless of the previous frame, as after a resize.
    pub fn draw_full(&mut self) -> Vec<u8> {
        self.renderer.invalidate();
        self.draw()
    }

    /// Install any finished off-tick highlight results, as the event loop
    /// would between frames. Returns how many were applied.
    pub fn apply_finished_highlights(&mut self) -> usize {
        let mut applied = 0;
        while let Some(result) = self.renderer.highlighter.try_next() {
            if self.renderer.apply_highlight(result) {
                applied += 1;
            }
        }
        applied
    }

    /// Force the session sidebar on regardless of width.
    pub fn show_sidebar(&mut self) {
        self.app.sidebar = crate::app::Sidebar::Shown;
    }

    /// Force the session sidebar off regardless of width.
    pub fn hide_sidebar(&mut self) {
        self.app.sidebar = crate::app::Sidebar::Hidden;
    }

    /// Load `messages` completed assistant messages into session `index`
    /// through an included body, as the client's pre-warm does.
    pub fn warm_session(&mut self, index: u8, messages: u8) {
        let body = SessionSnapshot {
            messages: (0..messages)
                .map(|row| {
                    let mut message = assistant_message(session_id(index), row, PARAGRAPH);
                    message.state = MessageState::Complete;
                    message
                })
                .collect(),
            ..fixtures::session_snapshot(summary(session_id(index), SessionStatus::Idle))
        };
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        self.app
            .apply_client_update(ClientUpdate::Snapshot(WorkspaceSnapshot {
                cursor: fixtures::cursor(sequence),
                sessions: Vec::new(),
                focused: None,
                included: vec![body],
                ..fixtures::workspace_snapshot()
            }));
    }

    /// Draw until every scheduled highlight has landed, as the event loop
    /// does between frames. Steady-state samples should reflect the
    /// highlighted cache, not a stream of upgrade frames.
    pub fn settle_highlights(&mut self) {
        loop {
            let applied = self.apply_finished_highlights();
            if applied > 0 {
                self.draw();
            }
            if !self.highlights_pending() && applied == 0 {
                self.draw();
                if !self.highlights_pending() {
                    return;
                }
            }
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
    }

    /// Whether highlight jobs are still running.
    pub fn highlights_pending(&self) -> bool {
        self.renderer.highlighter.in_flight() > 0
    }

    fn apply(&mut self, index: u8, event: SessionEvent) -> bool {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        let mut envelope = fixtures::envelope(sequence, session_id(index), event);
        envelope.run_id = Some(run_id(index));
        envelope.occurred_at_ms = sequence;
        self.app
            .apply_client_update(ClientUpdate::Event(envelope))
            .redraws()
    }
}

fn session_id(index: u8) -> SessionId {
    let mut bytes = [0x10; 16];
    bytes[15] = index;
    SessionId::from_bytes(bytes)
}

fn run_id(index: u8) -> RunId {
    let mut bytes = [0x20; 16];
    bytes[15] = index;
    RunId::from_bytes(bytes)
}

fn summary(id: SessionId, status: SessionStatus) -> SessionSummary {
    SessionSummary {
        title: format!("Session {}", id.as_bytes()[15]),
        status,
        ..fixtures::session_summary(id)
    }
}

fn assistant_message(session_id: SessionId, index: u8, output: &str) -> MessageSnapshot {
    let mut bytes = [0x30; 16];
    bytes[14] = session_id.as_bytes()[15];
    bytes[15] = index;
    MessageSnapshot {
        run_id: run_id(session_id.as_bytes()[15]),
        state: MessageState::Streaming,
        created_at_ms: u64::from(index),
        ..fixtures::message(MessageId::from_bytes(bytes), session_id, output)
    }
}

impl BenchHarness {
    /// A workspace with `sessions` root sessions listed in the sidebar; only
    /// the first is warm. Sessions beyond 255 are not needed: the sidebar
    /// cost is per visible row and the store cost is per session.
    #[must_use]
    pub fn with_sessions(size: (u16, u16), sessions: u8) -> Self {
        Self::new(size, sessions, 8)
    }

    /// Load `count` completed tool calls into the focused session's most
    /// recent run: a mix of reads, edits, and shell commands with results,
    /// as an agent's working turn looks.
    pub fn add_tool_calls(&mut self, count: u8) {
        let run_id = run_id(0);
        for index in 0..count {
            let (name, arguments, result) = match index % 3 {
                0 => (
                    "read_file",
                    format!(r#"{{"path":"crates/qq-tui/src/file_{index}.rs"}}"#),
                    "fn main() {}\n".repeat(20),
                ),
                1 => (
                    "edit_file",
                    format!(r#"{{"path":"crates/qq-tui/src/file_{index}.rs","content":"x"}}"#),
                    "edited".to_owned(),
                ),
                _ => (
                    "shell",
                    format!(r#"{{"command":"cargo test -p crate_{index}"}}"#),
                    "test result: ok. 12 passed\n".to_owned(),
                ),
            };
            let mut id = [0x50; 16];
            id[15] = index;
            let call = ToolCallSnapshot {
                run_id,
                turn_ordinal: u32::from(index) + 1,
                call_ordinal: 0,
                arguments,
                result: Some(result),
                ..fixtures::tool_call(ToolCallId::from_bytes(id), session_id(0), name)
            };
            self.apply(0, SessionEvent::ToolCallFinished { tool_call: call });
        }
    }

    /// Open the session picker, then dismiss it. Measures overlay open and
    /// close including any cache work they trigger.
    pub fn open_and_close_session_picker(&mut self) {
        self.app.execute(crate::commands::Command::OpenSessions);
        black_box_draw(self);
        self.app
            .handle_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        black_box_draw(self);
    }

    /// Resize the terminal by `delta` columns and draw the full frame.
    pub fn resize(&mut self, delta: i16) -> Vec<u8> {
        self.size.0 = self.size.0.saturating_add_signed(delta).max(40);
        self.draw_full()
    }

    /// Fold quiet finished tool blocks to one row.
    pub fn fold_tools(&mut self) {
        self.app.tool_detail = crate::app::ToolDetail::Folded;
    }

    /// Expand every tool call's body, as a user reviewing a whole turn would.
    pub fn expand_every_tool(&mut self) {
        let ids: Vec<_> = self
            .app
            .sessions
            .get(&session_id(0))
            .and_then(|session| session.tool_calls.as_ref())
            .map(|calls| calls.iter().map(|call| call.id).collect())
            .unwrap_or_default();
        self.app.expanded_tool_calls.extend(ids);
    }
}

fn black_box_draw(harness: &mut BenchHarness) {
    std::hint::black_box(harness.draw());
}

impl BenchHarness {
    /// The first minute of a real session, as events: the user's prompt is
    /// accepted, the run starts, the model reads a file, runs the tests, and
    /// is mid-way through an edit while streaming its explanation. Session 0
    /// must be empty. Returns the streaming message id so callers can keep
    /// appending.
    pub fn golden_path(&mut self) -> MessageId {
        let session = session_id(0);
        let run_id = run_id(0);
        let mut prompt =
            assistant_message(session, 0x10, "make the sse reconnect test deterministic");
        prompt.role = qq_protocol::MessageRole::User;
        prompt.state = MessageState::Complete;
        prompt.turn_ordinal = 0;
        self.apply(
            0,
            SessionEvent::PromptQueued {
                session: summary(session, SessionStatus::Queued),
                message: prompt,
                run: Box::new(fixtures::run(
                    run_id,
                    session,
                    qq_protocol::RunStatus::Queued,
                )),
                queue_position: 0,
            },
        );
        self.apply(
            0,
            SessionEvent::RunStarted {
                session: summary(session, SessionStatus::Running),
                run_id,
                plan: None,
            },
        );
        let call = |index: u8, name: &str, arguments: &str, turn: u32| {
            let mut id = [0x50; 16];
            id[15] = index;
            ToolCallSnapshot {
                run_id,
                turn_ordinal: turn,
                call_ordinal: 0,
                arguments: arguments.to_owned(),
                state: qq_protocol::ToolCallState::Running,
                ..fixtures::tool_call(ToolCallId::from_bytes(id), session, name)
            }
        };
        let read = call(
            1,
            "read_file",
            r#"{"path":"crates/qq-client/src/sse.rs"}"#,
            1,
        );
        self.apply(
            0,
            SessionEvent::ToolCallStarted {
                tool_call: read.clone(),
            },
        );
        self.apply(
            0,
            SessionEvent::ToolCallFinished {
                tool_call: ToolCallSnapshot {
                    state: qq_protocol::ToolCallState::Completed,
                    result: Some("fn reconnect() {}\n".repeat(412)),
                    ..read
                },
            },
        );
        let test = call(
            2,
            "shell",
            r#"{"command":"cargo test -p qq-client reconnect"}"#,
            2,
        );
        self.apply(
            0,
            SessionEvent::ToolCallStarted {
                tool_call: test.clone(),
            },
        );
        self.apply(
            0,
            SessionEvent::ToolCallFinished {
                tool_call: ToolCallSnapshot {
                    state: qq_protocol::ToolCallState::Completed,
                    result: Some(
                        "shell exit=101 elapsed=3.2 bytes=97\nrunning 4 tests\ntest reconnect_replays ... FAILED\ntest result: FAILED. 3 passed; 1 failed\n"
                            .to_owned(),
                    ),
                    ..test
                },
            },
        );
        let edit = call(
            3,
            "edit_file",
            r#"{"path":"crates/qq-client/src/sse.rs"}"#,
            3,
        );
        self.apply(0, SessionEvent::ToolCallStarted { tool_call: edit });
        let mut message = assistant_message(session, 0x11, "");
        message.turn_ordinal = 4;
        let id = message.id;
        self.apply(0, SessionEvent::AssistantMessageStarted { message });
        self.apply(
            0,
            SessionEvent::TextAppended {
                message_id: id,
                channel: qq_protocol::TextChannel::Output,
                text: "The test slept on wall-clock time; pausing the runtime clock removes the race. Applying the edit now.".to_owned(),
            },
        );
        id
    }
}

impl BenchHarness {
    /// Scroll the transcript by one wheel notch toward older rows, as the
    /// mouse would. Returns whether the viewport moved.
    pub fn wheel_up(&mut self) -> bool {
        self.app
            .handle_terminal_event(Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::ScrollUp,
                column: 10,
                row: 10,
                modifiers: KeyModifiers::NONE,
            }))
            .redraws()
    }

    /// Scroll back toward the live tail by one notch.
    pub fn wheel_down(&mut self) -> bool {
        self.app
            .handle_terminal_event(Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::ScrollDown,
                column: 10,
                row: 10,
                modifiers: KeyModifiers::NONE,
            }))
            .redraws()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_golden_path_draws_the_prompt_rows_and_streaming_text() {
        let mut harness = BenchHarness::new((110, 24), 3, 0);
        harness.hide_sidebar();
        let message = harness.golden_path();
        assert!(harness.append(0, message, " More."));
        let frame = String::from_utf8_lossy(&harness.draw_full()).into_owned();
        for needle in [
            "make the sse reconnect test deterministic",
            "Read",
            "412 lines",
            "exit 101",
            "Edit",
            "running",
            "pausing the runtime clock",
            "More.",
        ] {
            assert!(frame.contains(needle), "{needle} missing from frame");
        }
    }
}
