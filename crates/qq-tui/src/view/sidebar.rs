use super::*;
use layout::Tier;
use std::ops::Range;

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

/// Which rail group a session lists under. Only sessions are grouped: an
/// agent's state rises into its session's group, so a session whose agent
/// waits on an approval lists under NEEDS YOU even while the session itself
/// is idle. RECENT holds everything settled, done and never-run alike; the
/// row's glyph keeps the two apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum RailGroup {
    NeedsYou,
    Working,
    Recent,
}

impl RailGroup {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::NeedsYou => "NEEDS YOU",
            Self::Working => "WORKING",
            Self::Recent => "RECENT",
        }
    }
}

/// Agent rows listed under the focused session before the rest collapse to
/// a `+N more` row: the focused agent first, then waiting agents, then quiet
/// agents in tree order.
pub(super) const FOCUSED_AGENT_ROWS: usize = 6;

/// Waiting agents listed under an unfocused session before the rest collapse
/// to a `+N waiting` row, so one wide fan-out cannot crowd out the list.
pub(super) const WAITING_AGENT_ROWS: usize = 3;

/// One session as the rail and the strip see it, with its agents rolled up.
/// Produced by [`rollup`], the one pass both surfaces share; everything the
/// row planner asks is here so planning 200 sessions costs no further
/// lookups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RailEntry {
    pub id: SessionId,
    pub group: RailGroup,
    /// Assistant messages and finishes that arrived while the session was
    /// not shown; zero for the focused session, which has seen everything.
    pub unread: u32,
    /// Whether [`live_status_line`] has a row for the session itself.
    pub live: bool,
    /// The session's spend including its agents, when reported and nonzero.
    pub cost: Option<u64>,
    /// Agents under the session, audit agents excluded.
    pub agents: u32,
    /// Of those, how many are running or queued.
    pub agents_working: u32,
}

/// Every session grouped NEEDS YOU, WORKING, RECENT (tree order, newest
/// session first, within a group), how many each group holds, the agents
/// waiting on the user, and where the focused session's tree sits in
/// `thread_order`. One O(sessions) walk.
#[derive(Debug, Default)]
pub(super) struct Rollup {
    pub sessions: Vec<RailEntry>,
    pub counts: [usize; 3],
    /// `(session, agent)` for every agent with an approval waiting, in tree
    /// order.
    pub waiting: Vec<(SessionId, SessionId)>,
    /// The session whose tree holds the focused session, and the range of
    /// `thread_order` its agents occupy.
    pub focused_tree: Option<(SessionId, Range<usize>)>,
}

impl Rollup {
    fn waiting_under(&self, session: SessionId) -> impl Iterator<Item = SessionId> + '_ {
        self.waiting
            .iter()
            .filter(move |(root, _)| *root == session)
            .map(|(_, agent)| *agent)
    }
}

/// Whether a session exists for bookkeeping rather than for the user: audit
/// agents check a parent's final answer and are never listed.
fn is_audit(session: &SessionView) -> bool {
    session.summary.purpose == qq_protocol::SessionPurpose::Audit
}

/// The session being accumulated by [`rollup`]: its entry, its own group,
/// where its agents start in `thread_order`, and whether one waits.
struct Pending {
    entry: RailEntry,
    own: Group,
    start: usize,
    waiting: bool,
}

pub(super) fn rollup(app: &App) -> Rollup {
    let focused = app.focused();
    let order = app.sessions.thread_order();
    let mut buckets: [Vec<RailEntry>; 3] = Default::default();
    let mut rollup = Rollup::default();
    let mut current: Option<Pending> = None;
    let mut audit_depth: Option<usize> = None;
    let mut focused_root: Option<SessionId> = None;
    let mut finish =
        |pending: Pending, end: usize, focused_root: Option<SessionId>, rollup: &mut Rollup| {
            let Pending {
                entry,
                own,
                start,
                waiting,
            } = pending;
            let group = if own == Group::NeedsYou || waiting {
                RailGroup::NeedsYou
            } else if own == Group::Working || entry.agents_working > 0 {
                RailGroup::Working
            } else {
                RailGroup::Recent
            };
            if focused_root == Some(entry.id) {
                rollup.focused_tree = Some((entry.id, start..end));
            }
            buckets[group as usize].push(RailEntry { group, ..entry });
        };
    for (position, &id) in order.iter().enumerate() {
        let session = &app.sessions[&id];
        let root = session.summary.parent_id.is_none();
        // Everything below an audit agent is bookkeeping too.
        if let Some(depth) = audit_depth {
            if !root && app.sessions.depth(id) > depth {
                continue;
            }
            audit_depth = None;
        }
        if root {
            if let Some(pending) = current.take() {
                finish(pending, position, focused_root, &mut rollup);
            }
            let is_focused = focused == Some(id);
            if is_focused {
                focused_root = Some(id);
            }
            // Must agree with `live_status_line`; the planner relies on it.
            let live = !session.live.awaiting_approval.is_empty()
                || session.summary.status == SessionStatus::Running
                || session.summary.queued_prompts > 0
                || (!is_focused && !session.live.tail.is_empty());
            let cost = session
                .summary
                .accounting
                .map(|accounting| accounting.inclusive.estimated_cost_usd_nanos)
                .unwrap_or(session.summary.estimated_cost_usd_nanos)
                .filter(|nanos| *nanos > 0);
            current = Some(Pending {
                entry: RailEntry {
                    id,
                    group: RailGroup::Recent,
                    unread: if is_focused { 0 } else { session.unread },
                    live,
                    cost,
                    agents: 0,
                    agents_working: 0,
                },
                own: session.group(),
                start: position + 1,
                waiting: false,
            });
            continue;
        }
        // An agent whose session is not loaded has nothing to roll into.
        let Some(pending) = current.as_mut() else {
            continue;
        };
        if focused == Some(id) {
            focused_root = Some(pending.entry.id);
        }
        if is_audit(session) {
            audit_depth = Some(app.sessions.depth(id));
            continue;
        }
        pending.entry.agents += 1;
        match session.summary.status {
            SessionStatus::Running | SessionStatus::Queued => pending.entry.agents_working += 1,
            SessionStatus::Idle => {}
        }
        if !session.live.awaiting_approval.is_empty() {
            pending.waiting = true;
            rollup.waiting.push((pending.entry.id, id));
        }
    }
    if let Some(pending) = current.take() {
        finish(pending, order.len(), focused_root, &mut rollup);
    }
    rollup.counts = [buckets[0].len(), buckets[1].len(), buckets[2].len()];
    rollup.sessions = Vec::with_capacity(rollup.counts.iter().sum());
    for bucket in buckets {
        rollup.sessions.extend(bucket);
    }
    rollup
}

/// How much each rail row says, chosen by the tier the rail's width follows.
/// Height never changes the density; a short rail shows fewer rows of the
/// same shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RailDensity {
    /// One row per session: state glyph, name, agent and unread badges.
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

/// One planned rail row. Only the rows inside the scrolled window are
/// drawn, so the cost is per visible row, not per session.
#[derive(Debug, Clone, Copy)]
pub(super) enum RailRow {
    Gap,
    Header(RailGroup, usize),
    Session(RailEntry),
    Detail(RailEntry),
    Agent(SessionId),
    AgentDetail(SessionId),
    More(usize),
    MoreWaiting(usize),
}

/// Rows planned from one [`rollup`] and the index of the focused row: a row
/// per session, the focused session's agents under it (capped at
/// [`FOCUSED_AGENT_ROWS`]), and every agent waiting on the user under its
/// own session.
pub(super) fn plan_rail(app: &App, density: RailDensity) -> (Vec<RailRow>, usize) {
    let rollup = rollup(app);
    let focused = app.focused();
    let order = app.sessions.thread_order();
    let mut plan: Vec<RailRow> = Vec::with_capacity(rollup.sessions.len() * 2 + 8);
    let mut focused_row = 0;
    let mut current: Option<RailGroup> = None;
    for &entry in &rollup.sessions {
        if current != Some(entry.group) {
            current = Some(entry.group);
            if !plan.is_empty() {
                plan.push(RailRow::Gap);
            }
            plan.push(RailRow::Header(
                entry.group,
                rollup.counts[entry.group as usize],
            ));
        }
        if focused == Some(entry.id) {
            focused_row = plan.len();
        }
        plan.push(RailRow::Session(entry));
        let detail = match density {
            RailDensity::Compact => entry.live,
            RailDensity::Detailed => entry.live || entry.cost.is_some(),
        };
        if detail {
            plan.push(RailRow::Detail(entry));
        }
        let tree = rollup
            .focused_tree
            .as_ref()
            .filter(|(root, _)| *root == entry.id);
        let Some((_, range)) = tree else {
            let mut waiting = rollup.waiting_under(entry.id);
            for agent in waiting.by_ref().take(WAITING_AGENT_ROWS) {
                plan.push(RailRow::Agent(agent));
                plan.push(RailRow::AgentDetail(agent));
            }
            let rest = waiting.count();
            if rest > 0 {
                plan.push(RailRow::MoreWaiting(rest));
            }
            continue;
        };
        // The focused session's agents, at most `FOCUSED_AGENT_ROWS`: the
        // focused agent first, then waiting agents, then quiet agents, each
        // kept in tree order on screen.
        let agents: Vec<SessionId> = order[range.clone()]
            .iter()
            .copied()
            .filter(|id| !app.sessions.get(id).is_some_and(is_audit))
            .collect();
        let rank = |id: SessionId| {
            if focused == Some(id) {
                0
            } else if !app.sessions[&id].live.awaiting_approval.is_empty() {
                1
            } else {
                2
            }
        };
        let mut chosen: Vec<usize> = (0..agents.len()).collect();
        chosen.sort_by_key(|&index| rank(agents[index]));
        chosen.truncate(FOCUSED_AGENT_ROWS);
        chosen.sort_unstable();
        for &index in &chosen {
            let id = agents[index];
            if focused == Some(id) {
                focused_row = plan.len();
            }
            plan.push(RailRow::Agent(id));
            if !app.sessions[&id].live.awaiting_approval.is_empty() {
                plan.push(RailRow::AgentDetail(id));
            }
        }
        if chosen.len() < agents.len() {
            plan.push(RailRow::More(agents.len() - chosen.len()));
        }
    }
    (plan, focused_row)
}

/// The first planned row the rail draws at `height`: the window is centered
/// on the focused row. Shared with click hit-testing so a click lands on the
/// row it was drawn on.
pub(super) fn rail_window_start(rows: usize, focused_row: usize, height: usize) -> usize {
    focused_row
        .saturating_sub(height / 2)
        .min(rows.saturating_sub(height))
}

/// Right-hand session list grouped by what the user should do about each
/// session: NEEDS YOU, WORKING, RECENT. Each session takes one row of state
/// glyph, name, agent count, and unread badge; at
/// [`RailDensity::Detailed`] a second muted row carries the live tail and
/// the session's cost. Agents appear only under the focused session and
/// when they wait on the user. The focused row sits on the selection
/// background. Always `height` rows so it zips against the body.
pub(super) fn sidebar(app: &App, width: usize, height: usize, density: RailDensity) -> Vec<Line> {
    let inner = width.saturating_sub(2);
    let (plan, focused_row) = plan_rail(app, density);
    let mut rows: Vec<Line> = Vec::with_capacity(height);
    if plan.is_empty() {
        rows.push(Line::styled("│   no sessions yet", muted().italic()));
    }
    let start = rail_window_start(plan.len(), focused_row, height);
    let select = |line: &mut Line| {
        pad_line(line, width);
        for span in &mut line.spans[1..] {
            span.style = selection(span.style);
        }
    };
    for row in plan.into_iter().skip(start).take(height) {
        rows.push(match row {
            RailRow::Gap => Line::styled("│", border()),
            RailRow::Header(group, count) => {
                let mut header = Line::styled("│ ", border());
                header.push(
                    group.label(),
                    match group {
                        RailGroup::NeedsYou => warning().bold(),
                        RailGroup::Working => info().bold(),
                        RailGroup::Recent => muted().bold(),
                    },
                );
                header.push(format!("  {count}"), muted());
                truncate_line(header, width)
            }
            RailRow::Session(entry) => {
                // Badges are right-aligned and claim their cells first so a
                // long title truncates instead of pushing them off the rail:
                // `↳N` agents (plus the working count while any run), then
                // `N new` when the rail has room for it, else `N`.
                let mut right = Line::default();
                if entry.agents > 0 {
                    right.push(format!("↳{}", entry.agents), muted());
                    if entry.agents_working > 0 {
                        right.push(
                            format!(" {}{}", spinner(app.animation_tick), entry.agents_working),
                            info(),
                        );
                    }
                }
                if entry.unread > 0 {
                    if !right.is_empty() {
                        right.push(" ", muted());
                    }
                    right.push(
                        if width >= layout::RAIL_MIN_WIDTH + 6 {
                            format!("{} new", entry.unread)
                        } else {
                            entry.unread.to_string()
                        },
                        accent(),
                    );
                }
                let right_width = right.width();
                let title_width =
                    width.saturating_sub(if right_width == 0 { 0 } else { right_width + 3 });
                let mut line = session_line(app, entry.id, title_width, "│ ");
                if right_width > 0 {
                    pad_line(&mut line, width.saturating_sub(right_width + 1));
                    line.spans.extend(right.spans);
                }
                if app.focused() == Some(entry.id) {
                    select(&mut line);
                }
                truncate_line(line, width)
            }
            RailRow::Detail(entry) => {
                let mut line = Line::styled("│    ", muted());
                let used = line.width();
                let (text, style) =
                    live_status_line(app, entry.id).unwrap_or_else(|| (String::new(), muted()));
                match density {
                    RailDensity::Compact => {
                        line.push(preview(&text, inner.saturating_sub(used)), style);
                    }
                    RailDensity::Detailed => {
                        // Tail on the left, cost on the right. The cost
                        // includes the session's agents and stays muted so
                        // the accent is only ever the badge.
                        let cost = entry.cost.map(format_cost);
                        let cost_width = cost.as_ref().map_or(0, |cost| cost.chars().count() + 3);
                        let tail_width = inner.saturating_sub(used).saturating_sub(cost_width);
                        line.push(preview(&text, tail_width), style);
                        if let Some(cost) = cost {
                            pad_line(&mut line, width.saturating_sub(cost.chars().count() + 1));
                            line.push(cost, muted());
                        }
                    }
                }
                truncate_line(line, width)
            }
            RailRow::Agent(id) => {
                let depth = app.sessions.depth(id).clamp(1, 4);
                let prefix = format!("│ {}↳ ", "  ".repeat(depth - 1));
                let mut line = session_line(app, id, width, &prefix);
                if app.focused() == Some(id) {
                    select(&mut line);
                }
                truncate_line(line, width)
            }
            RailRow::AgentDetail(id) => {
                let depth = app.sessions.depth(id).clamp(1, 4);
                let mut line = Line::styled(format!("│ {}    ", "  ".repeat(depth - 1)), muted());
                let used = line.width();
                let (text, style) =
                    live_status_line(app, id).unwrap_or_else(|| (String::new(), muted()));
                line.push(preview(&text, inner.saturating_sub(used)), style);
                truncate_line(line, width)
            }
            RailRow::More(count) => {
                let mut line = Line::styled(format!("│   +{count} more"), muted());
                line.push("  /agents", muted().italic());
                truncate_line(line, width)
            }
            RailRow::MoreWaiting(count) => {
                let mut line = Line::styled(format!("│   +{count} waiting"), warning());
                if let Some(chord) = app.chord_label(crate::commands::Command::FocusNextApproval) {
                    line.push(format!("  {chord}"), muted());
                }
                truncate_line(line, width)
            }
        });
    }
    while rows.len() < height {
        rows.push(Line::styled("│", border()));
    }
    rows.truncate(height);
    rows
}

/// One row above the composer when the rail is hidden and more than one
/// session exists: how many sessions there are and how many need the user,
/// are working, or have unread output, with the chord that jumps to them.
/// Built from the same [`rollup`] pass as the rail, so agents count toward
/// their session rather than as sessions of their own.
pub(super) fn agent_strip(app: &App, width: usize) -> Option<Line> {
    let rollup = rollup(app);
    if rollup.sessions.len() < 2 {
        return None;
    }
    let needs = rollup.counts[RailGroup::NeedsYou as usize];
    let working = rollup.counts[RailGroup::Working as usize];
    let unread = rollup
        .sessions
        .iter()
        .filter(|entry| entry.unread > 0)
        .count();
    let mut line = Line::styled(format!(" {} sessions", rollup.sessions.len()), muted());
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
        line.push(format!("● {unread} new"), accent());
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
        let activity = session.activity.map(|(_, activity)| activity);
        // A compacting run has usually streamed text already; that tail is
        // stale until the summary lands, so the activity wins over it.
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
