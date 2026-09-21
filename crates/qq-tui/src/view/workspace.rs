use super::*;

/// The attention pane: every reason a session needs the user across the
/// workspace, most urgent first. Each item is one row (session and reason)
/// plus one row of detail (the waiting command or the outcome). Enter on the
/// focused pane jumps to the session; approvals answer in place with the
/// background chords.
pub(super) fn attention_body(app: &App, width: usize) -> Vec<Line> {
    let mut lines = vec![section("NEEDS YOU", "most urgent first"), Line::default()];
    let needing = app.sessions_needing_attention();
    if needing.is_empty() {
        lines.push(Line::styled(
            "  Nothing needs you. Every agent is working or done.",
            muted().italic(),
        ));
        return lines;
    }
    for session_id in needing {
        let view = &app.sessions[&session_id];
        let Some(need) = view.need() else {
            continue;
        };
        let (glyph, style, label) = match need {
            qq_client::state::Need::Approval => ("◇", warning(), "needs approval"),
            qq_client::state::Need::Failed => ("✕", failure(), "failed"),
            qq_client::state::Need::FinishedUnread => ("●", success(), "finished"),
        };
        let mut line = Line::styled(format!("  {glyph} "), style);
        line.push(&view.summary.title, normal().bold());
        line.push(format!("  {label}"), style);
        if view.unread > 0 {
            line.push(format!("  {} new", view.unread), accent());
        }
        lines.push(truncate_line(line, width));
        let detail: Option<(String, Style)> = match need {
            qq_client::state::Need::Approval => view
                .tool_calls
                .as_ref()
                .and_then(|calls| {
                    calls
                        .iter()
                        .find(|call| call.state == ToolCallState::AwaitingApproval)
                })
                .map(|call| {
                    let row = ToolRow::derive(call);
                    let subject = row.subject.unwrap_or_default();
                    (format!("{} {subject}", row.verb), normal())
                })
                .or_else(|| {
                    view.live
                        .active_tool
                        .as_ref()
                        .map(|tool| (tool.clone(), normal()))
                }),
            qq_client::state::Need::Failed => match &view.summary.last_outcome {
                Some(qq_protocol::RunOutcome::Failed { failure }) => {
                    Some((failure.message.clone(), failure_style()))
                }
                _ => None,
            },
            qq_client::state::Need::FinishedUnread => {
                (!view.live.tail.is_empty()).then(|| (view.live.tail.clone(), muted()))
            }
        };
        if let Some((text, style)) = detail {
            let mut line = Line::styled("      ", muted());
            line.push(preview(&text, width.saturating_sub(6)), style);
            lines.push(truncate_line(line, width));
        }
    }
    lines.push(Line::default());
    let mut hint = Line::styled("  ", muted());
    if let Some(chord) = app.chord_label(crate::commands::Command::FocusNextApproval) {
        hint.push(format!("{chord} jumps"), muted());
    }
    if let (Some(approve), Some(deny)) = (
        app.chord_label(crate::commands::Command::ApproveBackground),
        app.chord_label(crate::commands::Command::DenyBackground),
    ) {
        hint.push(format!("  {approve} approves  {deny} denies"), muted());
    }
    lines.push(truncate_line(hint, width));
    lines
}

fn failure_style() -> Style {
    failure()
}

/// The changes pane: every file any agent edited in a loaded session,
/// grouped by path with per-agent `+N −M`. A path touched by more than one
/// agent is flagged so overlapping work is visible before it collides.
pub(super) fn changes_body(app: &App, width: usize) -> Vec<Line> {
    let mut lines = vec![
        section("CHANGES", "every edit across agents"),
        Line::default(),
    ];
    let mut by_path: Vec<(String, Vec<AgentEdit>)> = Vec::new();
    for session_id in app.sessions.thread_order() {
        let view = &app.sessions[session_id];
        let Some(calls) = view.tool_calls.as_ref() else {
            continue;
        };
        for call in calls {
            let Some(ToolCallDisplay::Diff { path, diff }) = &call.display else {
                continue;
            };
            let (added, removed) = diff_counts(diff);
            let entry = match by_path.iter_mut().find(|(candidate, _)| candidate == path) {
                Some((_, agents)) => agents,
                None => {
                    by_path.push((path.clone(), Vec::new()));
                    &mut by_path.last_mut().expect("just pushed").1
                }
            };
            match entry
                .iter_mut()
                .find(|edit| edit.agent == view.summary.title)
            {
                Some(edit) => {
                    edit.added += added;
                    edit.removed += removed;
                }
                None => entry.push(AgentEdit {
                    agent: view.summary.title.clone(),
                    added,
                    removed,
                }),
            }
        }
    }
    if by_path.is_empty() {
        lines.push(Line::styled("  No files changed yet.", muted().italic()));
        return lines;
    }
    for (path, agents) in by_path {
        let mut line = Line::styled("  ", muted());
        let conflict = agents.len() > 1;
        line.push(
            if conflict { "! " } else { "● " },
            if conflict {
                warning().bold()
            } else {
                success()
            },
        );
        line.push(elide_path(&path, width.saturating_sub(20)), normal().bold());
        let (added, removed) = agents
            .iter()
            .fold((0, 0), |(a, r), edit| (a + edit.added, r + edit.removed));
        line.push(format!("  +{added} −{removed}"), muted());
        if conflict {
            line.push(format!("  {} agents", agents.len()), warning());
        }
        lines.push(truncate_line(line, width));
        for edit in agents {
            let mut line = Line::styled("      ↳ ", muted());
            line.push(edit.agent, normal());
            line.push(format!("  +{} −{}", edit.added, edit.removed), muted());
            lines.push(truncate_line(line, width));
        }
    }
    lines
}

/// One agent's net edit to a path on the change board.
struct AgentEdit {
    agent: String,
    added: usize,
    removed: usize,
}

fn diff_counts(diff: &str) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
}

/// The inspector column at Wide and above (or wherever it is pinned): the
/// focused pane's detail, beside the transcript so the prose column stays
/// prose. It shows the workspace view the pane is on, or else the expanded
/// body of every expanded tool call in the pane's session, each under its
/// summary row, through the same `tool_expanded_lines` the inline path uses.
/// Always `height` rows so it zips onto the body; rows past the height are
/// dropped behind a count (the inspector does not scroll yet), and no row is
/// built past what fits.
pub(super) fn inspector_pane(
    app: &App,
    cache: &TranscriptCache,
    pane: &TranscriptPane,
    width: usize,
    height: usize,
) -> Vec<Line> {
    let inner = width.saturating_sub(2);
    let mut header = Line::styled("│ ", border());
    header.push("INSPECTOR", muted().bold());
    let body_height = height.saturating_sub(1);
    let mut rows: Vec<Line> = Vec::new();
    match pane.view {
        View::Attention => rows = attention_body(app, inner),
        View::Changes => rows = changes_body(app, inner),
        View::Transcript(session_id) => {
            let calls = session_id
                .and_then(|session_id| app.sessions.get(&session_id))
                .and_then(|session| Some((session, session.tool_calls.as_deref()?)));
            if let Some((session, calls)) = calls {
                for call in calls {
                    if rows.len() >= body_height {
                        break;
                    }
                    if !app.expanded_tool_calls.contains(&call.id) {
                        continue;
                    }
                    // Rows are derived for the transcript this frame; a call
                    // the focused pane did not lay out (an overlay is up)
                    // derives once here.
                    let derived;
                    let row = match cache.tool_row(call) {
                        Some(row) => row,
                        None => {
                            derived = ToolRow::derive(call);
                            &derived
                        }
                    };
                    let context = ToolRowContext {
                        row,
                        clock: RowClock {
                            timing: session
                                .tool_timing
                                .get(&call.id)
                                .copied()
                                .unwrap_or_default(),
                            now_ms: app.now_ms,
                        },
                        expanded: true,
                        inline_detail: true,
                        fold: false,
                        selected: app.transcript_cursor == Some(call.id),
                    };
                    if !rows.is_empty() {
                        rows.push(Line::default());
                    }
                    rows.push(tool_summary_line(call, context, app.animation_tick, inner));
                    rows.extend(tool_expanded_lines(call, context, inner));
                }
            }
            if rows.is_empty() {
                let chord = app
                    .chord_label(crate::commands::Command::CursorUp)
                    .unwrap_or_else(|| "Ctrl-Up".to_owned());
                rows.push(Line::styled(
                    format!("Nothing expanded — {chord} selects a tool row, Enter expands it"),
                    muted().italic(),
                ));
            }
        }
    }
    let overflow = rows.len().saturating_sub(body_height);
    if overflow > 0 {
        rows.truncate(body_height.saturating_sub(1));
        rows.push(Line::styled(
            format!("… {} more", count_noun(overflow + 1, "row", "rows")),
            muted(),
        ));
    }
    let mut lines = Vec::with_capacity(height);
    lines.push(truncate_line(header, width));
    lines.extend(indent_lines(rows, "│ ", border(), width));
    let rule = Line::styled("│", border());
    while lines.len() < height {
        lines.push(rule.clone());
    }
    lines.truncate(height);
    lines
}
