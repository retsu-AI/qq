use super::*;

#[cfg(test)]
pub(super) const VERSION: &str = env!("CARGO_PKG_VERSION");

pub(super) fn spinner(tick: usize) -> &'static str {
    TOOL_SPINNER[tick % TOOL_SPINNER.len()]
}

pub(super) fn status_row(app: &App, width: usize, compact: bool) -> Line {
    let focused = app
        .focused()
        .and_then(|id| app.sessions.get(&id))
        .map(|session| &session.summary);
    let mut left = Line::styled("  ", normal());
    let title = match app.focused() {
        Some(id) => {
            let mut chain = Vec::new();
            let mut cursor = Some(id);
            while let Some(id) = cursor {
                let Some(session) = app.sessions.get(&id) else {
                    break;
                };
                chain.push(session.summary.title.as_str());
                cursor = session.summary.parent_id;
            }
            match chain.as_slice() {
                [] => "new session".to_owned(),
                [own] => (*own).to_owned(),
                [own, .., root] => format!("{root} › {own}"),
            }
        }
        None => "new session".to_owned(),
    };
    left.push(title, normal());
    let mut parts: Vec<(String, Style)> = Vec::new();
    for item in app.settings.status_line() {
        let part: Option<(String, Style)> = match item {
            StatusItem::Model => Some(
                focused
                    .and_then(|session| session.model.as_deref())
                    .or(app.model.model.as_deref())
                    .map_or_else(
                        || ("no model".to_owned(), warning()),
                        |model| {
                            let short = model.split_once('/').map_or(model, |(_, name)| name);
                            let effort = focused
                                .map_or(app.reasoning_effort, |session| session.reasoning_effort);
                            (
                                match effort {
                                    Some(effort) => {
                                        format!("{short} {}", effort_label(Some(effort)))
                                    }
                                    None => short.to_owned(),
                                },
                                muted(),
                            )
                        },
                    ),
            ),
            StatusItem::Profile => {
                let profile = focused.map_or(&app.profile, |session| &session.profile);
                (!profile.is_default()).then(|| (format!("as {}", profile.as_str()), muted()))
            }
            StatusItem::ApprovalMode => {
                let mode = focused.map_or(app.approval_mode, |session| session.approval_mode);
                (mode != ApprovalMode::Auto).then(|| {
                    let style = if mode == ApprovalMode::Full {
                        warning()
                    } else {
                        muted()
                    };
                    (mode_name(mode).to_owned(), style)
                })
            }
            StatusItem::Effort => None,
            StatusItem::Context if compact => None,
            StatusItem::Context => match app.focused_context_usage() {
                Some((tokens, limit)) if limit > 0 => {
                    let percent = u128::from(tokens) * 100 / u128::from(limit);
                    Some((format!("{percent}%"), muted()))
                }
                _ => None,
            },
            StatusItem::Cost if compact => None,
            StatusItem::Cost => focused
                .and_then(|session| {
                    session
                        .accounting
                        .map(|accounting| accounting.inclusive.estimated_cost_usd_nanos)
                        .unwrap_or(session.estimated_cost_usd_nanos)
                })
                .filter(|nanos| *nanos > 0)
                .map(|nanos| (format_cost(nanos), muted())),
            StatusItem::Workspace => None,
            StatusItem::Tools => Some((format!("tools {}", app.tool_detail.label()), muted())),
        };
        parts.extend(part);
    }
    for (text, style) in parts {
        left.push(" · ", faint());
        left.push(text, style);
    }
    let right = match app.connection {
        crate::ConnectionState::Connecting => Some(("connecting", warning())),
        crate::ConnectionState::Replaying => Some(("reconnecting", warning())),
        crate::ConnectionState::Offline => Some(("offline", warning())),
        crate::ConnectionState::Live => (!compact && !app.workspace_path.is_empty())
            .then(|| (app.workspace_path.as_str(), faint())),
    };
    match right {
        Some((text, style)) if left.width() + text_width(text) + 2 <= width => {
            align_sides(left, Line::styled(text, style), width)
        }
        Some((text, style)) if app.connection != crate::ConnectionState::Live => {
            let right = Line::styled(text, style);
            let room = width.saturating_sub(right.width() + 2);
            align_sides(truncate_line(left, room), right, width)
        }
        _ => truncate_line(left, width),
    }
}

pub(super) const GOAL_CHECK_RULE_COMMAND: usize = 40;

pub(crate) const fn mode_name(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::ReadOnly => "read-only",
        ApprovalMode::Ask => "ask",
        ApprovalMode::Auto => "auto",
        ApprovalMode::Full => "full",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuleState {
    Idle,
    Running,
    NeedsYou,
    Failed,
}

impl RuleState {
    pub(crate) fn style(self) -> Style {
        match self {
            Self::Idle => faint(),
            Self::Running => info(),
            Self::NeedsYou => warning(),
            Self::Failed => failure(),
        }
    }
}

pub(super) fn rule_state(app: &App) -> RuleState {
    let Some(session) = app.focused().and_then(|id| app.sessions.get(&id)) else {
        return RuleState::Idle;
    };
    if app.pending_approval().is_some()
        || !session.live.awaiting_approval.is_empty()
        || matches!(app.overlay, Some(crate::input::Overlay::GoalCheck { .. }))
    {
        return RuleState::NeedsYou;
    }
    if matches!(
        session.goal.as_deref().map(|goal| &goal.status),
        Some(qq_protocol::GoalStatus::Paused {
            reason: qq_protocol::GoalPauseReason::NeedsUser { .. }
        })
    ) {
        return RuleState::NeedsYou;
    }
    match session.summary.status {
        SessionStatus::Running | SessionStatus::Queued => RuleState::Running,
        SessionStatus::Idle => match session.summary.last_outcome {
            Some(qq_protocol::RunOutcome::Failed { .. }) => RuleState::Failed,
            _ => RuleState::Idle,
        },
    }
}

pub(super) fn rule_activity(app: &App) -> Option<String> {
    let session = app.focused().and_then(|id| app.sessions.get(&id))?;
    if app.pending_question().is_some() {
        return Some("answer the question".to_owned());
    }
    if let Some(call) = app.pending_approval() {
        let verb = match call.name.as_str() {
            "edit_file" | "write_file" => "apply this edit?",
            "shell" | "exec" => "run this command?",
            "web_fetch" => "fetch this URL?",
            _ => "allow this call?",
        };
        return Some(verb.to_owned());
    }
    if let Some(crate::input::Overlay::GoalCheck { target, .. }) = &app.overlay {
        let command: String = target
            .command
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .filter_map(terminal_safe_character)
            .collect();
        return Some(format!(
            "run {} in {}?",
            preview(&command, GOAL_CHECK_RULE_COMMAND),
            target.workspace_path
        ));
    }
    if let Some(goal) = session.goal.as_deref() {
        match &goal.status {
            qq_protocol::GoalStatus::Paused {
                reason:
                    qq_protocol::GoalPauseReason::NeedsUser {
                        cause: qq_protocol::GoalNeedsUser::CheckApproval,
                    },
            } => return Some("goal needs check consent".to_owned()),
            qq_protocol::GoalStatus::Active if session.summary.status != SessionStatus::Running => {
                return Some(format!(
                    "goal run {}/{} · next {}",
                    goal.counters.runs,
                    goal.budget.max_goal_runs,
                    goal_duration(goal.next_run_at_ms.saturating_sub(app.goal_now_ms))
                ));
            }
            qq_protocol::GoalStatus::Active => {}
            status => {
                let (label, _) = super::workspace::goal_status(status);
                let safe: String = label.chars().filter_map(terminal_safe_character).collect();
                if session.summary.status != SessionStatus::Running {
                    if goal.pending_run_id.is_some() {
                        return Some(format!("goal {safe} · cancellation draining"));
                    }
                    return Some(format!("goal {safe}"));
                }
            }
        }
    }
    match session.summary.status {
        SessionStatus::Queued => Some("queued".to_owned()),
        SessionStatus::Idle => None,
        SessionStatus::Running => {
            let reasoning = session
                .summary
                .active_run_id
                .and_then(|run| session.reasoning.get(&run))
                .filter(|reasoning| reasoning.streaming);
            let mut activity = if let Some(reasoning) = reasoning {
                format!("thinking · {}s", reasoning.ticks / 8)
            } else {
                match (
                    session.live.active_tool.as_deref(),
                    session.activity.map(|(_, activity)| activity),
                ) {
                    (Some(tool), _) => present_verb(tool).to_owned(),
                    (None, Some(qq_protocol::RunActivity::WaitingForProvider)) => {
                        "waiting for model".to_owned()
                    }
                    (None, Some(qq_protocol::RunActivity::Reasoning)) => "thinking".to_owned(),
                    (None, Some(qq_protocol::RunActivity::GeneratingResponse)) => {
                        "writing".to_owned()
                    }
                    (None, Some(qq_protocol::RunActivity::PreparingToolCall)) => {
                        "preparing a tool call".to_owned()
                    }
                    (None, Some(qq_protocol::RunActivity::Compacting)) => {
                        "compacting context".to_owned()
                    }
                    (None, None) => "working".to_owned(),
                }
            };
            if reasoning.is_none()
                && let Some(started) = session
                    .summary
                    .active_run_id
                    .and_then(|run_id| session.runs.get(&run_id))
                    .and_then(|stats| stats.started_at_ms)
            {
                let elapsed = app.now_ms.saturating_sub(started);
                if elapsed >= 1000 {
                    activity.push_str(&format!(" · {}", format_duration_ms(elapsed)));
                }
            }
            Some(activity)
        }
    }
}

fn present_verb(tool: &str) -> &str {
    match tool {
        "read_file" => "reading",
        "tree" | "list_dir" => "listing",
        "search" => "searching",
        "read_tool_result" => "recalling",
        "edit_file" => "editing",
        "write_file" => "writing a file",
        "shell" | "exec" => "running",
        "spawn_agent" => "spawning",
        "web_fetch" => "fetching",
        other => other,
    }
}

pub(super) fn rule_attention(app: &App, rail_shown: bool) -> Option<(String, Style)> {
    let waiting = app
        .sessions_needing_attention()
        .into_iter()
        .filter(|id| Some(*id) != app.view().transcript())
        .filter(|id| {
            app.sessions
                .get(id)
                .is_some_and(|session| session.need() == Some(Need::Approval))
        })
        .count();
    let jump = app
        .chord_label(crate::commands::Command::FocusNextApproval)
        .map(|chord| compact_chord(&chord));
    if !rail_shown && app.sessions.len() > 1 {
        let (entries, counts) = rail_entries(app);
        let mut text = format!("{} sessions", entries.len());
        let working = counts[Group::Working as usize];
        let needs = counts[Group::NeedsYou as usize];
        if working > 0 {
            text.push_str(&format!("  ◐ {working}"));
        }
        if needs > 0 {
            text.push_str(&format!("  ◇ {needs}"));
            if let Some(jump) = &jump {
                text.push_str(&format!("  {jump}"));
            }
        }
        let style = if needs > 0 { warning() } else { faint() };
        return Some((text, style));
    }
    if waiting == 0 {
        return None;
    }
    let mut text = format!(
        "◇ {waiting} {}",
        if waiting == 1 {
            "needs you"
        } else {
            "need you"
        }
    );
    if let Some(jump) = jump {
        text.push_str(&format!("  {jump}"));
    }
    Some((text, warning()))
}

pub(super) fn dock_rule(app: &App, width: usize, rail_shown: bool) -> Line {
    let state = rule_state(app);
    let style = state.style();
    let mut left = Line::default();
    if let Some((status, level)) = app.visible_status() {
        let (prefix, style) = match level {
            crate::app::NoticeLevel::Info => ("", accent()),
            crate::app::NoticeLevel::Warning => ("warning: ", warning()),
            crate::app::NoticeLevel::Error => ("✕ ", failure()),
        };
        left.push(format!("{prefix}{status} "), style);
    } else if app.focused().is_none()
        && let Some(guidance) = app.startup_guidance()
    {
        left.push(format!("{guidance} "), accent());
    } else if let Some(activity) = rule_activity(app) {
        let glyph = match state {
            RuleState::Idle => "○",
            RuleState::Running => spinner(app.animation_tick),
            RuleState::NeedsYou => "◇",
            RuleState::Failed => "✕",
        };
        left.push(format!("{glyph} "), style);
        left.push(format!("{activity} "), normal());
    } else if state == RuleState::Failed {
        left.push("✕ ", style);
        left.push("run failed ", normal());
    }
    let right = rule_attention(app, rail_shown)
        .map(|(text, style)| Line::styled(format!(" {text}"), style))
        .unwrap_or_default();
    let right = if left.width() + right.width() + 4 > width {
        Line::default()
    } else {
        right
    };
    let mut rule = truncate_line(left, width.saturating_sub(right.width() + 1));
    let fill = width.saturating_sub(rule.width() + right.width());
    rule.push("─".repeat(fill), style);
    for span in right.spans {
        rule.push(span.text, span.style);
    }
    truncate_line(rule, width)
}

pub(super) fn key_row(keys: &[(&str, &str)]) -> Line {
    let mut line = Line::default();
    for (index, (key, label)) in keys.iter().enumerate() {
        if index > 0 {
            line.push("   ", normal());
        }
        line.push(*key, normal());
        if !label.is_empty() {
            line.push(format!(" {label}"), faint());
        }
    }
    line
}

pub(super) fn answering_question(app: &App) -> bool {
    app.pending_question()
        .and_then(|question| question.questions.get(app.question_answers.len()))
        .is_some_and(|question| question.free_text)
}

pub(super) fn dock_footer(app: &App, width: usize, compact: bool) -> Line {
    match app.mode() {
        Mode::Sessions => {
            let mut row = Line::styled("  ", normal());
            for span in key_row(&[
                ("enter", "open"),
                ("tab", "agents"),
                ("^A", "all agents"),
                ("^D", "delete"),
                ("esc", "back"),
            ])
            .spans
            {
                row.push(span.text, span.style);
            }
            truncate_line(row, width)
        }
        Mode::Models
        | Mode::Profiles
        | Mode::ApprovalModes
        | Mode::Effort
        | Mode::Skills
        | Mode::Themes
        | Mode::Commands
        | Mode::History => {
            let mut row = Line::styled("  ", normal());
            for span in key_row(&[("enter", "choose"), ("↑↓", "move"), ("esc", "back")]).spans {
                row.push(span.text, span.style);
            }
            truncate_line(row, width)
        }
        Mode::Compose | Mode::Approval | Mode::GoalForm | Mode::GoalCheck => {
            if app.mode() == Mode::Compose
                && matches!(app.view(), View::Goal(_))
                && app.composer.text.is_empty()
            {
                let mut row = Line::styled("  ", normal());
                for span in key_row(&[
                    ("/goal check", "consent"),
                    ("/goal pause", ""),
                    ("esc", "back"),
                ])
                .spans
                {
                    row.push(span.text, span.style);
                }
                return truncate_line(row, width);
            }
            status_row(app, width, compact)
        }
    }
}

pub(super) fn dock_panel(app: &App, width: usize, limit: usize) -> Vec<Line> {
    let rows = match app.mode() {
        Mode::GoalForm => goal_form(app, width, limit),
        Mode::GoalCheck => goal_check_picker(app, width, limit),
        Mode::Models => model_picker(app, width, limit),
        Mode::Profiles => profile_picker(app, width, limit),
        Mode::ApprovalModes => approval_mode_picker(app, width, limit),
        Mode::Effort => effort_picker(app, width, limit),
        Mode::Skills => skill_picker(app, width, limit),
        Mode::Themes => theme_picker(app, width, limit),
        Mode::Sessions => session_picker(app, width, limit),
        Mode::Commands => command_picker(app, width, limit),
        Mode::History => history_picker(app, width, limit),
        Mode::Approval => approval_block(app, width, limit),
        Mode::Compose => match app.view() {
            View::Goal(session_id) => {
                let rows = goal_panel(app, session_id, width);
                let start = app.goal_scroll.min(rows.len().saturating_sub(limit));
                let hidden_below = rows.len().saturating_sub(start + limit);
                let mut shown: Vec<Line> = rows.into_iter().skip(start).collect();
                if hidden_below > 0 {
                    shown.truncate(limit.saturating_sub(1));
                    shown.push(Line::styled(
                        format!(
                            "  … {} · PgDn",
                            count_noun(hidden_below + 1, "more row", "more rows")
                        ),
                        faint(),
                    ));
                }
                shown
            }
            _ => Vec::new(),
        },
    };
    let mut rows: Vec<Line> = rows
        .into_iter()
        .map(|line| truncate_line(line, width))
        .collect();
    while rows.last().is_some_and(Line::is_empty) {
        rows.pop();
    }
    rows.truncate(limit);
    rows
}

pub(super) fn goal_panel(app: &App, session_id: SessionId, width: usize) -> Vec<Line> {
    use qq_protocol::{GoalCheckAuthorization, GoalChecklistState};
    let hold = app.check_interruption.map(|hold| {
        let mut line = Line::styled("  ◇ ", warning());
        line.push(
            format!(
                "Workspace held by interrupted check {}; resolve its effects and acknowledge it",
                hold.run_id
            ),
            normal(),
        );
        truncate_line(line, width)
    });
    let Some(goal) = app
        .sessions
        .get(&session_id)
        .and_then(|view| view.goal.as_deref())
    else {
        let mut rows = vec![truncate_line(
            Line::styled(
                "  No goal for this session. /goal <objective> opens the form.",
                faint(),
            ),
            width,
        )];
        rows.extend(hold);
        return rows;
    };
    let safe =
        |text: &str| -> String { text.chars().filter_map(terminal_safe_character).collect() };
    let field = |label: &str, value: String| {
        let mut line = Line::styled(format!("  {label:<10}"), faint());
        line.push(value, normal());
        truncate_line(line, width)
    };
    let mut rows = vec![truncate_line(
        Line::styled(format!("  {}", safe(&goal.objective)), normal().bold()),
        width,
    )];
    rows.push(field(
        "status",
        safe(&super::workspace::goal_status(&goal.status).0),
    ));
    rows.push(field(
        "runs",
        format!("{} / {}", goal.counters.runs, goal.budget.max_goal_runs),
    ));
    let tokens = goal.counters.usage.and_then(|usage| {
        usage
            .input_tokens
            .checked_add(usage.cache_read_input_tokens)?
            .checked_add(usage.cache_write_input_tokens)?
            .checked_add(usage.output_tokens)
    });
    rows.push(field(
        "tokens",
        format!(
            "{} / {}",
            tokens.map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
            goal.budget
                .max_total_tokens
                .map_or_else(|| "no cap".to_owned(), |value| value.to_string())
        ),
    ));
    rows.push(field(
        "cost",
        format!(
            "{} / {}",
            goal.counters
                .estimated_cost_usd_nanos
                .map_or_else(|| "unknown".to_owned(), format_cost),
            goal.budget
                .max_cost_usd_nanos
                .map_or_else(|| "no cap".to_owned(), format_cost)
        ),
    ));
    rows.push(field("deadline", short_timestamp(goal.budget.deadline_ms)));
    let mut audits = format!("{}", goal.counters.audits);
    if goal.counters.check_failures > 0 {
        audits.push_str(&format!(
            " · {} failed {}",
            goal.counters.check_failures,
            if goal.counters.check_failures == 1 {
                "check"
            } else {
                "checks"
            }
        ));
    }
    rows.push(field("audits", audits));
    if let Some(check) = &goal.check {
        let consent = match goal.check_authorization {
            Some(GoalCheckAuthorization::Pending) | None => "needs consent",
            Some(GoalCheckAuthorization::Once) => "one check",
            Some(GoalCheckAuthorization::Consumed) => "consent used",
            Some(GoalCheckAuthorization::ForGoal) => "repeated checks",
            Some(GoalCheckAuthorization::TaskScoped) => "automatic in auto",
        };
        rows.push(field(
            "check",
            format!(
                "{} · {} · {consent}",
                safe(&check.command),
                format_duration_ms(check.timeout_ms)
            ),
        ));
    } else {
        rows.push(field("check", "none · independent model audit".to_owned()));
    }
    if !goal.notes.trim().is_empty() {
        rows.push(field("notes", preview(&safe(&goal.notes), width)));
    }
    if let Some(wait) = &goal.wait {
        rows.push(field(
            "wait",
            format!(
                "until {} · {}",
                super::workspace::goal_timestamp(wait.until_ms),
                safe(&wait.reason)
            ),
        ));
    }
    for item in &goal.checklist {
        let (glyph, style, text) = match item.state {
            GoalChecklistState::Pending => ("○", faint(), muted()),
            GoalChecklistState::InProgress => ("◐", info(), normal()),
            GoalChecklistState::Done => ("●", faint(), muted()),
            GoalChecklistState::Dropped => ("×", faint(), faint()),
        };
        let mut line = Line::styled(format!("  {glyph} "), style);
        line.push(safe(&item.text), text);
        rows.push(truncate_line(line, width));
    }
    if let Some(output) = &goal.check_output {
        let parts: Vec<&str> = output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        let mut line = Line::styled("  ✕ ", failure());
        line.push(
            format!(
                "last check {}",
                safe(parts.first().copied().unwrap_or_default())
            ),
            muted(),
        );
        if let Some(detail) = parts.get(1..).and_then(<[&str]>::last) {
            let detail = safe(detail);
            let room = width.saturating_sub(line.width() + 2);
            let count = detail.chars().count();
            let shown = if count <= room {
                detail
            } else if room == 0 {
                String::new()
            } else {
                let tail: String = detail.chars().skip(count + 1 - room).collect();
                format!("…{tail}")
            };
            line.push(format!("  {shown}"), muted());
        }
        rows.push(truncate_line(line, width));
    }
    if goal.audit_pending {
        let mut line = Line::styled("  ◇ ", warning());
        line.push("completion claim awaiting verification", normal());
        rows.push(truncate_line(line, width));
    }
    rows.extend(hold);
    rows
}

fn short_timestamp(ms: u64) -> String {
    match time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000) {
        Ok(time) => format!(
            "{} {:02}:{:02} UTC",
            time.date(),
            time.hour(),
            time.minute()
        ),
        Err(_) => "invalid timestamp".to_owned(),
    }
}

pub(super) fn goal_duration(ms: u64) -> String {
    if ms >= 3_600_000 {
        format!("{}h", ms / 3_600_000)
    } else if ms >= 60_000 {
        format!("{}m", ms / 60_000)
    } else {
        format!("{}s", ms / 1000)
    }
}

pub(super) fn compact_chord(chord: &str) -> String {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposerMode {
    Send,
    Steer,
    Queue,
    Approval,
}

impl ComposerMode {
    const fn placeholder(self) -> &'static str {
        match self {
            Self::Send => "Ask QQ…",
            Self::Steer => "Steer the run…",
            Self::Queue => "Queue for after this run…",
            Self::Approval => "Answer above",
        }
    }

    const fn disabled(self) -> bool {
        matches!(self, Self::Approval)
    }
}

pub(crate) fn rail_style(mode: ApprovalMode) -> Style {
    match mode {
        ApprovalMode::ReadOnly => faint(),
        ApprovalMode::Ask | ApprovalMode::Auto => accent(),
        ApprovalMode::Full => warning(),
    }
}

const RAIL: &str = "┃ ";
const RAIL_WIDTH: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CursorPosition {
    pub column: u16,
    pub row: u16,
}

pub(super) fn composer(
    app: &App,
    width: usize,
    max_rows: usize,
) -> (Vec<Line>, Option<(usize, usize)>) {
    let max_rows = max_rows.max(1);
    let mode = app.composer_mode();
    let rail = rail_style(app.effective_approval_mode());
    if let Some(choice) = &app.approval_amendment {
        let mut line = Line::styled(RAIL, warning());
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
                "what should it do instead? enter sends, esc cancels",
                faint(),
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
    let answering = app
        .pending_question()
        .and_then(|question| question.questions.get(app.question_answers.len()))
        .is_some_and(|question| question.free_text);
    if let Some(keys) = dock_keys(app).filter(|_| !answering && app.composer.text.is_empty()) {
        let mut line = Line::styled(RAIL, rail);
        for span in key_row(&keys).spans {
            line.push(span.text, span.style);
        }
        return (vec![truncate_line(line, width)], None);
    }
    if app.composer.text.is_empty() {
        let mut line = Line::styled(RAIL, rail);
        line.push(
            if answering {
                "Type your answer…"
            } else {
                mode.placeholder()
            },
            faint(),
        );
        let caret = (!mode.disabled() || answering).then_some((RAIL_WIDTH, 0));
        return (vec![truncate_line(line, width)], caret);
    }

    let content_width = width.saturating_sub(RAIL_WIDTH).max(1);
    let cursor = app.composer.cursor();
    let mut wrapped: Vec<Line> = Vec::new();
    let mut caret: Option<(usize, usize)> = None;
    let mut consumed = 0_usize;
    for part in app.composer.text.split('\n') {
        let content_rows = if part.is_empty() {
            vec![Line::default()]
        } else {
            wrap_line_chars(composer_row(part), content_width)
        };
        let cursor_in_part = cursor >= consumed && cursor <= consumed + part.len();
        let mut row_start = consumed;
        for content in content_rows {
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
                    caret = Some((RAIL_WIDTH + column, wrapped.len()));
                }
            }
            row_start += row_bytes;
            let mut row = Line::styled(RAIL, rail);
            for span in content.spans {
                row.push(span.text, span.style);
            }
            wrapped.push(row);
        }
        consumed += part.len() + 1;
    }

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
            let mut clipped = Line::styled("┃↑", rail);
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

pub(super) fn queued_drafts(app: &App, width: usize, budget: usize) -> Vec<Line> {
    let Some(session_id) = app.focused() else {
        return Vec::new();
    };
    let waiting_steers = app
        .sessions
        .get(&session_id)
        .and_then(|session| session.messages.as_ref())
        .into_iter()
        .flatten()
        .filter(|message| message.steering && message.state == MessageState::Queued)
        .map(|message| message.output.as_str());
    let drafts: Vec<&str> = waiting_steers
        .chain(app.queued_drafts(session_id))
        .collect();
    if drafts.is_empty() || budget == 0 {
        return Vec::new();
    }
    let rows: Vec<Vec<Line>> = drafts
        .into_iter()
        .map(|draft| {
            let safe: String = draft.chars().filter_map(terminal_safe_character).collect();
            wrap_line(
                Line::styled(safe, muted()),
                width.saturating_sub(RAIL_WIDTH).max(1),
            )
            .into_iter()
            .take(2)
            .map(|row| {
                let mut line = Line::styled("┋ ", faint());
                for span in row.spans {
                    line.push(span.text, span.style);
                }
                truncate_line(line, width)
            })
            .collect::<Vec<_>>()
        })
        .collect();
    let total: usize = rows.iter().map(Vec::len).sum();
    if total <= budget {
        return rows.into_iter().flatten().collect();
    }
    let mut kept: Vec<Vec<Line>> = Vec::new();
    let mut used = 1;
    for draft in rows.iter().rev() {
        if used + draft.len() > budget {
            break;
        }
        used += draft.len();
        kept.push(draft.clone());
    }
    kept.reverse();
    let hidden = rows.len() - kept.len();
    let mut out = vec![truncate_line(
        Line::styled(format!("┋ +{hidden} more queued"), faint()),
        width,
    )];
    out.extend(kept.into_iter().flatten());
    out
}

pub(super) fn dock_keys(app: &App) -> Option<Vec<(&'static str, &'static str)>> {
    if app.pending_question().is_some() {
        return None;
    }
    match app.mode() {
        Mode::Approval => {
            let edit = app
                .pending_approval()
                .is_some_and(|call| matches!(call.name.as_str(), "edit_file" | "write_file"));
            Some(if edit {
                vec![("y", "apply"), ("n", "deny"), ("Y / N", "with a note")]
            } else {
                vec![("y", "allow"), ("n", "deny"), ("Y / N", "with a note")]
            })
        }
        Mode::GoalForm => Some(match &app.overlay {
            Some(crate::input::Overlay::GoalForm(form)) if form.confirm => {
                vec![("y", "replace"), ("n", "edit"), ("esc", "back")]
            }
            _ => vec![
                ("tab", "next field"),
                ("←→", "budget"),
                ("enter", "create"),
                ("esc", "back"),
            ],
        }),
        Mode::GoalCheck => Some(vec![("enter", "apply"), ("↑↓", "choose"), ("esc", "back")]),
        Mode::Compose
        | Mode::Models
        | Mode::Profiles
        | Mode::ApprovalModes
        | Mode::Effort
        | Mode::Skills
        | Mode::Themes
        | Mode::Sessions
        | Mode::Commands
        | Mode::History => None,
    }
}

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

const MAX_SLASH_ROWS: usize = 8;

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
