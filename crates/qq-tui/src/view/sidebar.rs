use super::*;
use layout::Tier;
pub(super) use qq_client::state::{Group, Need};

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

pub(super) const RAIL_PADDING: usize = 2;

pub(super) fn rail_glyph(app: &App, session_id: SessionId) -> (&'static str, Style) {
    let view = &app.sessions[&session_id];
    match view.need() {
        Some(Need::Approval) => ("◇", warning()),
        Some(Need::Failed) => ("✕", failure()),
        Some(Need::FinishedUnread) => ("●", accent()),
        None => match view.summary.status {
            SessionStatus::Running => ("◐", info()),
            SessionStatus::Queued => ("○", warning()),
            SessionStatus::Idle => match view.summary.last_outcome.as_ref() {
                Some(qq_protocol::RunOutcome::Failed { .. }) => ("✕", failure()),
                Some(qq_protocol::RunOutcome::Completed) => ("●", faint()),
                Some(_) => ("◌", faint()),
                None => ("○", faint()),
            },
        },
    }
}

const fn rail_group_label(group: Group) -> &'static str {
    match group {
        Group::NeedsYou => "NEEDS YOU",
        Group::Working => "WORKING",
        Group::Idle | Group::Done => "RECENT",
    }
}

pub(super) fn sidebar(app: &App, width: usize, height: usize, tier: Tier) -> Vec<Line> {
    let wide = tier >= Tier::Wide;
    let inner = width.saturating_sub(2 * RAIL_PADDING);
    let fill = |line: Line| {
        let mut row = Line::styled(" ".repeat(RAIL_PADDING), normal());
        for span in truncate_line(line, inner).spans {
            row.push(span.text, span.style);
        }
        pad_line(&mut row, width);
        for span in &mut row.spans {
            span.style = surface(span.style);
        }
        row
    };
    let (entries, _) = rail_entries(app);
    enum Row {
        Blank,
        Header(&'static str),
        Session(usize),
        Detail(usize),
    }
    let mut plan: Vec<Row> = vec![Row::Blank];
    let mut focused_row = 0;
    let mut current: Option<&str> = None;
    for (index, entry) in entries.iter().enumerate() {
        let label = rail_group_label(entry.group);
        if current != Some(label) {
            if current.is_some() {
                plan.push(Row::Blank);
            }
            current = Some(label);
            plan.push(Row::Header(label));
        }
        if app.focused() == Some(entry.id) {
            focused_row = plan.len();
        }
        plan.push(Row::Session(index));
        if wide && (entry.live || entry.cost.is_some()) {
            plan.push(Row::Detail(index));
        }
    }
    let start = focused_row
        .saturating_sub(height / 2)
        .min(plan.len().saturating_sub(height));
    let render = |row: &Row| -> Line {
        match *row {
            Row::Blank => Line::default(),
            Row::Header(label) => Line::styled(format!("  {label}"), faint()),
            Row::Session(index) => {
                let entry = &entries[index];
                let focused = app.focused() == Some(entry.id);
                let view = &app.sessions[&entry.id];
                let mut line = if focused {
                    Line::styled("▎ ", accent())
                } else {
                    Line::styled("  ", normal())
                };
                line.push(
                    if index < 9 {
                        format!("{} ", index + 1)
                    } else {
                        "  ".to_owned()
                    },
                    faint(),
                );
                let (glyph, glyph_style) = rail_glyph(app, entry.id);
                line.push(format!("{glyph} "), glyph_style);
                let agents = app
                    .sessions
                    .children_of(entry.id)
                    .iter()
                    .filter(|child| app.sessions.contains_key(child))
                    .count();
                let badge = if agents > 0 {
                    format!("  ↳{agents}")
                } else if entry.unread > 0 {
                    format!("  {} new", entry.unread)
                } else {
                    String::new()
                };
                let room = inner.saturating_sub(line.width() + text_width(&badge));
                line.push(
                    fit_text(&view.summary.title, room),
                    if focused { normal() } else { muted() },
                );
                line.push(badge, faint());
                line
            }
            Row::Detail(index) => {
                let entry = &entries[index];
                let activity = live_status_line(app, entry.id).map(|(text, _)| text);
                let cost = entry.cost.map(format_cost);
                let mut line = Line::styled("      ", normal());
                let cost_width = cost.as_deref().map_or(0, |cost| text_width(cost) + 2);
                let room = inner.saturating_sub(line.width() + cost_width);
                line.push(
                    fit_text(activity.as_deref().unwrap_or_default(), room),
                    faint(),
                );
                if let Some(cost) = cost {
                    pad_line(&mut line, inner.saturating_sub(text_width(&cost)));
                    line.push(cost, faint());
                }
                line
            }
        }
    };
    let mut lines: Vec<Line> = plan
        .iter()
        .skip(start)
        .take(height)
        .map(|row| fill(render(row)))
        .collect();
    while lines.len() < height {
        lines.push(fill(Line::default()));
    }
    lines
}

pub(super) fn fit_text(text: &str, width: usize) -> String {
    let plain = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text_width(&plain) <= width {
        return plain;
    }
    if width == 0 {
        return String::new();
    }
    let mut kept = String::new();
    for character in plain.chars() {
        let next = text_width(&kept)
            + unicode_width::UnicodeWidthChar::width(character).unwrap_or_default();
        if next + 1 > width {
            break;
        }
        kept.push(character);
    }
    format!("{}…", kept.trim_end())
}

pub(super) fn child_rows(app: &App, tool_call_id: ToolCallId, width: usize) -> Vec<Line> {
    let Some(child) = app.sessions.child_spawned_by(tool_call_id) else {
        return Vec::new();
    };
    let view = &app.sessions[&child];
    let (glyph, style) = rail_glyph(app, child);
    let mut line = Line::styled("└ ", faint());
    line.push(format!("{glyph} "), style);
    line.push(view.summary.title.clone(), normal());
    let activity = live_status_line(app, child)
        .map(|(text, _)| text)
        .or_else(|| {
            let calls = view
                .runs
                .values()
                .map(|stats| stats.tool_calls)
                .sum::<u32>();
            (calls > 0).then(|| format!("done · {}", count_noun(calls as usize, "call", "calls")))
        });
    if let Some(activity) = activity {
        line.push("   ", normal());
        let room = width.saturating_sub(line.width());
        line.push(fit_text(&activity, room), muted());
    }
    vec![truncate_line(line, width)]
}

pub(super) fn live_status_line(app: &App, session_id: SessionId) -> Option<(String, Style)> {
    let session = app.sessions.get(&session_id)?;
    let live = &session.live;
    if !live.awaiting_approval.is_empty() {
        let tool = match live.active_tool.as_deref() {
            Some("shell" | "exec") => "run a command",
            Some("edit_file" | "write_file") => "edit a file",
            Some("web_fetch") => "fetch a URL",
            Some(_) | None => "use a tool",
        };
        return Some((format!("wants to {tool}"), warning()));
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
            Some(qq_protocol::RunActivity::WaitingForProvider) | None => "waiting for model",
            Some(qq_protocol::RunActivity::Reasoning) => "thinking",
            Some(qq_protocol::RunActivity::GeneratingResponse) => "writing",
            Some(qq_protocol::RunActivity::PreparingToolCall) => "preparing a tool call",
            Some(qq_protocol::RunActivity::Compacting) => "compacting context",
        };
        return Some((label.to_owned(), muted()));
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
        "read_file" => "reading",
        "tree" | "list_dir" => "listing",
        "search" => "searching",
        "read_tool_result" => "recalling",
        "edit_file" => "editing",
        "write_file" => "writing",
        "shell" | "exec" => "running",
        "spawn_agent" => "spawning",
        "web_fetch" => "fetching",
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
