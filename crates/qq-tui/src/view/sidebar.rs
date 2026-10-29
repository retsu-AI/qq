use super::*;
use layout::Tier;
use qq_client::state::{Group, Need};

pub(super) fn session_line(app: &App, session_id: SessionId, width: usize, prefix: &str) -> Line {
    let view = &app.sessions[&session_id];
    let session = &view.summary;
    let (marker, style) = match view.need() {
        Some(Need::Approval) => ("◇", warning()),
        Some(Need::Failed) => ("✕", failure()),
        Some(Need::FinishedUnread) => ("●", accent()),
        None => match session.status {
            SessionStatus::Idle => match session.last_outcome.as_ref() {
                Some(qq_protocol::RunOutcome::Completed) => ("●", muted()),
                Some(
                    qq_protocol::RunOutcome::Cancelled
                    | qq_protocol::RunOutcome::Interrupted
                    | qq_protocol::RunOutcome::BudgetExhausted { .. }
                    | qq_protocol::RunOutcome::Paused { .. },
                ) => ("◌", muted()),
                Some(qq_protocol::RunOutcome::Failed { .. }) => ("✕", failure()),
                None => ("○", muted()),
            },
            SessionStatus::Queued => ("○", warning()),
            SessionStatus::Running => (spinner(app.animation_tick), info()),
        },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RailEntry {
    pub id: SessionId,
    pub group: Group,
    pub unread: u32,
    pub live: bool,
    pub cost: Option<u64>,
}

pub(super) fn rail_entries(app: &App) -> (Vec<RailEntry>, [usize; 4]) {
    let mut buckets: [Vec<RailEntry>; 4] = Default::default();
    for &id in app.sessions.thread_order() {
        let session = &app.sessions[&id];
        let group = session.group();
        let focused = app.focused() == Some(id);
        let unread = if focused { 0 } else { session.unread };
        let live = !session.live.awaiting_approval.is_empty()
            || session.summary.status == SessionStatus::Running
            || session.summary.queued_prompts > 0
            || (!focused && !session.live.tail.is_empty());
        let cost = session
            .summary
            .accounting
            .map(|accounting| accounting.direct.estimated_cost_usd_nanos)
            .unwrap_or(session.summary.estimated_cost_usd_nanos)
            .filter(|nanos| *nanos > 0);
        buckets[group as usize].push(RailEntry {
            id,
            group,
            unread,
            live,
            cost,
        });
    }
    let counts = [
        buckets[0].len(),
        buckets[1].len(),
        buckets[2].len(),
        buckets[3].len(),
    ];
    let mut entries = Vec::with_capacity(counts.iter().sum());
    for bucket in buckets {
        entries.extend(bucket);
    }
    (entries, counts)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RailDensity {
    Compact,
    Detailed,
}

impl RailDensity {
    pub(super) const fn of(tier: Tier) -> Self {
        match tier {
            Tier::Compact | Tier::Regular => Self::Compact,
            Tier::Wide | Tier::Ultra => Self::Detailed,
        }
    }
}

pub(super) fn sidebar(app: &App, width: usize, height: usize, density: RailDensity) -> Vec<Line> {
    let inner = width.saturating_sub(2);
    let mut rows: Vec<Line> = Vec::new();
    let mut focused_row = 0;
    let (entries, counts) = rail_entries(app);
    enum Entry {
        Gap,
        Header(Group, usize),
        Session(RailEntry),
        Detail(RailEntry),
    }
    let mut plan: Vec<Entry> = Vec::with_capacity(entries.len() * 2 + 8);
    let mut current: Option<Group> = None;
    for entry in entries {
        if current != Some(entry.group) {
            current = Some(entry.group);
            if !plan.is_empty() {
                plan.push(Entry::Gap);
            }
            plan.push(Entry::Header(entry.group, counts[entry.group as usize]));
        }
        if app.focused() == Some(entry.id) {
            focused_row = plan.len();
        }
        plan.push(Entry::Session(entry));
        let detail = match density {
            RailDensity::Compact => entry.live,
            RailDensity::Detailed => entry.live || entry.cost.is_some(),
        };
        if detail {
            plan.push(Entry::Detail(entry));
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
            Entry::Session(entry) => {
                let indent = "  ".repeat(app.sessions.depth(entry.id).min(4));
                let focused = app.focused() == Some(entry.id);
                let badge = if entry.unread > 0 {
                    if width >= layout::RAIL_MIN_WIDTH + 6 {
                        format!("{} new", entry.unread)
                    } else {
                        entry.unread.to_string()
                    }
                } else {
                    String::new()
                };
                let title_width = width.saturating_sub(if badge.is_empty() {
                    0
                } else {
                    badge.chars().count() + 3
                });
                let mut line = session_line(app, entry.id, title_width, &format!("│ {indent}"));
                if !badge.is_empty() {
                    pad_line(&mut line, title_width + 2);
                    line.push(badge, accent());
                }
                if focused {
                    pad_line(&mut line, width);
                    for span in &mut line.spans[1..] {
                        span.style = selection(span.style);
                    }
                }
                truncate_line(line, width)
            }
            Entry::Detail(entry) => {
                let indent = "  ".repeat(app.sessions.depth(entry.id).min(4));
                let mut line = Line::styled(format!("│ {indent}   "), muted());
                let used = line.width();
                match density {
                    RailDensity::Compact => {
                        let (text, style) = live_status_line(app, entry.id)
                            .unwrap_or_else(|| (String::new(), muted()));
                        line.push(preview(&text, inner.saturating_sub(used)), style);
                    }
                    RailDensity::Detailed => {
                        let cost = entry.cost.map(format_cost);
                        let cost_width = cost.as_ref().map_or(0, |cost| cost.chars().count() + 3);
                        let tail_width = inner.saturating_sub(used).saturating_sub(cost_width);
                        let (text, style) = live_status_line(app, entry.id)
                            .unwrap_or_else(|| (String::new(), muted()));
                        line.push(preview(&text, tail_width), style);
                        if let Some(cost) = cost {
                            pad_line(&mut line, width.saturating_sub(cost.chars().count() + 1));
                            line.push(cost, muted());
                        }
                    }
                }
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

pub(super) fn agent_strip(app: &App, width: usize) -> Option<Line> {
    let (entries, counts) = rail_entries(app);
    if entries.len() < 2 {
        return None;
    }
    let needs = counts[Group::NeedsYou as usize];
    let working = counts[Group::Working as usize];
    let unread = entries.iter().filter(|entry| entry.unread > 0).count();
    let mut line = Line::styled(format!(" {} agents", entries.len()), muted());
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

pub(super) fn child_rows(app: &App, tool_call_id: ToolCallId, width: usize) -> Vec<Line> {
    let Some(child) = app.sessions.child_spawned_by(tool_call_id) else {
        return Vec::new();
    };
    let view = &app.sessions[&child];
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
        let activity = session.activity.map(|(_, activity)| activity);
        if !live.tail.is_empty() && activity != Some(qq_protocol::RunActivity::Compacting) {
            return Some((live.tail.clone(), muted()));
        }
        let label = match activity {
            Some(qq_protocol::RunActivity::WaitingForProvider) | None => "waiting for provider",
            Some(qq_protocol::RunActivity::Reasoning) => "reasoning",
            Some(qq_protocol::RunActivity::GeneratingResponse) => "responding",
            Some(qq_protocol::RunActivity::PreparingToolCall) => "preparing a tool call",
            Some(qq_protocol::RunActivity::Compacting) => "compacting context",
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

fn tool_verb(name: &str) -> &str {
    match name {
        "read_file" => "read",
        "tree" | "list_dir" => "list",
        "search" => "search",
        "read_tool_result" => "recall",
        "edit_file" => "edit",
        "write_file" => "write",
        "shell" | "exec" => "run",
        "spawn_agent" => "spawn",
        "web_fetch" => "fetch",
        other => other
            .strip_prefix("mcp__")
            .and_then(|rest| rest.split_once("__"))
            .map_or(other, |(_, tool)| tool),
    }
}

pub(super) fn pad_line(line: &mut Line, width: usize) {
    let used = line.width();
    if used < width {
        line.push(" ".repeat(width - used), normal());
    }
}
