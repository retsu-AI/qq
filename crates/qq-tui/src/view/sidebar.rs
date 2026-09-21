use super::*;
use layout::Tier;
use qq_client::state::{Group, Need};

pub(super) fn session_line(app: &App, session_id: SessionId, width: usize, prefix: &str) -> Line {
    let view = &app.sessions[&session_id];
    let session = &view.summary;
    // The same state vocabulary tool rows use: ● done, ✕ failed, ◌ stopped
    // early, ◇ waiting, the shared spinner while running. Color follows the
    // attention rule: `warning` for a pending approval, `error` for a
    // failure, `accent` for a finish the user has not looked at, muted for
    // everything settled.
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
                    | qq_protocol::RunOutcome::BudgetExhausted { .. },
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

/// One session as the rail and the strip see it: which group it lists
/// under, the unread count the badge shows, and what its detail row would
/// carry. Produced by [`rail_entries`], the one pass both surfaces share;
/// everything the row planner asks is here so planning 200 sessions costs
/// no further lookups. Tree depth is read only for the rows drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RailEntry {
    pub id: SessionId,
    pub group: Group,
    /// Assistant messages and finishes that arrived while the session was
    /// not shown; zero for the focused session, which has seen everything.
    pub unread: u32,
    /// Whether [`live_status_line`] has a row for it: an approval, a running
    /// tool or activity, queued prompts, or a live tail while unfocused.
    pub live: bool,
    /// The session's own spend when the server has reported one. A known
    /// zero is not shown: nothing to say about a session that has cost
    /// nothing yet.
    pub cost: Option<u64>,
}

/// Every session grouped NEEDS YOU, WORKING, IDLE, DONE, in tree order
/// within each group, plus how many entries each group holds (indexed as
/// [`Group`] is declared). One O(sessions) walk with one lookup per session;
/// the rail lays out rows from it and the strip counts from it, so the two
/// never disagree.
pub(super) fn rail_entries(app: &App) -> (Vec<RailEntry>, [usize; 4]) {
    let mut buckets: [Vec<RailEntry>; 4] = Default::default();
    for &id in app.sessions.thread_order() {
        let session = &app.sessions[&id];
        let group = session.group();
        let focused = app.focused() == Some(id);
        let unread = if focused { 0 } else { session.unread };
        // Must agree with `live_status_line`; the planner relies on it.
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

/// How much each rail row says, chosen by the tier the rail's width follows.
/// Height never changes the density; a short rail shows fewer rows of the
/// same shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RailDensity {
    /// One row per session: state glyph, name, unread badge.
    Compact,
    /// A second muted row per session with the live tail and cost.
    Detailed,
}

impl RailDensity {
    /// The rail's width follows the terminal's: a quarter of it clamped to
    /// 20–28 columns, so the rail is at its full width from 112 columns on
    /// and the tier, not the rail's own cell count, is what separates a
    /// Regular rail from a Wide one. Wide and Ultra get the detail row; a
    /// rail pinned open at Compact is the narrowest and gets the least.
    pub(super) const fn of(tier: Tier) -> Self {
        match tier {
            Tier::Compact | Tier::Regular => Self::Compact,
            Tier::Wide | Tier::Ultra => Self::Detailed,
        }
    }
}

/// Right-hand session list grouped by what the user should do about each:
/// NEEDS YOU (approvals, failures, unread finishes), WORKING, IDLE, DONE.
/// Within a group, sessions keep tree order. Each session takes one row of
/// state glyph, name, and unread badge; at [`RailDensity::Detailed`] a
/// second muted row carries the live tail and the session's cost. The
/// focused row sits on the selection background. Always `height` rows so it
/// zips against the body.
pub(super) fn sidebar(app: &App, width: usize, height: usize, density: RailDensity) -> Vec<Line> {
    let inner = width.saturating_sub(2);
    let mut rows: Vec<Line> = Vec::new();
    let mut focused_row = 0;
    let (entries, counts) = rail_entries(app);
    // Row plan first: each entry is a header, a session, its detail row, or
    // a gap. Only the entries inside the scrolled window are drawn, so the
    // cost is per visible row, not per session.
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
        // Planning asks only whether a detail row exists; the text is built
        // for the rows inside the window, not for every session. A session
        // with nothing to say takes one row at every density so a quiet
        // list stays dense.
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
                // The badge is right-aligned and claims its cells first so a
                // long title truncates instead of pushing the count off the
                // rail; `N new` when the rail has room for it, else `N`.
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
                        // Tail on the left, cost on the right. The tail is
                        // the live status (approval, tool verb, streamed
                        // text, or activity) when the session has one; the
                        // cost is the session's own direct spend and stays
                        // muted so the accent is only ever the badge.
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

/// One row above the composer when the rail is hidden and more than one
/// session exists: how many agents there are and how many need the user,
/// are working, or finished unseen, with the chord that jumps to them. Built
/// from the same [`rail_entries`] pass as the rail.
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

/// Extend `line` with spaces to exactly `width` display columns.
pub(super) fn pad_line(line: &mut Line, width: usize) {
    let used = line.width();
    if used < width {
        line.push(" ".repeat(width - used), normal());
    }
}
