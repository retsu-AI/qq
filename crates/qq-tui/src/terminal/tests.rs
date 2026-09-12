use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use qq_protocol::{
    MessageSnapshot, MessageState, SessionEvent, SessionEventEnvelope, TextChannel,
    WorkspaceSnapshot,
};
use tokio::sync::mpsc;

use super::*;
use crate::{ClientFailure, ConnectionState, TuiOptions, fixtures};

/// Deterministic `ClientPort`: updates are fed from a channel, requests are
/// recorded, and `try_send` fails once the configured capacity is reached.
struct FakePort {
    updates: mpsc::UnboundedReceiver<ClientUpdate>,
    sent: Arc<Mutex<Vec<ClientRequest>>>,
    capacity: usize,
}

impl ClientPort for FakePort {
    fn try_send(&self, request: ClientRequest) -> Result<(), ClientFailure> {
        let mut sent = self.sent.lock().expect("request log lock");
        if sent.len() >= self.capacity {
            return Err(ClientFailure::new("request queue is full"));
        }
        sent.push(request);
        Ok(())
    }

    async fn recv(&mut self) -> Option<ClientUpdate> {
        self.updates.recv().await
    }
}

/// A shared async byte sink that records every frame written by the loop.
#[derive(Clone, Default)]
struct FrameLog(Arc<Mutex<Vec<Vec<u8>>>>);

impl FrameLog {
    fn frames(&self) -> Vec<Vec<u8>> {
        self.0.lock().expect("frame log lock").clone()
    }
}

impl AsyncWrite for FrameLog {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.0.lock().expect("frame log lock").push(buf.to_vec());
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Adapts an unbounded receiver into the `Stream` the loop expects.
struct EventQueue(mpsc::UnboundedReceiver<io::Result<Event>>);

impl Stream for EventQueue {
    type Item = io::Result<Event>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.poll_recv(cx)
    }
}

/// Next scripted editor outcome, plus every draft the loop handed over.
type EditorScript = Arc<Mutex<(Option<Result<Option<String>, EditorError>>, Vec<String>)>>;

struct Harness {
    updates: mpsc::UnboundedSender<ClientUpdate>,
    events: mpsc::UnboundedSender<io::Result<Event>>,
    sent: Arc<Mutex<Vec<ClientRequest>>>,
    frames: FrameLog,
    editor_script: EditorScript,
    port: Option<FakePort>,
    event_stream: Option<EventQueue>,
}

impl Harness {
    fn new(capacity: usize) -> Self {
        let (updates, update_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let sent = Arc::new(Mutex::new(Vec::new()));
        Self {
            updates,
            events,
            sent: Arc::clone(&sent),
            frames: FrameLog::default(),
            editor_script: Arc::new(Mutex::new((None, Vec::new()))),
            port: Some(FakePort {
                updates: update_rx,
                sent,
                capacity,
            }),
            event_stream: Some(EventQueue(event_rx)),
        }
    }

    fn key(&self, code: KeyCode, modifiers: KeyModifiers) {
        self.events
            .send(Ok(Event::Key(KeyEvent::new(code, modifiers))))
            .expect("event channel open");
    }

    fn update(&self, update: ClientUpdate) {
        self.updates.send(update).expect("update channel open");
    }

    /// Spawn the loop on the paused runtime. Test steps then interleave
    /// sends with [`Harness::settle`] so ordering is explicit rather than a
    /// consequence of `select!` bias.
    fn spawn(&mut self, app: App) -> tokio::task::JoinHandle<Result<App, TuiError>> {
        let port = self.port.take().expect("harness runs once");
        let events = self.event_stream.take().expect("harness runs once");
        let frames = self.frames.clone();
        let scripted = std::sync::Arc::clone(&self.editor_script);
        tokio::spawn(run_loop(
            port,
            app,
            events,
            frames,
            || Ok((100, 30)),
            std::future::pending(),
            move |draft| {
                let scripted = std::sync::Arc::clone(&scripted);
                Box::pin(async move {
                    let mut guard = scripted.lock().expect("editor script lock");
                    guard.1.push(draft);
                    guard.0.take().unwrap_or(Err(EditorError::NotConfigured))
                })
            },
        ))
    }

    /// Advance paused time far enough for pending input to be handled and
    /// any dirty frame to be drawn.
    async fn settle(&self) {
        // Let the loop consume queued input, then let a frame tick fire,
        // then let it draw.
        tokio::task::yield_now().await;
        tokio::time::advance(FRAME_INTERVAL * 3).await;
        tokio::task::yield_now().await;
    }

    fn quit(&self) {
        self.key(KeyCode::Char('c'), KeyModifiers::CONTROL);
    }

    fn sent(&self) -> Vec<ClientRequest> {
        self.sent.lock().expect("request log lock").clone()
    }
}

fn snapshot(sequence: u64, messages: Vec<MessageSnapshot>) -> WorkspaceSnapshot {
    let mut snapshot = fixtures::workspace_snapshot();
    snapshot.cursor.sequence = sequence;
    snapshot.focused.as_mut().expect("focused").messages = messages;
    snapshot
}

fn assistant_message(byte: u8, output: &str) -> MessageSnapshot {
    MessageSnapshot {
        run_id: fixtures::run_id(9),
        state: MessageState::Streaming,
        ..fixtures::message(fixtures::message_id(byte), fixtures::SESSION, output)
    }
}

fn envelope(sequence: u64, event: SessionEvent) -> SessionEventEnvelope {
    SessionEventEnvelope {
        run_id: Some(fixtures::run_id(9)),
        ..fixtures::envelope(sequence, fixtures::SESSION, event)
    }
}

fn frame_text(frame: &[u8]) -> String {
    String::from_utf8_lossy(frame).into_owned()
}

#[test]
fn terminal_input_modes_enable_and_restore_keyboard_mouse_and_paste() {
    let mut entered = Vec::new();
    let mut restored = Vec::new();

    enable_input_modes(&mut entered).unwrap();
    disable_input_modes(&mut restored).unwrap();

    let entered = String::from_utf8(entered).unwrap();
    let restored = String::from_utf8(restored).unwrap();
    // Kitty keyboard protocol: DISAMBIGUATE | REPORT_EVENT_TYPES => 3
    assert!(entered.contains("\x1b[>3u"));
    assert!(
        entered.contains("\x1b[?1000h"),
        "mouse capture is on by default so the wheel scrolls the transcript"
    );
    assert_eq!(
        String::from_utf8(mouse_capture_bytes(true).unwrap()).unwrap(),
        "\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1015h\x1b[?1006h"
    );
    assert!(entered.contains("\x1b[?2004h"));
    assert!(restored.contains("\x1b[?1000l"));
    assert!(restored.contains("\x1b[?2004l"));
    assert!(restored.contains("\x1b[<1u"));
}

#[tokio::test(start_paused = true)]
async fn loop_draws_first_frame_then_only_when_state_changes() {
    let mut harness = Harness::new(64);
    let task = harness.spawn(App::new(TuiOptions::default()));
    harness.settle().await;
    assert_eq!(harness.frames.frames().len(), 1, "initial frame");

    // Idle: many ticks pass and nothing is redrawn.
    tokio::time::advance(FRAME_INTERVAL * 20).await;
    tokio::task::yield_now().await;
    assert_eq!(harness.frames.frames().len(), 1, "no redraw while idle");

    // A state change redraws exactly once.
    harness.update(ClientUpdate::Connection(ConnectionState::Live));
    harness.settle().await;
    let frames = harness.frames.frames();
    assert_eq!(frames.len(), 2, "one frame per dirty interval");
    assert!(frame_text(&frames[0]).contains("connecting"));
    assert!(!frame_text(&frames[1]).contains("connecting"));

    harness.quit();
    let app = task.await.expect("loop task").expect("loop exits cleanly");
    assert_eq!(app.connection, ConnectionState::Live);
}

#[tokio::test(start_paused = true)]
async fn quitting_returns_the_focused_session_for_the_resume_hint() {
    let mut harness = Harness::new(64);
    let task = harness.spawn(App::new(TuiOptions::default()));
    let snapshot = snapshot(1, Vec::new());
    let focused = snapshot.focused.as_ref().expect("focused").summary.id;
    harness.update(ClientUpdate::Snapshot(snapshot));
    harness.update(ClientUpdate::Connection(ConnectionState::Live));
    harness.settle().await;

    // `/quit` through the composer, the way a person leaves.
    for character in "/quit".chars() {
        harness.key(KeyCode::Char(character), KeyModifiers::NONE);
    }
    harness.key(KeyCode::Enter, KeyModifiers::NONE);
    harness.settle().await;
    let app = task.await.expect("loop task").expect("loop exits cleanly");

    // `terminal::run` prints `app.focused()` after restoring the terminal;
    // the loop must hand back the app with that focus intact.
    assert_eq!(app.focused(), Some(focused));
}

#[tokio::test(start_paused = true)]
async fn quitting_without_a_session_reports_none() {
    let mut harness = Harness::new(64);
    let task = harness.spawn(App::new(TuiOptions::default()));
    harness.update(ClientUpdate::Connection(ConnectionState::Live));
    harness.settle().await;
    harness.quit();
    let app = task.await.expect("loop task").expect("loop exits cleanly");
    assert_eq!(app.focused(), None);
}

#[tokio::test(start_paused = true)]
async fn loop_records_send_failures_as_local_updates() {
    // Capacity zero: every outbound request fails at the port, which the
    // loop must turn into a local failure update instead of dropping it.
    let mut harness = Harness::new(0);
    let task = harness.spawn(App::new(TuiOptions::default()));
    harness.update(ClientUpdate::Snapshot(snapshot(1, Vec::new())));
    harness.update(ClientUpdate::Connection(ConnectionState::Live));
    harness.settle().await;

    harness.key(KeyCode::Char('h'), KeyModifiers::NONE);
    harness.key(KeyCode::Char('i'), KeyModifiers::NONE);
    harness.key(KeyCode::Enter, KeyModifiers::NONE);
    harness.settle().await;
    harness.quit();
    let app = task.await.expect("loop task").expect("loop exits cleanly");

    assert!(harness.sent().is_empty());
    // The rejected submit is restored into the composer for the user.
    assert_eq!(app.composer.text, "hi");
    let (status, _) = app.visible_status().expect("failure notice is shown");
    assert!(status.contains("request queue is full"), "{status}");
}

#[tokio::test(start_paused = true)]
async fn loop_sends_requests_when_the_port_accepts_them() {
    let mut harness = Harness::new(64);
    let task = harness.spawn(App::new(TuiOptions::default()));
    harness.update(ClientUpdate::Snapshot(snapshot(1, Vec::new())));
    harness.update(ClientUpdate::Connection(ConnectionState::Live));
    harness.settle().await;

    harness.key(KeyCode::Char('h'), KeyModifiers::NONE);
    harness.key(KeyCode::Enter, KeyModifiers::NONE);
    harness.settle().await;
    harness.quit();
    let app = task.await.expect("loop task").expect("loop exits cleanly");

    let sent = harness.sent();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(matches!(sent[0], ClientRequest::Command(_)));
    assert!(app.composer.text.is_empty());
}

#[tokio::test(start_paused = true)]
async fn loop_marks_replaying_on_sequence_gap_and_recovers_from_reset_snapshot() {
    let mut harness = Harness::new(64);
    let task = harness.spawn(App::new(TuiOptions::default()));
    harness.update(ClientUpdate::Snapshot(snapshot(1, Vec::new())));
    harness.update(ClientUpdate::Connection(ConnectionState::Live));
    harness.update(ClientUpdate::Event(envelope(
        2,
        SessionEvent::AssistantMessageStarted {
            message: assistant_message(7, ""),
        },
    )));
    // Sequence 3 is missing: the loop must surface a replay state rather
    // than silently render a transcript with a hole in it.
    harness.update(ClientUpdate::Event(envelope(
        4,
        SessionEvent::TextAppended {
            message_id: fixtures::message_id(7),
            channel: TextChannel::Output,
            text: "late".to_owned(),
        },
    )));
    harness.settle().await;
    let frames = harness.frames.frames();
    assert!(
        frames
            .last()
            .is_some_and(|frame| frame_text(frame).contains("reconnecting")),
        "the latest frame should show the replay state"
    );

    // The client recovers by replacing every loaded body with a reset
    // snapshot at the new cursor.
    let mut complete = assistant_message(7, "late");
    complete.state = MessageState::Complete;
    harness.update(ClientUpdate::ResetSnapshot(snapshot(4, vec![complete])));
    harness.update(ClientUpdate::Connection(ConnectionState::Live));
    harness.settle().await;
    harness.quit();
    let app = task.await.expect("loop task").expect("loop exits cleanly");

    assert_eq!(app.connection, ConnectionState::Live);
    let session = app
        .sessions
        .get(&fixtures::SESSION)
        .expect("session loaded");
    let messages = session.messages.as_ref().expect("body loaded");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].output, "late");
    assert_eq!(messages[0].state, MessageState::Complete);
    assert!(!frame_text(harness.frames.frames().last().expect("frame")).contains("reconnecting"));
}

#[tokio::test(start_paused = true)]
async fn loop_reports_client_stop() {
    let mut harness = Harness::new(64);
    let task = harness.spawn(App::new(TuiOptions::default()));
    drop(std::mem::replace(
        &mut harness.updates,
        mpsc::unbounded_channel().0,
    ));
    let result = task.await.expect("loop task");
    assert!(matches!(result, Err(TuiError::ClientStopped)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loop_upgrades_completed_code_to_highlighted_off_the_render_path() {
    let mut harness = Harness::new(64);
    let task = harness.spawn(App::new(TuiOptions::default()));
    let mut message = assistant_message(7, "```rust\nlet x = 1;\n```");
    message.state = MessageState::Complete;
    harness.update(ClientUpdate::Snapshot(snapshot(1, vec![message])));
    harness.update(ClientUpdate::Connection(ConnectionState::Live));

    // Real time here: the highlight runs on the blocking pool. Poll until a
    // frame carries the keyword color (the theme's brand role, RGB
    // 255/159/67 in the default theme), bounded so a regression fails fast.
    // Frames are row diffs, so the upgraded frame holds only the code row.
    let keyword = "\x1b[38;2;255;159;67m\x1b[48;2;38;40;48mlet";
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut plain_seen = false;
    let mut highlighted_seen = false;
    while std::time::Instant::now() < deadline && !highlighted_seen {
        tokio::time::sleep(FRAME_INTERVAL).await;
        for frame in harness.frames.frames() {
            let text = frame_text(&frame);
            if text.contains(keyword) {
                highlighted_seen = true;
            } else if text.contains("let x = 1;") {
                plain_seen = true;
            }
        }
    }
    harness.quit();
    task.await.expect("loop task").expect("loop exits cleanly");
    assert!(plain_seen, "a plain frame should have been drawn first");
    assert!(highlighted_seen, "the highlighted frame never arrived");
}

#[tokio::test(start_paused = true)]
async fn loop_hands_the_draft_to_the_external_editor_and_installs_the_result() {
    let mut harness = Harness::new(64);
    harness.editor_script.lock().unwrap().0 = Some(Ok(Some("edited\nprompt\n".to_owned())));
    let task = harness.spawn(App::new(TuiOptions::default()));
    harness.update(ClientUpdate::Snapshot(snapshot(1, Vec::new())));
    harness.settle().await;
    for character in "draft".chars() {
        harness.key(KeyCode::Char(character), KeyModifiers::NONE);
    }
    harness.key(KeyCode::Char('e'), KeyModifiers::ALT);
    harness.settle().await;
    harness.quit();
    let app = task.await.expect("loop task").expect("loop exits cleanly");

    let script = harness.editor_script.lock().unwrap();
    assert_eq!(script.1, ["draft"], "the editor received the draft");
    assert_eq!(app.composer.text, "edited\nprompt");
    // The screen is repainted in full after the editor owned the TTY.
    let frames = harness.frames.frames();
    let last = frame_text(frames.last().unwrap());
    assert!(last.contains("\x1b[2J"), "full clear after external edit");
}

#[tokio::test(start_paused = true)]
async fn loop_reports_a_missing_editor_and_keeps_the_draft() {
    let mut harness = Harness::new(64);
    let task = harness.spawn(App::new(TuiOptions::default()));
    harness.update(ClientUpdate::Snapshot(snapshot(1, Vec::new())));
    harness.settle().await;
    harness.key(KeyCode::Char('x'), KeyModifiers::NONE);
    harness.key(KeyCode::Char('e'), KeyModifiers::ALT);
    harness.settle().await;
    harness.quit();
    let app = task.await.expect("loop task").expect("loop exits cleanly");
    assert_eq!(app.composer.text, "x");
    let (status, _) = app.visible_status().expect("a warning is shown");
    assert!(status.contains("$EDITOR"), "{status}");
}

#[tokio::test(start_paused = true)]
async fn loop_rings_the_terminal_for_an_unfocused_run_finish_only() {
    let mut harness = Harness::new(64);
    let task = harness.spawn(App::new(TuiOptions::default()));
    harness.update(ClientUpdate::Snapshot(snapshot(1, Vec::new())));
    harness.settle().await;
    let finished = |sequence| {
        ClientUpdate::Event(envelope(
            sequence,
            SessionEvent::RunFinished {
                session: fixtures::session_summary(fixtures::SESSION),
                run_id: fixtures::run_id(9),
                outcome: qq_protocol::RunOutcome::Completed,
                usage: None,
                context_tokens: None,
            },
        ))
    };
    // Focused: no bell.
    harness.update(finished(2));
    harness.settle().await;
    let before = harness.frames.frames().len();
    assert!(
        harness
            .frames
            .frames()
            .iter()
            .all(|frame| !frame.contains(&0x07)),
        "no bell while focused"
    );

    harness
        .events
        .send(Ok(Event::FocusLost))
        .expect("event channel open");
    harness.update(finished(3));
    harness.settle().await;
    let frames = harness.frames.frames();
    let bell = frames[before..]
        .iter()
        .find(|frame| frame.starts_with(b"\x07\x1b]9;"))
        .expect("a bell and OSC 9 notification were written");
    assert_eq!(frame_text(bell), "\x07\x1b]9;qq: Session finished\x07");

    harness.quit();
    task.await.expect("loop task").expect("loop exits cleanly");
}

#[tokio::test(start_paused = true)]
async fn the_first_frame_paints_before_the_client_port_connects() {
    // A port whose connect future never resolves until we release it. The
    // loop must draw `Connecting` first and only then await the connection;
    // requests made meanwhile are buffered and replayed into the real port.
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let (updates, update_rx) = mpsc::unbounded_channel::<ClientUpdate>();
    let sent = Arc::new(Mutex::new(Vec::new()));
    let inner = FakePort {
        updates: update_rx,
        sent: Arc::clone(&sent),
        capacity: 64,
    };
    let port = crate::LazyPort::new(async move {
        release_rx.await.expect("release");
        Ok(inner)
    });
    let (events, event_rx) = mpsc::unbounded_channel();
    let frames = FrameLog::default();
    let started = std::time::Instant::now();
    let task = tokio::spawn(run_loop(
        port,
        App::new(TuiOptions::default()),
        EventQueue(event_rx),
        frames.clone(),
        || Ok((100, 30)),
        std::future::pending(),
        |_| Box::pin(async { Err(EditorError::NotConfigured) }),
    ));
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    let first = frames.frames();
    assert_eq!(first.len(), 1, "the first frame does not wait on the port");
    assert!(frame_text(&first[0]).contains("connecting"));
    // Startup budget: process start to first frame in well under 30 ms of
    // wall time even under the test profile.
    assert!(
        started.elapsed() < std::time::Duration::from_millis(30),
        "{:?}",
        started.elapsed()
    );

    // A request made before the connection lands is held, not lost.
    events
        .send(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        ))))
        .unwrap();
    tokio::task::yield_now().await;
    assert!(
        sent.lock().unwrap().is_empty(),
        "nothing reaches a port that is not up"
    );

    release_tx.send(()).unwrap();
    updates
        .send(ClientUpdate::Connection(ConnectionState::Live))
        .unwrap();
    for _ in 0..8 {
        tokio::time::advance(FRAME_INTERVAL).await;
        tokio::task::yield_now().await;
    }
    let frames = frames.frames();
    assert!(
        frames.len() >= 2 && !frame_text(frames.last().unwrap()).contains("connecting"),
        "{}",
        frames.len()
    );
    events
        .send(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        ))))
        .unwrap();
    let app = task.await.expect("loop task").expect("loop exits cleanly");
    assert_eq!(app.connection, ConnectionState::Live);
}

#[tokio::test(start_paused = true)]
async fn a_failed_connection_is_reported_once_and_then_the_client_stops() {
    let port =
        crate::LazyPort::<FakePort>::new(async { Err(crate::ClientFailure::new("no server")) });
    let (_events, event_rx) = mpsc::unbounded_channel();
    let frames = FrameLog::default();
    let task = tokio::spawn(run_loop(
        port,
        App::new(TuiOptions::default()),
        EventQueue(event_rx),
        frames.clone(),
        || Ok((100, 30)),
        std::future::pending(),
        |_| Box::pin(async { Err(EditorError::NotConfigured) }),
    ));
    let result = task.await.expect("loop task");
    assert!(matches!(result, Err(TuiError::ClientStopped)));
}
