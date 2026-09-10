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
