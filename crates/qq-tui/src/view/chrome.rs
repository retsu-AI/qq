use super::*;

#[cfg(test)]
pub(super) const VERSION: &str = env!("CARGO_PKG_VERSION");

/// One spinner for every running thing on screen, at the animation tick.
pub(super) fn spinner(tick: usize) -> &'static str {
    TOOL_SPINNER[tick % TOOL_SPINNER.len()]
}

/// The single top row: brand mark, breadcrumb of the focused session, then
/// right-aligned model, context occupancy, cost, and the connection state
/// only when it is degraded. `local` and the version are not shown; they
/// are in `/status`.
pub(super) fn top_row(app: &App, width: usize) -> Line {
    let mut left = Line::styled(" qq", brand().bold());
    if let Some(focused) = app.focused() {
        let mut ancestors = Vec::new();
        let mut cursor = Some(focused);
        while let Some(id) = cursor {
            let Some(session) = app.sessions.get(&id) else {
                break;
            };
            ancestors.push(session.summary.title.as_str());
            cursor = session.summary.parent_id;
        }
        ancestors.reverse();
        left.push("  ", muted());
        for (index, title) in ancestors.iter().enumerate() {
            if index > 0 {
                left.push(" › ", muted());
            }
            left.push(
                *title,
                if index + 1 == ancestors.len() {
                    normal().bold()
                } else {
                    muted()
                },
            );
        }
    } else if !app.workspace_path.is_empty() {
        left.push(format!("  {}", app.workspace_path), muted());
    }

    let mut right = Line::default();
    let focused = app
        .focused()
        .and_then(|id| app.sessions.get(&id))
        .map(|session| &session.summary);
    for item in app.settings.status_line() {
        let part: Option<(String, Style)> = match item {
            // Without a session or a client default there is nothing to
            // route to; say so where the model would be so `/models` is the
            // obvious next step.
            StatusItem::Model => Some(
                focused
                    .and_then(|session| session.model.as_deref())
                    .or(app.model.model.as_deref())
                    .map_or_else(
                        || ("no model".to_owned(), warning()),
                        |model| (model.to_owned(), accent()),
                    ),
            ),
            // The default profile is the unremarkable case; the badge appears
            // only when a session (or the next one) runs as something else.
            StatusItem::Profile => {
                let profile = focused.map_or(&app.profile, |session| &session.profile);
                (!profile.is_default()).then(|| (format!("as {}", profile.as_str()), accent()))
            }
            // `auto` is the default; the badge names anything stricter or
            // looser so the user always knows what a held call means. A
            // session delegate override rides along: `ask · delegate on`,
            // `auto · delegate off`.
            StatusItem::ApprovalMode => {
                let mode = focused.map_or(app.approval_mode, |session| session.approval_mode);
                let delegate = focused.and_then(|session| session.approval_delegate);
                (mode != ApprovalMode::Auto || delegate.is_some()).then(|| {
                    let style = if mode == ApprovalMode::Full {
                        warning()
                    } else {
                        muted()
                    };
                    let text = match delegate {
                        Some(delegate) => format!(
                            "{} · delegate {}",
                            approval_mode_label(mode),
                            delegate.as_str()
                        ),
                        None => approval_mode_label(mode).to_owned(),
                    };
                    (text, style)
                })
            }
            // Omission is the unremarkable case; the badge names an explicit pin.
            StatusItem::Effort => {
                let effort =
                    focused.map_or(app.reasoning_effort, |session| session.reasoning_effort);
                effort.map(|effort| (format!("effort {}", effort_label(Some(effort))), accent()))
            }
            // The configured Jev capabilities are the unremarkable case; the
            // badge names a session's explicit rung.
            StatusItem::Jev => focused
                .and_then(|session| session.jev_mode)
                .map(|mode| (format!("jev {}", jev_mode_label(Some(mode))), accent())),
            StatusItem::Context => match app.focused_context_usage() {
                Some((tokens, limit)) if limit > 0 => {
                    let percent = u128::from(tokens) * 100 / u128::from(limit);
                    Some((format!("{percent}% ctx"), muted()))
                }
                _ => None,
            },
            StatusItem::Cost => focused
                .and_then(|session| {
                    session
                        .accounting
                        .map(|accounting| accounting.inclusive.estimated_cost_usd_nanos)
                        .unwrap_or(session.estimated_cost_usd_nanos)
                })
                .map(|nanos| (format_cost(nanos), muted())),
            StatusItem::Workspace => {
                (!app.workspace_path.is_empty()).then(|| (app.workspace_path.clone(), muted()))
            }
            StatusItem::Tools => Some((format!("tools {}", app.tool_detail.label()), muted())),
        };
        if let Some((text, style)) = part {
            if !right.is_empty() {
                right.push("  ", muted());
            }
            right.push(text, style);
        }
    }
    let connection = match app.connection {
        crate::ConnectionState::Connecting => Some("connecting"),
        crate::ConnectionState::Replaying => Some("reconnecting"),
        crate::ConnectionState::Offline => Some("offline"),
        crate::ConnectionState::Live => None,
    };
    if let Some(connection) = connection {
        if !right.is_empty() {
            right.push("  ", muted());
        }
        right.push(connection, warning().bold());
    }
    right.push(" ", muted());
    align_sides(left, right, width)
}

/// The rule between transcript and composer carries the status the user
/// needs while typing. A transient notice takes the left side while it
/// lasts; otherwise a running session shows its activity, elapsed time, and
/// time to first token. Approvals waiting in other sessions are named here
/// too so they never stall silently. Context-sensitive key hints from the
/// command table sit right, so the composer needs no hint row of its own.
pub(super) fn composer_rule(app: &App, width: usize) -> Line {
    let mut left = Line::default();
    if let Some((status, level)) = app.visible_status() {
        let (prefix, style) = match level {
            crate::app::NoticeLevel::Info => ("", accent()),
            crate::app::NoticeLevel::Warning => ("warning: ", warning()),
            crate::app::NoticeLevel::Error => ("error: ", failure()),
        };
        left.push(format!(" {prefix}{status} "), style.bold());
        return rule_with(left, Line::default(), width);
    }
    // Standing guidance for a client that cannot create a session yet: it
    // occupies the notice slot for as long as nothing is focused, so the
    // first frame already says what to do instead of waiting for Alt-N to
    // fail. A transient notice above takes precedence while it lasts.
    if app.focused().is_none()
        && let Some(guidance) = app.startup_guidance()
    {
        left.push(format!(" {guidance} "), accent().bold());
        return rule_with(left, Line::default(), width);
    }
    if let Some(session) = app.focused().and_then(|id| app.sessions.get(&id))
        && session.summary.status == SessionStatus::Running
    {
        left.push(format!(" {} ", spinner(app.animation_tick)), info());
        left.push(
            match session.activity.map(|(_, activity)| activity) {
                Some(qq_protocol::RunActivity::WaitingForProvider) => "waiting for model",
                Some(qq_protocol::RunActivity::Reasoning) => "reasoning",
                Some(qq_protocol::RunActivity::GeneratingResponse) => "generating",
                Some(qq_protocol::RunActivity::PreparingToolCall) => "preparing tool call",
                None => "working",
            },
            info(),
        );
        // Elapsed and time to first token answer "is it stuck?" without a
        // glance at the transcript.
        if let Some(stats) = session
            .summary
            .active_run_id
            .and_then(|run_id| session.runs.get(&run_id))
            && let Some(started) = stats.started_at_ms
        {
            left.push(
                format!(
                    " {}",
                    format_duration_ms(app.now_ms.saturating_sub(started))
                ),
                muted(),
            );
            if let Some(first) = stats.first_token_at_ms {
                left.push(
                    format!(
                        "  ttft {}",
                        format_duration_ms(first.saturating_sub(started))
                    ),
                    muted(),
                );
            }
            // Committed turns and their cost so far, so a long agentic run
            // shows it is progressing and what it has spent.
            if stats.turns > 0 {
                left.push(format!("  turn {}", stats.turns), muted());
            }
            if let Some(cost) = stats.live_cost_usd_nanos
                && cost > 0
            {
                left.push(format!("  {}", format_cost(cost)), muted());
            }
        }
    } else if let Some(session) = app.focused().and_then(|id| app.sessions.get(&id))
        && session.summary.status == SessionStatus::Queued
    {
        left.push(" queued", muted());
    }
    let waiting: Vec<&str> = app
        .sessions_awaiting_approval()
        .into_iter()
        .filter(|id| Some(*id) != app.focused())
        .filter_map(|id| app.sessions.get(&id))
        .map(|session| session.summary.title.as_str())
        .collect();
    if !waiting.is_empty() {
        left.push(if left.is_empty() { " " } else { "  " }, muted());
        left.push("◇ ", warning().bold());
        left.push(
            match waiting.as_slice() {
                [one] => format!("{one} needs approval"),
                [first, rest @ ..] => format!("{first} +{} need approval", rest.len()),
                [] => String::new(),
            },
            warning().bold(),
        );
        let jump = app.chord_label(crate::commands::Command::FocusNextApproval);
        let approve = app.chord_label(crate::commands::Command::ApproveBackground);
        let deny = app.chord_label(crate::commands::Command::DenyBackground);
        let mut hints = Vec::new();
        if let Some(chord) = jump {
            hints.push(format!("{chord} jump"));
        }
        if let (Some(approve), Some(deny)) = (approve, deny) {
            hints.push(format!("{approve}/{deny} answer"));
        }
        if !hints.is_empty() {
            left.push(format!("  {}", hints.join("  ")), muted());
        }
    }

    let mut right = Line::default();
    for (command, label) in hints_for(app) {
        // `?` on an empty composer opens help too, and is the cheaper key
        // to discover; the hint names it while it works and falls back to
        // the chord once typing has started.
        let chord = if command == crate::commands::Command::OpenHelp
            && app.mode() == Mode::Compose
            && app.composer.text.is_empty()
        {
            "?".to_owned()
        } else {
            let Some(chord) = app.chord_label(command) else {
                continue;
            };
            compact_chord(&chord)
        };
        if !right.is_empty() {
            right.push("  ", muted());
        }
        right.push(chord, accent());
        right.push(format!(" {label}"), muted());
    }
    rule_with(left, right, width)
}

/// A rule of `─` with `left` laid over its start and `right` over its end.
/// Status on the left outranks hints on the right: when both do not fit
/// with a visible run of rule, the hints go first.
fn rule_with(left: Line, mut right: Line, width: usize) -> Line {
    const MIN_RULE: usize = 4;
    if !right.is_empty() {
        let mut padded = Line::styled(" ", muted());
        padded.spans.append(&mut right.spans);
        padded.push(" ", muted());
        right = padded;
    }
    if left.width() + right.width() + MIN_RULE > width {
        right = Line::default();
    }
    // The separating space after a truncated left side must not eat into
    // the minimum rule.
    let mut rule = truncate_line(left, width.saturating_sub(right.width() + MIN_RULE + 1));
    if !rule.is_empty() {
        rule.push(" ", muted());
    }
    let filler = width.saturating_sub(rule.width() + right.width());
    rule.push("─".repeat(filler), border());
    for span in right.spans {
        rule.push(span.text, span.style);
    }
    truncate_line(rule, width)
}

/// The three or four most useful commands for the current state, in order.
fn hints_for(app: &App) -> Vec<(crate::commands::Command, &'static str)> {
    use crate::commands::Command;
    let mut hints = Vec::with_capacity(4);
    match app.mode() {
        Mode::Compose => {
            hints.push((Command::OpenHelp, "help"));
            hints.push((Command::OpenCommands, "commands"));
            let running = app
                .focused()
                .and_then(|id| app.sessions.get(&id))
                .is_some_and(|session| session.summary.active_run_id.is_some());
            if running {
                hints.push((Command::QueueDraft, "queue"));
                hints.push((Command::CancelRun, "cancel"));
            } else {
                hints.push((Command::ToggleToolDetail, "detail"));
                hints.push((Command::OpenSessions, "sessions"));
            }
        }
        Mode::Approval | Mode::Trust => {
            hints.push((Command::OpenHelp, "help"));
        }
        Mode::Models
        | Mode::Profiles
        | Mode::ApprovalModes
        | Mode::Effort
        | Mode::Delegate
        | Mode::Jev
        | Mode::Skills
        | Mode::Themes
        | Mode::Sessions
        | Mode::Commands
        | Mode::History => {}
    }
    hints
}

/// `Ctrl-K` as `^K`, `Alt-N` as `M-N`, function and plain keys unchanged:
/// the footer has room for four hints only in the short form.
fn compact_chord(chord: &str) -> String {
    let mut out = String::with_capacity(chord.len());
    let mut rest = chord;
    loop {
        if let Some(tail) = rest.strip_prefix("Ctrl-") {
            out.push('^');
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix("Alt-") {
            out.push_str("M-");
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix("Shift-") {
            out.push_str("S-");
            rest = tail;
        } else {
            break;
        }
    }
    out.push_str(rest);
    out
}

/// What Enter does right now, shown as the composer's prompt glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposerMode {
    /// Enter sends a new prompt.
    Send,
    /// Enter steers the active run at its next boundary.
    Steer,
    /// Enter holds the draft until the run finishes.
    Queue,
    /// An approval owns input; the composer is disabled.
    Approval,
    /// The trust prompt owns input; the composer is disabled.
    Trust,
}

impl ComposerMode {
    pub(crate) const fn glyph(self) -> &'static str {
        match self {
            Self::Send => "›",
            Self::Steer => "↦",
            Self::Queue => "⇥",
            Self::Approval | Self::Trust => "✎",
        }
    }

    const fn placeholder(self) -> &'static str {
        match self {
            Self::Send => "Ask QQ...",
            Self::Steer => "Steer the run...",
            Self::Queue => "Queue for after this run...",
            Self::Approval => "Answer the approval above",
            Self::Trust => "Answer the trust prompt above",
        }
    }

    /// Whether the composer accepts text at all.
    const fn disabled(self) -> bool {
        matches!(self, Self::Approval | Self::Trust)
    }
}

/// Where the terminal cursor belongs after a frame, in frame coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CursorPosition {
    pub column: u16,
    pub row: u16,
}

/// The composer block: a rule carrying the approval-mode chip, then the
/// draft rows with the mode glyph in the gutter of the first. Returns the
/// rows and the caret's position relative to the block's first row, or
/// `None` when the composer does not own the cursor.
pub(super) fn composer(
    app: &App,
    width: usize,
    max_rows: usize,
) -> (Vec<Line>, Option<(usize, usize)>) {
    let max_rows = max_rows.max(1);
    let mode = app.composer_mode();
    // An armed amendment turns the composer into the steering note field.
    if let Some(choice) = &app.approval_amendment {
        let mut line = Line::styled(" ✎ ", warning().bold());
        line.push(
            match choice {
                crate::app::ApprovalChoice::Deny => "deny and steer: ",
                _ => "approve and steer: ",
            },
            warning(),
        );
        let used = line.width();
        if app.composer.text.is_empty() {
            line.push(
                "what should it do instead? Enter sends, Esc cancels",
                muted().italic(),
            );
            return (vec![truncate_line(line, width)], Some((used, 0)));
        }
        let caret_column = used
            + app.composer.text[..app.composer.cursor()]
                .chars()
                .map(|character| {
                    unicode_width::UnicodeWidthChar::width(character).unwrap_or_default()
                })
                .sum::<usize>();
        for span in composer_row(&app.composer.text).spans {
            line.push(span.text, span.style);
        }
        return (vec![truncate_line(line, width)], Some((caret_column, 0)));
    }
    let glyph_style = match mode {
        ComposerMode::Send => accent().bold(),
        ComposerMode::Steer | ComposerMode::Queue => warning().bold(),
        ComposerMode::Approval | ComposerMode::Trust => muted(),
    };
    let gutter = format!(" {} ", mode.glyph());
    // A free-text question uses the composer for the answer, so its caret
    // shows; a permission hold has nothing to type.
    let answering = app
        .pending_question()
        .and_then(|question| question.questions.get(app.question_answers.len()))
        .is_some_and(|question| question.free_text);
    if app.composer.text.is_empty() {
        let mut line = Line::styled(gutter, glyph_style);
        line.push(
            if answering {
                "Type your answer..."
            } else {
                mode.placeholder()
            },
            muted().italic(),
        );
        let caret = (!mode.disabled() || answering).then_some((3, 0));
        return (vec![truncate_line(line, width)], caret);
    }

    // Lay the draft out with the same wrapping the caret is measured by so
    // both agree on where each character lands.
    let content_width = width.saturating_sub(3).max(1);
    let cursor = app.composer.cursor();
    let mut wrapped: Vec<Line> = Vec::new();
    let mut caret: Option<(usize, usize)> = None;
    let mut consumed = 0_usize;
    for (line_index, part) in app.composer.text.split('\n').enumerate() {
        let content_rows = if part.is_empty() {
            vec![Line::default()]
        } else {
            wrap_line_chars(composer_row(part), content_width)
        };
        let cursor_in_part = cursor >= consumed && cursor <= consumed + part.len();
        let mut row_start = consumed;
        for (row_index, content) in content_rows.into_iter().enumerate() {
            let row_bytes: usize = content.spans.iter().map(|span| span.text.len()).sum();
            if cursor_in_part && caret.is_none() {
                let last_row = row_start + row_bytes == consumed + part.len();
                if cursor < row_start + row_bytes || (last_row && cursor <= row_start + row_bytes) {
                    let column = content
                        .spans
                        .iter()
                        .flat_map(|span| span.text.chars())
                        .scan(row_start, |offset, character| {
                            let start = *offset;
                            *offset += character.len_utf8();
                            Some((start, character))
                        })
                        .take_while(|(start, _)| *start < cursor)
                        .map(|(_, character)| {
                            unicode_width::UnicodeWidthChar::width(character).unwrap_or_default()
                        })
                        .sum::<usize>();
                    caret = Some((3 + column, wrapped.len()));
                }
            }
            row_start += row_bytes;
            let mut row = if line_index == 0 && row_index == 0 {
                Line::styled(gutter.clone(), glyph_style)
            } else {
                Line::styled("   ", muted())
            };
            for span in content.spans {
                row.push(span.text, span.style);
            }
            wrapped.push(row);
        }
        consumed += part.len() + 1;
    }

    // When the draft outgrows the reserved composer region, keep the rows
    // around the caret so the cursor and newest typing stay visible.
    if wrapped.len() > max_rows {
        let caret_row = caret.map_or(wrapped.len() - 1, |(_, row)| row);
        let skip = caret_row
            .saturating_sub(max_rows - 1)
            .min(wrapped.len() - max_rows);
        wrapped.drain(..skip);
        wrapped.truncate(max_rows);
        if let Some((_, row)) = caret.as_mut() {
            *row = row.saturating_sub(skip);
        }
        if skip > 0
            && let Some(first) = wrapped.first_mut()
        {
            let rest = std::mem::take(first);
            let mut clipped = Line::styled(" … ", muted());
            for span in rest.spans.into_iter().skip(1) {
                clipped.push(span.text, span.style);
            }
            *first = clipped;
        }
    }
    for row in &mut wrapped {
        *row = truncate_line(std::mem::take(row), width);
    }
    if mode.disabled() {
        caret = None;
    }
    (wrapped, caret)
}

/// Drafts held locally while the focused session runs, oldest first. Each
/// takes one row; the newest is the one Alt-Up brings back.
pub(super) fn queued_drafts(app: &App, width: usize) -> Vec<Line> {
    let Some(session_id) = app.focused() else {
        return Vec::new();
    };
    let drafts: Vec<&str> = app.queued_drafts(session_id).collect();
    if drafts.is_empty() {
        return Vec::new();
    }
    let count = drafts.len();
    let edit_hint = app
        .chord_label(crate::commands::Command::DequeueDraft)
        .map_or_else(String::new, |chord| format!("  {chord} edits"));
    drafts
        .into_iter()
        .enumerate()
        .map(|(index, draft)| {
            let mut line = Line::styled(" ~ ", warning());
            line.push(
                if index + 1 == count {
                    format!("queued{edit_hint}  ")
                } else {
                    "queued  ".to_owned()
                },
                warning(),
            );
            line.push(preview(draft, width.saturating_sub(line.width())), muted());
            truncate_line(line, width)
        })
        .collect()
}

/// One logical composer line with paste placeholders styled as tokens so the
/// user can tell them from typed text.
pub(super) fn composer_row(part: &str) -> Line {
    let mut line = Line::default();
    let mut rest = part;
    while let Some(start) = rest.find("[Pasted #") {
        let Some(len) = rest[start..].find(']') else {
            break;
        };
        line.push(&rest[..start], normal());
        line.push(&rest[start..=start + len], accent().italic());
        rest = &rest[start + len + 1..];
    }
    line.push(rest, normal());
    line
}

/// Rows the slash menu wants to show. The rule stays within `MAX_SLASH_ROWS`
/// so it never hides more than a few transcript rows.
const MAX_SLASH_ROWS: usize = 8;

/// The slash-command menu: a boxed list anchored to the bottom of the body,
/// drawn over the transcript. Rows are the matching commands with the cursor
/// kept visible; the box has a top rule so it reads as a menu, not as text.
pub(super) fn slash_autocomplete(app: &App, width: usize, height: usize) -> Vec<Line> {
    let commands = app.filtered_slash_commands();
    if commands.is_empty() || height < 2 {
        return Vec::new();
    }
    let selected = app.slash_selected(commands.len());
    let visible = height
        .saturating_sub(1)
        .min(MAX_SLASH_ROWS)
        .min(commands.len());
    let start = selected
        .saturating_sub(visible.saturating_sub(1))
        .min(commands.len().saturating_sub(visible));
    let name_column = commands
        .iter()
        .map(|command| command.name.len())
        .max()
        .unwrap_or(0)
        + 2;
    let mut lines = Vec::with_capacity(visible + 1);
    let mut rule = Line::styled("─".repeat(width.min(2)), muted());
    rule.push(" commands ", muted());
    let used = rule.width();
    rule.push("─".repeat(width.saturating_sub(used)), muted());
    lines.push(truncate_line(rule, width));
    for (index, command) in commands.iter().enumerate().skip(start).take(visible) {
        let mut line = Line::styled(if index == selected { " > " } else { "   " }, accent());
        line.push(
            format!("{:<name_column$}", command.name),
            if index == selected {
                normal().bold()
            } else {
                normal()
            },
        );
        line.push(command.title.as_ref(), muted());
        pad_line(&mut line, width);
        lines.push(truncate_line(line, width));
    }
    lines
}

/// The `@` completion menu: workspace paths for the token at the cursor,
/// same box as the slash menu so the two read as one affordance.
pub(super) fn mention_autocomplete(app: &App, width: usize, height: usize) -> Vec<Line> {
    let candidates = &app.mention.candidates;
    if candidates.is_empty() || app.composer.mention_token().is_none() || height < 2 {
        return Vec::new();
    }
    let selected = app.mention.cursor.selected(candidates.len());
    let visible = height
        .saturating_sub(1)
        .min(MAX_SLASH_ROWS)
        .min(candidates.len());
    let start = selected
        .saturating_sub(visible.saturating_sub(1))
        .min(candidates.len().saturating_sub(visible));
    let mut lines = Vec::with_capacity(visible + 1);
    let mut rule = Line::styled("─".repeat(width.min(2)), muted());
    rule.push(" files ", muted());
    let used = rule.width();
    rule.push("─".repeat(width.saturating_sub(used)), muted());
    lines.push(truncate_line(rule, width));
    for (index, candidate) in candidates.iter().enumerate().skip(start).take(visible) {
        let mut line = Line::styled(if index == selected { " > " } else { "   " }, accent());
        let style = if index == selected {
            normal().bold()
        } else if candidate.ends_with('/') {
            muted()
        } else {
            normal()
        };
        line.push(elide_path(candidate, width.saturating_sub(4)), style);
        pad_line(&mut line, width);
        lines.push(truncate_line(line, width));
    }
    lines
}

pub(super) fn overlay_slash_autocomplete(body: &mut [Line], autocomplete: Vec<Line>) {
    let start = body.len().saturating_sub(autocomplete.len());
    for (target, line) in body[start..].iter_mut().zip(autocomplete) {
        *target = line;
    }
}

pub(super) fn align_sides(mut left: Line, right: Line, width: usize) -> Line {
    let right_width = right.width();
    if right_width >= width {
        return truncate_line(right, width);
    }
    left = truncate_line(left, width - right_width);
    left.push(" ".repeat(width - right_width - left.width()), muted());
    for span in right.spans {
        left.push(span.text, span.style);
    }
    left
}

pub(super) fn format_cost(usd_nanos: u64) -> String {
    let whole = usd_nanos / 1_000_000_000;
    let micros = (usd_nanos % 1_000_000_000) / 1_000;
    let mut fractional = format!("{micros:06}");
    while fractional.len() > 2 && fractional.ends_with('0') {
        fractional.pop();
    }
    format!("${whole}.{fractional}")
}

pub(super) fn section(title: &str, subtitle: &str) -> Line {
    let mut line = Line::styled(format!(" {title} "), accent().bold());
    line.push(subtitle, muted());
    line
}
