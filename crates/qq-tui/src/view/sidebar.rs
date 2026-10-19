use super::*;
use layout::Tier;
use qq_client::state::{Group, Need};

pub(super) fn session_line(app: &App, session_id: SessionId, width: usize, prefix: &str) -> Line {
    let view = &app.sessions[&session_id];
    let session = &view.summary;
    // The same state vocabulary tool rows use: ● done, ✕ failed, ◌ stopped
    // early, ◇ waiting, the shared spinner while running. Color follows the
    // attention rule: `warning` for an approval the human is asked for,
    // `error` for a failure, `accent` for a finish the user has not looked
    // at, muted for everything settled. A hold an approval delegate still
    // owns is work, so it reads as running.
    let (marker, style) = match agent_need(view) {
        Some(Need::Approval) => ("◇", warning()),
        Some(Need::Failed) => ("✕", failure()),
        Some(Need::FinishedUnread) => ("●", accent()),
        None if !view.live.awaiting_approval.is_empty()
            || session.running_side_question.is_some() =>
        {
            (spinner(app.animation_tick), info())
        }
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
    /// The slice of [`Rollup::needs`] holding this session's agents that
    /// need the user, most urgent first.
    pub needs_start: u32,
    pub needs_end: u32,
}

/// Every session grouped NEEDS YOU, WORKING, RECENT (tree order, newest
/// session first, within a group), how many each group holds, the agents
/// that need the user, and the focused session's agents. One O(sessions)
/// walk; audit agents, their subtrees, and audit roots are skipped.
#[derive(Debug, Default)]
pub(super) struct Rollup {
    pub sessions: Vec<RailEntry>,
    pub counts: [usize; 3],
    /// `(session, agent, need)` for every agent that needs the user, sorted
    /// by session tree order then by need (approval, failure, unseen
    /// finish) then tree order.
    pub needs: Vec<(SessionId, SessionId, Need)>,
    /// The session whose tree holds the focused session, and its non-audit
    /// agents in tree order.
    pub focused_tree: Option<(SessionId, Vec<SessionId>)>,
}

impl Rollup {
    /// Agents under `entry`'s session that need the user, most urgent first.
    /// A slice per session, so planning stays linear in sessions.
    pub(super) fn needs_under(&self, entry: &RailEntry) -> impl Iterator<Item = SessionId> + '_ {
        self.needs[entry.needs_start as usize..entry.needs_end as usize]
            .iter()
            .map(|(_, agent, _)| *agent)
    }
}

/// Whether a session exists for bookkeeping rather than for the user: audit
/// agents check a parent's final answer and are never listed.
pub(super) fn is_audit(session: &SessionView) -> bool {
    session.summary.purpose == qq_protocol::SessionPurpose::Audit
}

/// Whether the human is asked to answer an approval in `session`:
/// `awaiting_approval` minus the holds an approval delegate still owns.
pub(super) fn human_waiting(session: &SessionView) -> bool {
    // `with_delegate` is a subset of `awaiting_approval`.
    session.live.awaiting_approval.len() > session.live.with_delegate.len()
}

/// Whether `session` is doing work: running or queued (unless it is blocked
/// on the human's approval), answering a side question, or waiting on an
/// approval delegate.
pub(super) fn is_working(session: &SessionView) -> bool {
    if human_waiting(session) {
        return false;
    }
    matches!(
        session.summary.status,
        SessionStatus::Running | SessionStatus::Queued
    ) || session.summary.running_side_question.is_some()
        || !session.live.with_delegate.is_empty()
}

/// Why an agent needs the user, if it does. Like [`SessionView::need`],
/// except that an approval counts only while the human is asked: a hold an
/// approval delegate still owns (`with_delegate`) is the agent working.
pub(super) fn agent_need(session: &SessionView) -> Option<Need> {
    if human_waiting(session) {
        return Some(Need::Approval);
    }
    // `need()` reports any waiting hold first; with every hold owned by a
    // delegate, an unseen outcome from an earlier run still needs the user.
    if !session.finished_unread {
        return None;
    }
    Some(match session.summary.last_outcome {
        Some(qq_protocol::RunOutcome::Failed { .. }) => Need::Failed,
        _ => Need::FinishedUnread,
    })
}

/// Agents whose session is not loaded (a bounded snapshot can carry a child
/// without its parent): absent from `thread_order`, so listed as sessions of
/// their own until the parent arrives. Cheap when there are none.
pub(super) fn orphans(app: &App) -> Vec<SessionId> {
    if app.sessions.thread_order().len() == app.sessions.len() {
        return Vec::new();
    }
    let mut orphans: Vec<&SessionView> = app
        .sessions
        .values()
        .filter(|session| {
            session
                .summary
                .parent_id
                .is_some_and(|parent| !app.sessions.contains_key(&parent))
                && !is_audit(session)
        })
        .collect();
    orphans.sort_by(|a, b| {
        (b.summary.updated_at_ms, b.summary.id).cmp(&(a.summary.updated_at_ms, a.summary.id))
    });
    orphans
        .into_iter()
        .map(|session| session.summary.id)
        .collect()
}

/// A session's own group by the same human-aware rules the roll-up applies
/// to its agents: an approval counts only while the human is asked, and a
/// delegate deciding or a side question is work.
fn own_group(session: &SessionView) -> Group {
    match agent_need(session) {
        Some(_) => Group::NeedsYou,
        None if is_working(session) => Group::Working,
        None if session.summary.last_outcome.is_some() => Group::Done,
        None => Group::Idle,
    }
}

/// The session being accumulated by [`rollup`]: its entry, its own group,
/// whether any agent needs the user, where its needs start, and (only for
/// the focused tree) its agents.
struct Pending {
    entry: RailEntry,
    own: Group,
    needs_you: bool,
    agents: Option<Vec<SessionId>>,
    /// The session's summary predates inclusive accounting: its cost is its
    /// own direct alias plus each loaded agent's (audits included).
    legacy_cost: bool,
}

/// A summary's own spend in nanos: inclusive when it has accounting, the
/// direct compatibility alias otherwise.
fn own_cost(session: &SessionView) -> Option<u64> {
    session
        .summary
        .accounting
        .map(|accounting| accounting.inclusive.estimated_cost_usd_nanos)
        .unwrap_or(session.summary.estimated_cost_usd_nanos)
}

/// Add a loaded agent's direct spend to a legacy session's sum.
fn add_legacy_cost(pending: &mut Pending, agent: &SessionView) {
    if !pending.legacy_cost {
        return;
    }
    let direct = agent
        .summary
        .accounting
        .map(|accounting| accounting.direct.estimated_cost_usd_nanos)
        .unwrap_or(agent.summary.estimated_cost_usd_nanos);
    if let Some(direct) = direct.filter(|nanos| *nanos > 0) {
        pending.entry.cost = Some(pending.entry.cost.unwrap_or(0).saturating_add(direct));
    }
}

pub(super) fn rollup(app: &App) -> Rollup {
    let focused = app.focused();
    // The topmost loaded ancestor: a root, or an orphan whose parent is not
    // loaded. Bounded by the tree depth.
    let focused_root = focused.map(|mut id| {
        for _ in 0..=64 {
            match app
                .sessions
                .get(&id)
                .and_then(|session| session.summary.parent_id)
            {
                Some(parent) if app.sessions.contains_key(&parent) => id = parent,
                _ => break,
            }
        }
        id
    });
    let order = app.sessions.thread_order();
    let mut buckets: [Vec<RailEntry>; 3] = Default::default();
    let mut rollup = Rollup::default();
    let mut current: Option<Pending> = None;
    let mut audit_depth: Option<usize> = None;
    let mut finish = |pending: Pending, rollup: &mut Rollup| {
        let Pending {
            mut entry,
            own,
            needs_you,
            agents,
            ..
        } = pending;
        // Most urgent first; tree order within a rank (stable sort).
        let start = entry.needs_start as usize;
        rollup.needs[start..].sort_by_key(|(_, _, need)| *need);
        entry.needs_end = u32::try_from(rollup.needs.len()).unwrap_or(u32::MAX);
        let group = if own == Group::NeedsYou || needs_you {
            RailGroup::NeedsYou
        } else if own == Group::Working || entry.agents_working > 0 {
            RailGroup::Working
        } else {
            RailGroup::Recent
        };
        if let Some(agents) = agents {
            rollup.focused_tree = Some((entry.id, agents));
        }
        buckets[group as usize].push(RailEntry { group, ..entry });
    };
    let session_entry = |id: SessionId, session: &SessionView, needs_start: usize| {
        let is_focused = focused == Some(id);
        // Must agree with `live_status_line`; the planner relies on it.
        let live = !session.live.awaiting_approval.is_empty()
            || session.summary.status == SessionStatus::Running
            || session.summary.queued_prompts > 0
            || (!is_focused && !session.live.tail.is_empty());
        let cost = own_cost(session).filter(|nanos| *nanos > 0);
        let needs_start = u32::try_from(needs_start).unwrap_or(u32::MAX);
        Pending {
            entry: RailEntry {
                id,
                group: RailGroup::Recent,
                unread: if is_focused { 0 } else { session.unread },
                live,
                cost,
                agents: 0,
                agents_working: 0,
                needs_start,
                needs_end: needs_start,
            },
            own: own_group(session),
            needs_you: false,
            agents: (focused_root == Some(id)).then(Vec::new),
            legacy_cost: session.summary.accounting.is_none(),
        }
    };
    for &id in order {
        let session = &app.sessions[&id];
        let root = session.summary.parent_id.is_none();
        // Everything below an audit agent is bookkeeping too, but its spend
        // still belongs to a legacy session's total.
        if let Some(depth) = audit_depth {
            if !root && app.sessions.depth(id) > depth {
                if let Some(pending) = current.as_mut() {
                    add_legacy_cost(pending, session);
                }
                continue;
            }
            audit_depth = None;
        }
        if is_audit(session) {
            // An audit root (its parent was deleted) closes the session
            // before it like any root, then is skipped with its subtree. An
            // audit agent's spend still counts toward a legacy session.
            if root && let Some(pending) = current.take() {
                finish(pending, &mut rollup);
            } else if let Some(pending) = current.as_mut() {
                add_legacy_cost(pending, session);
            }
            audit_depth = Some(app.sessions.depth(id));
            continue;
        }
        if root {
            if let Some(pending) = current.take() {
                finish(pending, &mut rollup);
            }
            current = Some(session_entry(id, session, rollup.needs.len()));
            continue;
        }
        // An agent whose session is not loaded has nothing to roll into.
        let Some(pending) = current.as_mut() else {
            continue;
        };
        add_legacy_cost(pending, session);
        pending.entry.agents += 1;
        if is_working(session) {
            pending.entry.agents_working += 1;
        }
        // A focused agent still lifts its session while its approval waits;
        // focusing clears an unseen finish, never an approval.
        if let Some(need) = agent_need(session) {
            pending.needs_you = true;
            rollup.needs.push((pending.entry.id, id, need));
        }
        if let Some(agents) = pending.agents.as_mut() {
            agents.push(id);
        }
    }
    if let Some(pending) = current.take() {
        finish(pending, &mut rollup);
    }
    for id in orphans(app) {
        // A temporary session: its loaded descendants roll into it the
        // same way a root's do, depth-first in spawn order.
        let mut pending = session_entry(id, &app.sessions[&id], rollup.needs.len());
        // `(agent, under an audit)`: an audit's subtree is never listed but
        // its spend still counts toward a legacy session's total.
        let mut stack: Vec<(SessionId, bool)> = app
            .sessions
            .children_of(id)
            .iter()
            .rev()
            .map(|child| (*child, false))
            .collect();
        while let Some((agent, hidden)) = stack.pop() {
            let Some(session) = app.sessions.get(&agent) else {
                continue;
            };
            add_legacy_cost(&mut pending, session);
            let hidden = hidden || is_audit(session);
            stack.extend(
                app.sessions
                    .children_of(agent)
                    .iter()
                    .rev()
                    .map(|child| (*child, hidden)),
            );
            if hidden {
                continue;
            }
            pending.entry.agents += 1;
            if is_working(session) {
                pending.entry.agents_working += 1;
            }
            if let Some(need) = agent_need(session) {
                pending.needs_you = true;
                rollup.needs.push((id, agent, need));
            }
            if let Some(agents) = pending.agents.as_mut() {
                agents.push(agent);
            }
        }
        finish(pending, &mut rollup);
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
        let agent_rows = |plan: &mut Vec<RailRow>, id: SessionId| {
            plan.push(RailRow::Agent(id));
            if human_waiting(&app.sessions[&id]) {
                plan.push(RailRow::AgentDetail(id));
            }
        };
        let Some((_, agents)) = tree else {
            let mut needs = rollup.needs_under(&entry);
            for agent in needs.by_ref().take(WAITING_AGENT_ROWS) {
                agent_rows(&mut plan, agent);
            }
            let rest = needs.count();
            if rest > 0 {
                plan.push(RailRow::MoreWaiting(rest));
            }
            continue;
        };
        // The focused session's agents, at most `FOCUSED_AGENT_ROWS`, painted
        // in rank order: the focused agent, then agents that need the user
        // (most urgent first), then quiet agents in tree order.
        let mut ranked: Vec<SessionId> = Vec::with_capacity(agents.len());
        ranked.extend(focused.filter(|id| agents.contains(id)));
        ranked.extend(rollup.needs_under(&entry).filter(|id| Some(*id) != focused));
        ranked.extend(
            agents
                .iter()
                .copied()
                .filter(|id| Some(*id) != focused && agent_need(&app.sessions[id]).is_none()),
        );
        for &id in ranked.iter().take(FOCUSED_AGENT_ROWS) {
            if focused == Some(id) {
                focused_row = plan.len();
            }
            agent_rows(&mut plan, id);
        }
        if ranked.len() > FOCUSED_AGENT_ROWS {
            plan.push(RailRow::More(ranked.len() - FOCUSED_AGENT_ROWS));
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
/// background. Always `height` rows so it zips against the body. Also
/// returns the sessions and agents whose rows the window painted.
pub(super) fn sidebar(
    app: &App,
    width: usize,
    height: usize,
    density: RailDensity,
) -> (Vec<Line>, Vec<SessionId>) {
    let inner = width.saturating_sub(2);
    let (plan, focused_row) = plan_rail(app, density);
    let mut rows: Vec<Line> = Vec::with_capacity(height);
    let mut painted: Vec<SessionId> = Vec::new();
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
        match row {
            RailRow::Session(RailEntry { id, .. })
            | RailRow::Detail(RailEntry { id, .. })
            | RailRow::Agent(id)
            | RailRow::AgentDetail(id) => painted.push(id),
            RailRow::Gap | RailRow::Header(..) | RailRow::More(_) | RailRow::MoreWaiting(_) => {}
        }
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
                let mut line = Line::styled(format!("│   +{count} need you"), warning());
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
    (rows, painted)
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
    if human_waiting(session) {
        let tool = live.active_tool.as_deref().map_or("tool", tool_verb);
        return Some((format!("◇ approve {tool}"), warning().bold()));
    }
    if !live.with_delegate.is_empty() {
        return Some(("delegate deciding".to_owned(), muted().italic()));
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
