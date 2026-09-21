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
                session: Box::new(summary(session_id(index), SessionStatus::Running)),
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
        self.app.layout.rail = crate::view::PanePref::Shown;
    }

    /// Force the session sidebar off regardless of width.
    pub fn hide_sidebar(&mut self) {
        self.app.layout.rail = crate::view::PanePref::Hidden;
    }

    /// Force the inspector pane on regardless of width.
    pub fn show_inspector(&mut self) {
        self.app.layout.inspector = crate::view::PanePref::Shown;
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
        model_is_fallback: false,
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
                session: Box::new(summary(session, SessionStatus::Queued)),
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
                session: Box::new(summary(session, SessionStatus::Running)),
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

    #[test]
    fn every_scene_builds_at_every_golden_size_and_paints_every_row() {
        for scene in Scene::ALL {
            for &(width, height) in GOLDEN_SIZES {
                let mut harness = BenchHarness::scene(scene, (width, height));
                let rows = harness.plain_frame();
                assert!(
                    rows.len() <= usize::from(height),
                    "{scene:?} at {width}x{height} produced {} rows",
                    rows.len()
                );
                assert!(
                    rows.iter()
                        .all(|row| row.chars().count() <= usize::from(width)),
                    "{scene:?} at {width}x{height} overflowed a row"
                );
            }
        }
    }
}

/// Markdown covering every block and inline element the transcript lays
/// out. Kept in one place so the golden, the gallery, and the QA fixture
/// all show the same text.
pub const MARKDOWN_GALLERY: &str = "\
First paragraph of prose.

Second paragraph of prose, directly after the first.

# Level one heading

## Level two heading

### Level three heading

1. First numbered item
2. Second numbered item that is deliberately long enough to wrap onto a second row at this width
3. Third numbered item
   - nested bullet under three

- A bullet item that is also deliberately long enough to wrap onto a second physical row here
- Short bullet

- [ ] open task
- [x] done task

> A quote long enough to wrap onto a second row so we can see whether the rail repeats.

Some *emphasis*, some **strong**, some `inline code`, a [link](https://example.com/x), a footnote[^1], and math $x^2$.

[^1]: The footnote body.

```rust
fn main() {
    let x = 1;
    if x > 0 {
        println!(\"{x}\");
    }
}
```

| Role | Default |
| --- | --- |
| text | white |
| muted | dark grey |

---

Tail paragraph.
";

/// The sizes every golden frame is recorded at: a small laptop window, a
/// comfortable editor split, and three full-screen tiers up to a 48-inch
/// display. Widths straddle the responsive breakpoints the layout plan names.
pub const GOLDEN_SIZES: &[(u16, u16)] = &[(80, 24), (120, 40), (200, 60), (320, 90), (480, 120)];

/// One deterministic transcript state for review frames. Each scene is built
/// from protocol events only, so the golden pins what a user would see for
/// the same server output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scene {
    /// Every markdown element, as a completed message.
    MarkdownGallery,
    /// The first minute of a real session: prompt, reads, a failing test,
    /// an edit in flight, and streaming prose.
    GoldenPath,
    /// A finished turn of tool calls with every body expanded.
    ToolsExpanded,
    /// A finished turn of tool calls folded to summary rows.
    ToolsFolded,
    /// An `edit_file` call held for approval with its diff preview.
    Approval,
    /// A completed reasoning block above the run's message.
    Reasoning,
    /// Steering rows at every lifecycle state.
    Steering,
    /// Eight sessions across every rail group: an approval, two streaming,
    /// an unread finish, a child, and quiet idle and done sessions. The rail
    /// follows the tier, so the Compact frame shows the strip.
    Sessions,
}

impl Scene {
    pub const ALL: [Self; 8] = [
        Self::MarkdownGallery,
        Self::GoldenPath,
        Self::ToolsExpanded,
        Self::ToolsFolded,
        Self::Approval,
        Self::Reasoning,
        Self::Steering,
        Self::Sessions,
    ];

    /// The file stem goldens and gallery frames use for this scene.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::MarkdownGallery => "markdown-gallery",
            Self::GoldenPath => "golden-path",
            Self::ToolsExpanded => "tools-expanded",
            Self::ToolsFolded => "tools-folded",
            Self::Approval => "approval",
            Self::Reasoning => "reasoning",
            Self::Steering => "steering",
            Self::Sessions => "sessions",
        }
    }
}

impl BenchHarness {
    /// Build `scene` at `size` with the default options: three sessions so the
    /// sidebar has something to list where the layout shows one.
    #[must_use]
    pub fn scene(scene: Scene, size: (u16, u16)) -> Self {
        Self::scene_with_options(scene, size, TuiOptions::default())
    }

    /// `scene`, with the TUI options (themes, settings) supplied by the caller.
    #[must_use]
    pub fn scene_with_options(scene: Scene, size: (u16, u16), options: TuiOptions) -> Self {
        let mut harness = Self::with_options(size, 3, 0, options);
        match scene {
            Scene::MarkdownGallery => {
                harness.completed_turn("show me every markdown element", MARKDOWN_GALLERY);
            }
            Scene::GoldenPath => {
                let message = harness.golden_path();
                harness.append(0, message, " More.");
            }
            Scene::ToolsExpanded => {
                harness.completed_turn("clean up the renderer", "Done. Every file compiles.");
                harness.add_tool_calls(6);
                harness.expand_every_tool();
            }
            Scene::ToolsFolded => {
                harness.completed_turn("clean up the renderer", "Done. Every file compiles.");
                harness.add_tool_calls(6);
                harness.fold_tools();
            }
            Scene::Approval => {
                harness.completed_turn(
                    "fix the off-by-one in wrap.rs",
                    "I'll patch the loop bound.",
                );
                harness.request_edit_approval(
                    "crates/qq-tui/src/view/wrap.rs",
                    "@@ -40,3 +40,3 @@\n     let mut column = 0;\n-    while column <= width {\n+    while column < width {\n         column += 1;",
                );
            }
            Scene::Reasoning => {
                harness.reasoned_turn(
                    "why does the test flake?",
                    "First consider the callers.\n\nThen the tests: the sleep races the clock.",
                    "The test slept on wall-clock time; pausing the runtime clock removes the race.",
                );
            }
            Scene::Steering => {
                harness.steering_turn();
            }
            Scene::Sessions => {
                harness.completed_turn("plan the release", "Drafted the checklist.");
                harness.many_agents();
            }
        }
        harness
    }

    /// The current frame as plain rows, one per rendered row, with no styling
    /// and trailing spaces trimmed. Laid out at the same clamped size `draw`
    /// uses, so the golden pins what a terminal of `size` receives. Goldens
    /// compare this; styles are asserted separately so a palette change does
    /// not move every golden.
    pub fn plain_frame(&mut self) -> Vec<String> {
        let (width, height) = crate::view::render_size(self.size);
        let frame = self.renderer.frame_and_commit(&mut self.app, width, height);
        frame
            .iter()
            .map(|line| {
                let mut row = " ".repeat(line.indent);
                row.extend(line.spans.iter().map(|span| span.text.as_str()));
                let trimmed = row.trim_end().len();
                row.truncate(trimmed);
                row
            })
            .collect()
    }

    /// The last frame as the exact bytes a terminal would receive for a full
    /// repaint, for viewing with `cat` in a real terminal.
    pub fn ansi_frame(&mut self) -> Vec<u8> {
        self.draw_full()
    }

    /// Select the theme named `name` from the options, if present. Returns
    /// whether it was found.
    pub fn select_theme(&mut self, name: &str) -> bool {
        match self.app.themes.iter().position(|theme| theme.name == name) {
            Some(index) => {
                if self.app.theme != index {
                    self.app.theme = index;
                    self.app.theme_generation += 1;
                }
                true
            }
            None => false,
        }
    }

    /// Seven more sessions beside the focused one so every rail group has
    /// members: `Deploy helper` (a child of session 0) holds a `shell`
    /// approval; `Survey callers` and `Write tests` stream text; `Refactor`
    /// finished unseen with spend recorded; `Notes` and `Scratch` are idle;
    /// `Migrate` finished and was seen. Titles and spend are fixed so the
    /// frame is the same every run.
    fn many_agents(&mut self) {
        let parent = session_id(0);
        let titled = |index: u8, title: &str, status: SessionStatus, parent_id| {
            let mut summary = summary(session_id(index), status);
            summary.title = title.to_owned();
            summary.parent_id = parent_id;
            summary.updated_at_ms = u64::from(index);
            summary
        };
        let child = titled(1, "Deploy helper", SessionStatus::Running, Some(parent));
        let survey = titled(2, "Survey callers", SessionStatus::Running, None);
        let tests = titled(3, "Write tests", SessionStatus::Running, None);
        let refactor = titled(4, "Refactor", SessionStatus::Running, None);
        let notes = titled(5, "Notes", SessionStatus::Idle, None);
        let scratch = titled(6, "Scratch", SessionStatus::Idle, None);
        let mut migrate = titled(7, "Migrate", SessionStatus::Idle, None);
        migrate.last_outcome = Some(qq_protocol::RunOutcome::Completed);
        migrate.estimated_cost_usd_nanos = Some(40_000_000);
        for session in [
            &child, &survey, &tests, &refactor, &notes, &scratch, &migrate,
        ] {
            let index = session.id.as_bytes()[15];
            self.apply(
                index,
                SessionEvent::SessionCreated {
                    session: Box::new(session.clone()),
                },
            );
        }
        // Session 1 waits on a shell approval under its spawn.
        let mut call_id = [0x50; 16];
        call_id[15] = 0x61;
        self.apply(
            1,
            SessionEvent::ToolApprovalRequested {
                tool_call: ToolCallSnapshot {
                    run_id: run_id(1),
                    arguments: r#"{"command":"rm -rf build"}"#.to_owned(),
                    state: qq_protocol::ToolCallState::AwaitingApproval,
                    ..fixtures::tool_call(ToolCallId::from_bytes(call_id), session_id(1), "shell")
                },
                shell: None,
                edit: None,
                question: None,
                fetch: None,
            },
        );
        // Sessions 2 and 3 stream prose; the rail shows the tails.
        for (session, text) in [
            (survey, "Found twelve call sites across three crates"),
            (tests, "Adding a regression test for the reconnect path"),
        ] {
            let index = session.id.as_bytes()[15];
            let message = assistant_message(session.id, 200 + index, "");
            let message_id = message.id;
            self.apply(
                index,
                SessionEvent::RunStarted {
                    session: Box::new(session),
                    run_id: run_id(index),
                    plan: None,
                },
            );
            self.apply(
                index,
                SessionEvent::RunActivityChanged {
                    run_id: run_id(index),
                    activity: RunActivity::GeneratingResponse,
                },
            );
            self.apply(index, SessionEvent::AssistantMessageStarted { message });
            self.append(index, message_id, text);
        }
        // Session 4 finished while unfocused: one unread finish, with spend.
        let mut finished = refactor;
        finished.status = SessionStatus::Idle;
        finished.active_run_id = None;
        finished.last_outcome = Some(qq_protocol::RunOutcome::Completed);
        finished.estimated_cost_usd_nanos = Some(120_000_000);
        self.apply(
            4,
            SessionEvent::RunStarted {
                session: Box::new(titled(4, "Refactor", SessionStatus::Running, None)),
                run_id: run_id(4),
                plan: None,
            },
        );
        self.apply(
            4,
            SessionEvent::RunFinished {
                session: Box::new(finished),
                run_id: run_id(4),
                outcome: qq_protocol::RunOutcome::Completed,
                usage: None,
                context_tokens: None,
                final_output: None,
            },
        );
    }

    /// One completed exchange in session 0: the user's `prompt`, then a
    /// finished assistant message with `output`.
    fn completed_turn(&mut self, prompt: &str, output: &str) {
        let session = session_id(0);
        let run_id = run_id(0);
        let mut user = assistant_message(session, 0x10, prompt);
        user.role = qq_protocol::MessageRole::User;
        user.state = MessageState::Complete;
        user.turn_ordinal = 0;
        self.apply(
            0,
            SessionEvent::PromptQueued {
                session: Box::new(summary(session, SessionStatus::Queued)),
                message: user,
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
                session: Box::new(summary(session, SessionStatus::Running)),
                run_id,
                plan: None,
            },
        );
        let mut message = assistant_message(session, 0x11, "");
        message.turn_ordinal = 1;
        let id = message.id;
        self.apply(0, SessionEvent::AssistantMessageStarted { message });
        self.apply(
            0,
            SessionEvent::TextAppended {
                message_id: id,
                channel: TextChannel::Output,
                text: output.to_owned(),
            },
        );
        self.apply(
            0,
            SessionEvent::RunFinished {
                session: Box::new(summary(session, SessionStatus::Idle)),
                run_id,
                outcome: qq_protocol::RunOutcome::Completed,
                usage: None,
                context_tokens: None,
                final_output: None,
            },
        );
    }

    /// Hold an `edit_file` call on `path` for approval with `diff` as its
    /// preview, as the server does before a mutating tool runs.
    fn request_edit_approval(&mut self, path: &str, diff: &str) {
        let session = session_id(0);
        let mut id = [0x50; 16];
        id[15] = 0x77;
        let tool_call = ToolCallSnapshot {
            run_id: run_id(0),
            turn_ordinal: 2,
            call_ordinal: 0,
            arguments: format!(r#"{{"path":"{path}"}}"#),
            state: qq_protocol::ToolCallState::AwaitingApproval,
            ..fixtures::tool_call(ToolCallId::from_bytes(id), session, "edit_file")
        };
        self.apply(
            0,
            SessionEvent::RunStarted {
                session: Box::new(summary(session, SessionStatus::Running)),
                run_id: run_id(0),
                plan: None,
            },
        );
        self.apply(
            0,
            SessionEvent::ToolApprovalRequested {
                tool_call,
                shell: None,
                edit: Some(qq_protocol::EditPreview {
                    path: path.to_owned(),
                    diff: diff.to_owned(),
                }),
                question: None,
                fetch: None,
            },
        );
    }

    /// A completed turn whose run carried a reasoning summary before the
    /// assistant message.
    fn reasoned_turn(&mut self, prompt: &str, reasoning: &str, output: &str) {
        let session = session_id(0);
        let run_id = run_id(0);
        let mut user = assistant_message(session, 0x10, prompt);
        user.role = qq_protocol::MessageRole::User;
        user.state = MessageState::Complete;
        user.turn_ordinal = 0;
        self.apply(
            0,
            SessionEvent::PromptQueued {
                session: Box::new(summary(session, SessionStatus::Queued)),
                message: user,
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
                session: Box::new(summary(session, SessionStatus::Running)),
                run_id,
                plan: None,
            },
        );
        let kind = qq_protocol::ReasoningKind::Summary;
        self.apply(0, SessionEvent::ReasoningStarted { run_id, kind });
        self.apply(
            0,
            SessionEvent::ReasoningDelta {
                run_id,
                kind,
                text: reasoning.to_owned(),
            },
        );
        self.apply(0, SessionEvent::ReasoningCompleted { run_id, kind });
        let mut message = assistant_message(session, 0x11, "");
        message.turn_ordinal = 1;
        let id = message.id;
        self.apply(0, SessionEvent::AssistantMessageStarted { message });
        self.apply(
            0,
            SessionEvent::TextAppended {
                message_id: id,
                channel: TextChannel::Output,
                text: output.to_owned(),
            },
        );
        self.apply(
            0,
            SessionEvent::RunFinished {
                session: Box::new(summary(session, SessionStatus::Idle)),
                run_id,
                outcome: qq_protocol::RunOutcome::Completed,
                usage: None,
                context_tokens: None,
                final_output: None,
            },
        );
    }

    /// A running session with one model turn followed by steering messages
    /// in the pending, applied, and late states.
    fn steering_turn(&mut self) {
        let session = session_id(0);
        let run_id = run_id(0);
        self.apply(
            0,
            SessionEvent::RunStarted {
                session: Box::new(summary(session, SessionStatus::Running)),
                run_id,
                plan: None,
            },
        );
        let mut turn = assistant_message(session, 0x20, "the model turn");
        turn.turn_ordinal = 1;
        turn.state = MessageState::Complete;
        self.apply(0, SessionEvent::AssistantMessageStarted { message: turn });
        let steer = |ordinal: u8, text: &str| {
            let mut message = assistant_message(session, 0x20 + ordinal, text);
            message.turn_ordinal = u32::from(ordinal) + 1;
            message.role = qq_protocol::MessageRole::User;
            message.state = MessageState::Queued;
            message.steering = true;
            message
        };
        let pending = steer(1, "pending steer: also check the tests");
        let applied = steer(2, "applied steer: prefer edit_file");
        let late = steer(3, "late steer: never mind");
        let (applied_id, late_id) = (applied.id, late.id);
        for message in [pending, applied, late] {
            self.apply(0, SessionEvent::SteeringQueued { run_id, message });
        }
        self.apply(
            0,
            SessionEvent::SteeringApplied {
                run_id,
                message_id: applied_id,
                turn_ordinal: 3,
            },
        );
        self.apply(
            0,
            SessionEvent::SteeringSuperseded {
                run_id,
                message_id: late_id,
            },
        );
    }
}
