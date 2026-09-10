use super::*;
use qq_client::state::Group;

pub(super) fn session_line(app: &App, session_id: SessionId, width: usize, prefix: &str) -> Line {
    let session = &app.sessions[&session_id].summary;
    // The same state vocabulary tool rows use: ● done, ✕ failed, ◌ stopped
    // early, ◇ waiting, the shared spinner while running.
    let awaiting = !app.sessions[&session_id].live.awaiting_approval.is_empty();
    let (marker, style) = match session.status {
        _ if awaiting => ("◇", warning()),
        SessionStatus::Idle => match session.last_outcome.as_ref() {
            Some(qq_protocol::RunOutcome::Completed) => ("●", success()),
            Some(qq_protocol::RunOutcome::Cancelled | qq_protocol::RunOutcome::Interrupted) => {
                ("◌", warning())
            }
            Some(qq_protocol::RunOutcome::BudgetExhausted { .. }) => ("◌", warning()),
            Some(qq_protocol::RunOutcome::Failed { .. }) => ("✕", failure()),
            None => ("○", muted()),
        },
        SessionStatus::Queued => ("○", warning()),
        SessionStatus::Running => (spinner(app.animation_tick), info()),
    };
    let mut line = Line::styled(prefix, muted());
    line.push(format!("{marker} "), style);
    line.push(
        &session.title,
        if app.focused() == Some(session_id) {
            normal().bold()
        } else {
            normal()
        },
    );
    if session.queued_prompts > 0 {
        line.push(format!("  {} queued", session.queued_prompts), warning());
    }
    truncate_line(line, width)
}

/// Right-hand session list grouped by what the user should do about each:
/// NEEDS YOU (approvals, failures, unread finishes), WORKING, IDLE, DONE.
/// Within a group, sessions keep tree order. Each session takes one row
/// plus one row of live status when it has anything to say. The focused row
/// sits on the selection background. Always `height` rows so it zips
/// against the body.
pub(super) fn sidebar(app: &App, width: usize, height: usize) -> Vec<Line> {
    let inner = width.saturating_sub(2);
    let mut rows: Vec<Line> = Vec::new();
    let mut focused_row = 0;
    // One pass buckets sessions by group in tree order; the sidebar is
    // drawn for every frame with 200 sessions listed.
    let mut buckets: [Vec<SessionId>; 4] = Default::default();
    for &id in app.sessions.thread_order() {
        let bucket = match app.sessions[&id].group() {
            Group::NeedsYou => 0,
            Group::Working => 1,
            Group::Idle => 2,
            Group::Done => 3,
        };
        buckets[bucket].push(id);
    }
    // Row plan first: each entry is a header, a session, its status, or a
    // gap. Only the entries inside the scrolled window are drawn, so the
    // cost is per visible row, not per session.
    enum Entry {
        Gap,
        Header(Group, usize),
        Session(SessionId),
        Status(SessionId, String, Style),
    }
    let mut plan: Vec<Entry> = Vec::new();
    for (group, members) in [Group::NeedsYou, Group::Working, Group::Idle, Group::Done]
        .into_iter()
        .zip(buckets)
    {
        if members.is_empty() {
            continue;
        }
        if !plan.is_empty() {
            plan.push(Entry::Gap);
        }
        plan.push(Entry::Header(group, members.len()));
        for session_id in members {
            if app.focused() == Some(session_id) {
                focused_row = plan.len();
            }
            plan.push(Entry::Session(session_id));
            if let Some((text, style)) = live_status_line(app, session_id) {
                plan.push(Entry::Status(session_id, text, style));
            }
        }
    }
    if plan.is_empty() {
        rows.push(Line::styled("│   no sessions yet", muted().italic()));
    }
    let start = focused_row
        .saturating_sub(height / 2)
        .min(plan.len().saturating_sub(height));
    for entry in plan.into_iter().skip(start).take(height) {
        rows.push(match entry {
            Entry::Gap => Line::styled("│", border()),
            Entry::Header(group, count) => {
                let mut header = Line::styled("│ ", border());
                header.push(
                    group.label(),
                    match group {
                        Group::NeedsYou => warning().bold(),
                        Group::Working => info().bold(),
                        Group::Idle | Group::Done => muted().bold(),
                    },
                );
                header.push(format!("  {count}"), muted());
                truncate_line(header, width)
            }
            Entry::Session(session_id) => {
                let depth = app.sessions.depth(session_id);
                let indent = "  ".repeat(depth.min(4));
                let focused = app.focused() == Some(session_id);
                let mut line = session_line(app, session_id, width, &format!("│ {indent}"));
                let unread = app.sessions[&session_id].unread;
                if unread > 0 && !focused {
                    line.push(format!("  {unread} new"), accent());
                }
                if focused {
                    pad_line(&mut line, width);
                    for span in &mut line.spans[1..] {
                        span.style = selection(span.style);
                    }
                }
                truncate_line(line, width)
            }
            Entry::Status(session_id, text, style) => {
                let depth = app.sessions.depth(session_id);
                let indent = "  ".repeat(depth.min(4));
                let mut line = Line::styled(format!("│ {indent}   "), muted());
                let used = line.width();
                line.push(preview(&text, inner.saturating_sub(used)), style);
                truncate_line(line, width)
            }
        });
    }
    let mut lines = rows;
    while lines.len() < height {
        lines.push(Line::styled("│", border()));
    }
    lines.truncate(height);
    lines
}

/// One row above the composer when the sidebar is hidden and more than one
/// session exists: how many agents there are and how many need the user,
/// are working, or finished unseen, with the chord that jumps to them.
pub(super) fn agent_strip(app: &App, width: usize) -> Option<Line> {
    let total = app.sessions.values().count();
    if total < 2 {
        return None;
    }
    let mut needs = 0;
    let mut working = 0;
    let mut unread = 0;
    for session in app.sessions.values() {
        match session.group() {
            Group::NeedsYou => needs += 1,
            Group::Working => working += 1,
            Group::Idle | Group::Done => {}
        }
        if session.unread > 0 && app.focused() != Some(session.summary.id) {
            unread += 1;
        }
    }
    let mut line = Line::styled(format!(" {total} agents"), muted());
    if working > 0 {
        line.push("  ", muted());
        line.push(format!("{} {working}", spinner(app.animation_tick)), info());
    }
    if needs > 0 {
        line.push("  ", muted());
        line.push(format!("◇ {needs}"), warning().bold());
        if let Some(chord) = app.chord_label(crate::commands::Command::FocusNextApproval) {
            line.push(format!(" ({chord})"), muted());
        }
    }
    if unread > 0 {
        line.push("  ", muted());
        line.push(format!("● {unread} unread"), accent());
    }
    Some(truncate_line(line, width))
}

/// Rows for the child session a `spawn_agent` call created: its title and
/// status glyph, then one status line (approval wait, active tool, live tail,
/// or activity). Empty when the call has no recorded child.
pub(super) fn child_rows(app: &App, tool_call_id: ToolCallId, width: usize) -> Vec<Line> {
    let Some(child) = app.sessions.child_spawned_by(tool_call_id) else {
        return Vec::new();
    };
    let view = &app.sessions[&child];
    // `↳ title  ◐ current tool · 3 tools · 41s`: the card is the child's
    // one-line status while it works and its outcome once it is done.
    let mut line = session_line(app, child, width, "       ↳ ");
    let mut parts: Vec<String> = Vec::new();
    if let Some(tool) = &view.live.active_tool {
        parts.push(tool.clone());
    }
    let calls = view
        .runs
        .values()
        .map(|stats| stats.tool_calls)
        .sum::<u32>();
    if calls > 0 {
        parts.push(count_noun(calls as usize, "tool", "tools"));
    }
    if let Some(stats) = view
        .summary
        .active_run_id
        .and_then(|run| view.runs.get(&run))
        && let Some(started) = stats.started_at_ms
    {
        parts.push(format_duration_ms(app.now_ms.saturating_sub(started)));
    }
    if view.unread > 0 && app.focused() != Some(child) {
        parts.push(format!("{} new", view.unread));
    }
    if !parts.is_empty() {
        line.push(format!("  {}", parts.join(" · ")), muted());
    }
    let mut rows = vec![truncate_line(line, width)];
    if let Some((text, style)) = live_status_line(app, child)
        && view.live.active_tool.is_none()
    {
        let mut line = Line::styled("            ", muted());
        let used = line.width();
        line.push(preview(&text, width.saturating_sub(used)), style);
        rows.push(truncate_line(line, width));
    }
    rows
}

/// One-line live status for a session row, most urgent first.
pub(super) fn live_status_line(app: &App, session_id: SessionId) -> Option<(String, Style)> {
    let session = app.sessions.get(&session_id)?;
    let live = &session.live;
    if !live.awaiting_approval.is_empty() {
        let tool = live.active_tool.as_deref().map_or("tool", tool_verb);
        return Some((format!("◇ approve {tool}"), warning().bold()));
    }
    if session.summary.status == SessionStatus::Running {
        if let Some(tool) = &live.active_tool {
            return Some((tool_verb(tool).to_owned(), accent()));
        }
        if !live.tail.is_empty() {
            return Some((live.tail.clone(), muted()));
        }
        let label = match session.activity.map(|(_, activity)| activity) {
            Some(qq_protocol::RunActivity::WaitingForProvider) | None => "waiting for provider",
            Some(qq_protocol::RunActivity::Reasoning) => "reasoning",
            Some(qq_protocol::RunActivity::GeneratingResponse) => "responding",
            Some(qq_protocol::RunActivity::PreparingToolCall) => "preparing a tool call",
        };
        return Some((label.to_owned(), muted().italic()));
    }
    if session.summary.queued_prompts > 0 {
        return Some((
            format!("{} queued", session.summary.queued_prompts),
            warning(),
        ));
    }
    if app.focused() != Some(session_id) && !live.tail.is_empty() {
        return Some((live.tail.clone(), muted()));
    }
    None
}

/// The verb a tool call row would show for `name`, lower-cased for prose:
/// the sidebar says `edit`, not `edit_file`. MCP tools show their tool part.
fn tool_verb(name: &str) -> &str {
    match name {
        "read_file" => "read",
        "list_dir" => "list",
        "search" => "search",
        "edit_file" => "edit",
        "write_file" => "write",
        "shell" => "run",
        "spawn_agent" => "spawn",
        "web_fetch" => "fetch",
        other => other
            .strip_prefix("mcp__")
            .and_then(|rest| rest.split_once("__"))
            .map_or(other, |(_, tool)| tool),
    }
}

/// Extend `line` with spaces to exactly `width` display columns.
pub(super) fn pad_line(line: &mut Line, width: usize) {
    let used = line.width();
    if used < width {
        line.push(" ".repeat(width - used), normal());
    }
}
