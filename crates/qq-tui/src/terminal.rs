use std::{
    collections::VecDeque,
    future::Future,
    io::{self, stdout},
};

use crossterm::{
    cursor::{MoveTo, Show},
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, Event, EventStream, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    style::{Attribute, Print, ResetColor, SetAttribute},
    terminal::{self, Clear, ClearType, EndSynchronizedUpdate},
};
use futures_util::{Stream, StreamExt};
use qq_protocol::{SessionEvent, SessionEventEnvelope, SessionId};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    time::{Duration, MissedTickBehavior, interval},
};

use crate::{
    ClientPort, ClientRequest, ClientUpdate,
    app::{App, TuiError},
    effect::{Effect, Effects, Redraw},
    view::FrameRenderer,
};

pub(crate) const FRAME_INTERVAL: Duration = Duration::from_millis(8);
const ANIMATION_INTERVAL: Duration = Duration::from_millis(crate::app::ANIMATION_INTERVAL_MS);

pub async fn run<P>(client: P, app: App) -> Result<Option<SessionId>, TuiError>
where
    P: ClientPort,
{
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    tokio::select! {
        biased;
        result = &mut shutdown => return result.map(|()| None).map_err(TuiError::from),
        _ = tokio::task::yield_now() => {}
    }

    let terminal = TerminalGuard::enter()?;
    let events = EventStream::new();
    let output = tokio::io::stdout();
    let herdr = crate::herdr::HerdrPane::from_env().map(crate::herdr::Reporter::spawn);
    let app = run_loop(
        client,
        app,
        events,
        output,
        terminal::size,
        shutdown,
        Hooks {
            editor: external_editor,
            herdr: herdr.as_ref().map(crate::herdr::Reporter::sink),
        },
    )
    .await;
    if let Some(herdr) = herdr {
        herdr.release().await;
    }
    let app = app?;
    drop(terminal);
    Ok(app.focused())
}

fn external_editor(draft: String) -> EditorFuture {
    Box::pin(async move {
        let Some(command) = std::env::var_os("VISUAL")
            .or_else(|| std::env::var_os("EDITOR"))
            .filter(|value| !value.is_empty())
        else {
            return Err(EditorError::NotConfigured);
        };
        let mut output = stdout();
        let _ = disable_input_modes(&mut output);
        let _ = execute!(output, Show);
        let _ = terminal::disable_raw_mode();
        let result = tokio::task::spawn_blocking(move || {
            let file = tempfile::Builder::new()
                .prefix("qq-draft-")
                .suffix(".md")
                .tempfile()
                .map_err(EditorError::Io)?;
            std::fs::write(file.path(), &draft).map_err(EditorError::Io)?;
            let command = command.to_string_lossy();
            let mut parts = command.split_whitespace();
            let program = parts.next().ok_or(EditorError::NotConfigured)?;
            let status = std::process::Command::new(program)
                .args(parts)
                .arg(file.path())
                .status()
                .map_err(EditorError::Io)?;
            if !status.success() {
                return Err(EditorError::Exited(status.code()));
            }
            let edited = std::fs::read_to_string(file.path()).map_err(EditorError::Io)?;
            Ok((edited != draft).then_some(edited))
        })
        .await
        .unwrap_or(Err(EditorError::NotConfigured));
        let _ = terminal::enable_raw_mode();
        let _ = enable_input_modes(&mut output);
        let _ = execute!(output, Clear(ClearType::All), MoveTo(0, 0));
        result
    })
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum EditorError {
    #[error("set $VISUAL or $EDITOR to edit the draft externally")]
    NotConfigured,
    #[error("the editor exited with status {}", .0.map_or("unknown".to_owned(), |code| code.to_string()))]
    Exited(Option<i32>),
    #[error("the editor could not run: {0}")]
    Io(io::Error),
}

pub(crate) type EditorFuture =
    std::pin::Pin<Box<dyn Future<Output = Result<Option<String>, EditorError>> + Send>>;

pub(crate) struct Hooks<X> {
    pub(crate) editor: X,
    pub(crate) herdr: Option<crate::herdr::ReportSink>,
}

pub(crate) async fn run_loop<P, E, W, S, F, X>(
    mut client: P,
    mut app: App,
    mut terminal_events: E,
    mut output: W,
    mut size: S,
    shutdown: F,
    hooks: Hooks<X>,
) -> Result<App, TuiError>
where
    P: ClientPort,
    E: Stream<Item = io::Result<Event>> + Unpin,
    W: AsyncWrite + Unpin,
    S: FnMut() -> io::Result<(u16, u16)>,
    F: Future<Output = io::Result<()>>,
    X: FnMut(String) -> EditorFuture,
{
    let Hooks { mut editor, herdr } = hooks;
    tokio::pin!(shutdown);
    let mut renderer = FrameRenderer::default();
    let mut frame_tick = interval(FRAME_INTERVAL);
    let mut animation_tick = interval(ANIMATION_INTERVAL);
    frame_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    animation_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut terminal_size = size()?;
    let mut redraw = Some(Redraw::Immediate);
    let mut last_client_failure: Option<String> = None;
    let mut state_may_change = false;

    loop {
        if redraw == Some(Redraw::Immediate) {
            app.refresh_goal_clock();
            let bytes = renderer.draw(&mut app, terminal_size)?;
            output.write_all(&bytes).await?;
            output.flush().await?;
            redraw = None;
            frame_tick.reset();
        }
        let effects = tokio::select! {
            biased;
            result = &mut shutdown => {
                result?;
                break;
            }
            event = terminal_events.next() => {
                match event {
                    Some(Ok(event)) => {
                        if let Event::Resize(columns, rows) = event {
                            terminal_size = (columns, rows);
                        }
                        state_may_change = true;
                        app.handle_terminal_event(event)
                    }
                    Some(Err(error)) => return Err(TuiError::Terminal(error)),
                    None => break,
                }
            }
            update = client.recv() => {
                let Some(update) = update else {
                    return Err(TuiError::ClientStopped(last_client_failure.take()));
                };
                if let ClientUpdate::SnapshotFailed(failure) = &update {
                    last_client_failure = Some(failure.message().to_owned());
                }
                state_may_change |= !matches!(
                    &update,
                    ClientUpdate::Event(SessionEventEnvelope {
                        event: SessionEvent::TextAppended { .. }
                            | SessionEvent::ReasoningDelta { .. }
                            | SessionEvent::ToolCallOutputDelta { .. }
                            | SessionEvent::RunActivityChanged { .. },
                        ..
                    })
                );
                app.apply_client_update(update)
            }
            highlighted = renderer.highlighter.next() => {
                Effects::changed(renderer.apply_highlight(highlighted))
            }
            _ = animation_tick.tick(), if app.has_activity() => {
                Effects::changed(app.advance_animation())
            }
            _ = frame_tick.tick(), if redraw.is_some() => {
                app.refresh_goal_clock();
                let bytes = renderer.draw(&mut app, terminal_size)?;
                output.write_all(&bytes).await?;
                output.flush().await?;
                redraw = None;
                Effects::none()
            }
        };
        let mut queue: VecDeque<Effect> = effects.into_iter().collect();
        let mut quit = false;
        while let Some(effect) = queue.pop_front() {
            match effect {
                Effect::Redraw(level) => {
                    redraw = Some(redraw.map_or(level, |existing| existing.max(level)));
                }
                Effect::Send(request) => {
                    if let Err(error) = client.try_send(request.clone()) {
                        queue.extend(apply_send_failure(&mut app, request, error));
                    }
                }
                Effect::Editor(draft) => {
                    match editor(draft).await {
                        Ok(text) => {
                            app.apply_editor_result(text);
                        }
                        Err(error) => {
                            app.note_editor_failure(&error.to_string());
                        }
                    }
                    renderer.invalidate();
                    redraw = Some(Redraw::Immediate);
                }
                Effect::ResolveMentions(submit) => {
                    let root = app.workspace_root.clone();
                    let text = submit.text.clone();
                    let resolved = match root {
                        Some(root) => tokio::task::spawn_blocking(move || {
                            qq_core::mentions::resolve_prompt(&root, &text)
                        })
                        .await
                        .map_err(|_| "mention resolution stopped unexpectedly".to_owned()),
                        None => Err("this client has no workspace tree".to_owned()),
                    };
                    queue.extend(app.apply_resolved_mentions(submit, resolved));
                }
                Effect::CompleteMention { query, recent } => {
                    let Some(root) = app.workspace_root.clone() else {
                        continue;
                    };
                    let walk_query = query.clone();
                    let candidates = tokio::task::spawn_blocking(move || {
                        qq_core::mentions::complete_paths(&root, &walk_query, &recent)
                    })
                    .await
                    .unwrap_or_default();
                    if app.apply_mention_completions(query, candidates) {
                        redraw = Some(redraw.map_or(Redraw::Scheduled, |existing| {
                            existing.max(Redraw::Scheduled)
                        }));
                    }
                }
                Effect::Attention(attention) => {
                    output.write_all(&attention_bytes(&attention)).await?;
                    output.flush().await?;
                }
                Effect::MouseCapture(enabled) => {
                    let bytes = mouse_capture_bytes(enabled)?;
                    output.write_all(&bytes).await?;
                    output.flush().await?;
                }
                Effect::Quit => quit = true,
            }
        }
        if quit {
            break;
        }
        if state_may_change
            && app.workspace_id.is_some()
            && let Some(herdr) = &herdr
        {
            state_may_change = false;
            herdr.update(crate::herdr::Report {
                activity: app.sessions.activity(),
                session: app
                    .focused()
                    .and_then(|focused| app.sessions.root_of(focused)),
            });
        }
    }
    Ok(app)
}

fn attention_bytes(attention: &crate::app::Attention) -> Vec<u8> {
    let text: String = attention
        .summary()
        .chars()
        .filter(|character| !character.is_control())
        .take(200)
        .collect();
    format!("\x07\x1b]9;{text}\x07").into_bytes()
}

fn apply_send_failure(
    app: &mut App,
    request: ClientRequest,
    error: crate::ClientFailure,
) -> Effects {
    let update = match request {
        ClientRequest::Command(command) => ClientUpdate::CommandResult {
            command_id: command.command_id,
            result: Err(error),
        },
        ClientRequest::Snapshot(_) => ClientUpdate::SnapshotFailed(error),
        ClientRequest::Capabilities | ClientRequest::Models(_) => return Effects::none(),
    };
    app.apply_client_update(update)
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = Self;
        let mut output = stdout();
        enable_input_modes(&mut output)?;
        execute!(output, Clear(ClearType::All), MoveTo(0, 0))?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let (_, height) = terminal::size().unwrap_or((80, 24));
        let mut output = stdout();
        let _ = execute!(
            output,
            SetAttribute(Attribute::Reset),
            ResetColor,
            EndSynchronizedUpdate
        );
        let _ = disable_input_modes(&mut output);
        let _ = execute!(
            output,
            MoveTo(0, height.saturating_sub(1)),
            Clear(ClearType::CurrentLine),
            Show,
            Print("\r\n")
        );
    }
}

fn mouse_capture_bytes(enabled: bool) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    if enabled {
        execute!(bytes, EnableMouseCapture)?;
    } else {
        execute!(bytes, DisableMouseCapture)?;
    }
    Ok(bytes)
}

fn enable_input_modes(output: &mut impl io::Write) -> io::Result<()> {
    execute!(
        output,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        ),
        EnableBracketedPaste,
        EnableMouseCapture,
        EnableFocusChange
    )
}

fn disable_input_modes(output: &mut impl io::Write) -> io::Result<()> {
    execute!(
        output,
        DisableFocusChange,
        DisableMouseCapture,
        DisableBracketedPaste,
        PopKeyboardEnhancementFlags
    )
}

#[cfg(unix)]
async fn shutdown_signal() -> io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
        _ = hangup.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> io::Result<()> {
    tokio::signal::ctrl_c().await
}

#[cfg(test)]
mod tests;
