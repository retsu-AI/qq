use super::*;
use qq_client::state::ToolCallTiming;

/// Runs with more than this many quiet tool calls fold into one summary row.
pub(super) const TOOL_FOLD_THRESHOLD: usize = 3;
/// Every marker line qq-core inserts into a result (omitted head/tail, scan
/// caps, unlisted entries) starts with this; markers are excluded from counts.
pub(super) const TOOL_MARKER_PREFIX: &str = "…[qq: ";
pub(super) const TOOL_SUBJECT_WIDTH: usize = 48;
pub(super) const MAX_TOOL_ERROR_BYTES: usize = 2 * 1024;
pub(super) const MAX_TOOL_ERROR_ROWS: usize = 6;
pub(super) const MAX_TOOL_DETAIL_BYTES: usize = 4 * 1024;
pub(super) const MAX_TOOL_ARGUMENT_ROWS: usize = 8;
pub(super) const MAX_TOOL_RESULT_ROWS: usize = 12;
/// Rows of live streamed output shown under a running call's one-liner.
pub(super) const MAX_LIVE_TAIL_ROWS: usize = 6;
pub(super) const TOOL_SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];
/// Files named in a folded read-only group before `+N`.
const FOLD_NAMED_FILES: usize = 3;

/// What the transcript needs to know about a tool call to draw its row,
/// derived once from the JSON arguments and result and cached by the
/// renderer under [`ToolRowKey`]. Nothing here depends on width or time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolRow {
    /// `Read`, `Edit`, `Run`, `Search`, `Spawn`, or the raw tool name.
    pub verb: &'static str,
    pub raw_name: bool,
    /// Path, command, query, or task: what the call acted on.
    pub subject: Option<String>,
    /// The subject is a path and may be middle-elided and hyperlinked.
    pub subject_is_path: bool,
    /// `212 lines`, `+12 −3`, `exit 0`, `14 hits · 3 files`.
    pub metric: Option<String>,
    /// The result was cut short by the runtime.
    pub truncated: bool,
    /// Arguments as `key: value` rows, for tools without a curated view.
    pub arguments: Vec<(String, String)>,
    /// Which end of the result the expanded view shows.
    pub body: ResultBody,
    /// Diff text for the expanded view and approvals, when the call has one.
    pub diff: Option<String>,
}

/// Which end of a result reads best: the head for content the model asked
/// to see (files, listings, matches), the tail for command output whose
/// verdict comes last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResultBody {
    Head,
    Tail,
}

/// Identity of a cached [`ToolRow`]: the call plus everything that changes
/// its derivation. Width and time are applied at draw time, not cached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ToolRowKey {
    pub id: ToolCallId,
    pub state: ToolCallState,
    pub result_len: usize,
    pub is_error: bool,
    pub has_display: bool,
}

impl ToolRowKey {
    pub(crate) fn of(call: &ToolCallSnapshot) -> Self {
        Self {
            id: call.id,
            state: call.state,
            result_len: call.result.as_ref().map_or(0, String::len),
            is_error: call.is_error,
            has_display: call.display.is_some(),
        }
    }
}

impl ToolRow {
    /// Derive the row from a call. The one place `serde_json` touches tool
    /// arguments on the render side.
    pub(crate) fn derive(call: &ToolCallSnapshot) -> Self {
        let arguments = if call.arguments.len() <= MAX_TOOL_DETAIL_BYTES {
            serde_json::from_str::<serde_json::Value>(&call.arguments).ok()
        } else {
            None
        };
        let argument_rows: Vec<(String, String)> =
            match arguments.as_ref().and_then(|v| v.as_object()) {
                Some(object) => object
                    .iter()
                    .map(|(key, value)| {
                        let text = match value {
                            serde_json::Value::String(text) => text.clone(),
                            other => other.to_string(),
                        };
                        (key.clone(), text)
                    })
                    .collect(),
                None => vec![(
                    "arguments".to_owned(),
                    bounded_tail(&call.arguments, MAX_TOOL_DETAIL_BYTES).to_owned(),
                )],
            };
        let string_argument = |key: &str| {
            arguments
                .as_ref()
                .and_then(|value| value.get(key))
                .and_then(|value| value.as_str())
                .map(str::to_owned)
        };
        let result = call.result.as_deref().unwrap_or_default();
        let truncated = result.lines().any(is_marker_line);
        let content_lines = || result.lines().filter(|line| !is_marker_line(line)).count();
        let has_result = call.result.is_some() && !call.is_error;
        let diff = match &call.display {
            Some(ToolCallDisplay::Diff { diff, .. }) => Some(diff.clone()),
            None => (matches!(call.name.as_str(), "edit_file" | "write_file")
                && has_result
                && looks_like_diff(result))
            .then(|| result.to_owned()),
        };

        let compact = || {
            let text = preview(&call.arguments, TOOL_SUBJECT_WIDTH);
            (!text.is_empty()).then_some(text)
        };
        let (verb, raw_name, subject, subject_is_path, metric) = match call.name.as_str() {
            "read_file" => (
                "Read",
                false,
                string_argument("path").or_else(compact),
                arguments.is_some(),
                has_result.then(|| count_noun(content_lines(), "line", "lines")),
            ),
            // `tree <path> depth= entries=<shown>/<total> files= dirs=` heads
            // the result; `list_dir` is its alias.
            "tree" | "list_dir" => (
                "List",
                false,
                string_argument("path")
                    .or_else(|| (call.name == "tree").then(|| ".".to_owned()))
                    .or_else(compact),
                arguments.is_some(),
                has_result.then(|| tree_metric(result)),
            ),
            "search" => (
                "Search",
                false,
                string_argument("query")
                    .map(|query| format!("\"{query}\""))
                    .or_else(compact),
                false,
                has_result.then(|| search_metric(result)),
            ),
            "edit_file" => (
                "Edit",
                false,
                string_argument("path").or_else(compact),
                arguments.is_some(),
                diff.as_deref()
                    .map(diff_metric)
                    .or_else(|| has_result.then(|| format_result_size(result.len()))),
            ),
            "write_file" => (
                "Write",
                false,
                string_argument("path").or_else(compact),
                arguments.is_some(),
                diff.as_deref().map(diff_metric).or_else(|| {
                    string_argument("content")
                        .map(|content| count_noun(content.lines().count(), "line", "lines"))
                }),
            ),
            "shell" => {
                let cwd = string_argument("cwd");
                let command = string_argument("command").map(|command| match cwd {
                    Some(cwd) => format!("{command}  (in {cwd})"),
                    None => command,
                });
                // `shell exit=<code> elapsed=<s> bytes=<n>` heads the result.
                let exit = header_field(result, "shell", "exit").map(|code| format!("exit {code}"));
                ("Run", false, command, false, exit)
            }
            "spawn_agent" => (
                "Spawn",
                false,
                string_argument("task").map(|task| first_sentence(&task)),
                false,
                None,
            ),
            "web_fetch" => (
                "Fetch",
                false,
                string_argument("url"),
                false,
                has_result.then(|| format_result_size(result.len())),
            ),
            name => {
                // MCP tools arrive as `mcp__server__tool`; show `server · tool`
                // and the first string argument as the subject.
                let (verb, raw) = match name.strip_prefix("mcp__") {
                    Some(rest) => {
                        let _ = rest;
                        ("MCP", true)
                    }
                    None => ("", true),
                };
                let subject = arguments
                    .as_ref()
                    .and_then(|value| value.as_object())
                    .and_then(|object| {
                        object
                            .values()
                            .find_map(|value| value.as_str().map(str::to_owned))
                    })
                    .or_else(|| {
                        let text = preview(&call.arguments, TOOL_SUBJECT_WIDTH);
                        (!text.is_empty()).then_some(text)
                    });
                (
                    verb,
                    raw,
                    subject,
                    false,
                    has_result.then(|| format_result_size(result.len())),
                )
            }
        };
        let body = match call.name.as_str() {
            "shell" => ResultBody::Tail,
            _ => ResultBody::Head,
        };
        Self {
            verb,
            raw_name,
            subject,
            subject_is_path,
            metric,
            truncated,
            arguments: argument_rows,
            body,
            diff,
        }
    }

    /// The name shown in the row: the verb, or `server · tool` for MCP, or
    /// the raw tool name.
    fn label(&self, call: &ToolCallSnapshot) -> String {
        if !self.raw_name {
            return self.verb.to_owned();
        }
        match call.name.strip_prefix("mcp__") {
            Some(rest) => match rest.split_once("__") {
                Some((server, tool)) => format!("{server} · {tool}"),
                None => rest.to_owned(),
            },
            None => call.name.clone(),
        }
    }
}

/// A line qq-core inserted rather than the tool's content.
pub(super) fn is_marker_line(line: &str) -> bool {
    line.starts_with(TOOL_MARKER_PREFIX)
}

/// `value` of `key=value` from a result's header line, when the result
/// starts with `<tool> …`.
fn header_field<'a>(result: &'a str, tool: &str, key: &str) -> Option<&'a str> {
    let header = result.lines().next()?;
    let rest = header.strip_prefix(tool)?;
    if !rest.starts_with(' ') {
        return None;
    }
    rest.split_whitespace()
        .find_map(|field| field.split_once('=').filter(|(k, _)| *k == key))
        .map(|(_, value)| value)
}

/// The result without its `<tool> k=v …` header line; the row's metric
/// already shows what the header says.
fn strip_header<'a>(result: &'a str, tool: &str) -> &'a str {
    let Some(header) = result.lines().next() else {
        return result;
    };
    if !header
        .strip_prefix(tool)
        .is_some_and(|rest| rest.starts_with(' '))
    {
        return result;
    }
    result.get(header.len() + 1..).unwrap_or_default()
}

/// `<shown> hits · <files> files` from the `search "…" mode= matches=<shown>/
/// <total> files=<n>` header; `no matches` when the total is zero.
fn search_metric(result: &str) -> String {
    let (shown, total) = header_field(result, "search", "matches")
        .and_then(|value| value.split_once('/'))
        .map_or((0, "0"), |(shown, total)| {
            (shown.parse::<usize>().unwrap_or(0), total)
        });
    if total == "0" {
        return "no matches".to_owned();
    }
    let files = header_field(result, "search", "files")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut metric = count_noun(shown, "hit", "hits");
    if total != shown.to_string() {
        metric = format!("{shown}/{total} hits");
    }
    metric.push_str(" · ");
    metric.push_str(&count_noun(files, "file", "files"));
    metric
}

/// `<n> entries` from the `tree … entries=<shown>/<total>` header, with the
/// total when entries were left unlisted.
fn tree_metric(result: &str) -> String {
    match header_field(result, "tree", "entries").and_then(|value| value.split_once('/')) {
        Some((shown, total)) if shown != total => format!("{shown}/{total} entries"),
        Some((shown, _)) => count_noun(shown.parse().unwrap_or(0), "entry", "entries"),
        None => count_noun(
            result.lines().filter(|line| !is_marker_line(line)).count(),
            "entry",
            "entries",
        ),
    }
}

/// `+12 −3` from a unified diff.
fn diff_metric(diff: &str) -> String {
    let mut added = 0_usize;
    let mut removed = 0_usize;
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
    format!("+{added} −{removed}")
}

fn first_sentence(text: &str) -> String {
    let trimmed = text.trim();
    let end = trimmed
        .find(['.', '\n'])
        .map_or(trimmed.len(), |index| index);
    trimmed[..end].trim_end().to_owned()
}

/// Shorten a path from the middle so the file name always survives:
/// `crates/qq-tui/src/view/tools.rs` at 24 columns becomes
/// `crates/…/view/tools.rs`.
pub(crate) fn elide_path(path: &str, width: usize) -> String {
    let count = path.chars().count();
    if count <= width {
        return path.to_owned();
    }
    if width < 6 {
        return preview(path, width);
    }
    let Some((head, tail)) = path.rsplit_once('/') else {
        return preview(path, width);
    };
    // Keep the file name whole when it fits with an ellipsis and one head
    // segment; otherwise fall back to a plain truncation of the name.
    let tail_count = tail.chars().count();
    if tail_count + 2 >= width {
        return format!(
            "…{}",
            tail.chars()
                .skip(tail_count + 1 - width)
                .collect::<String>()
        );
    }
    let budget = width - tail_count - 2;
    let mut kept = String::new();
    for segment in head.split('/') {
        let candidate_len = kept.chars().count() + segment.chars().count() + 1;
        if candidate_len > budget {
            break;
        }
        kept.push_str(segment);
        kept.push('/');
    }
    if kept.is_empty() {
        format!("…/{tail}")
    } else {
        format!("{kept}…/{tail}")
    }
}

/// Live timing for the row: how long a running call has run and when it
/// last produced output, or its wall-clock start and end when finished.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RowClock {
    pub timing: ToolCallTiming,
    pub now_ms: u64,
}

impl RowClock {
    /// Elapsed since start, live for running calls, fixed for finished ones.
    fn duration(&self, running: bool) -> Option<u64> {
        let started = self.timing.started_at_ms?;
        let end = if running {
            self.now_ms
        } else {
            self.timing.finished_at_ms?
        };
        Some(end.saturating_sub(started))
    }
}

/// Per-call display state the caller resolves before rendering.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ToolRowContext<'a> {
    pub row: &'a ToolRow,
    pub clock: RowClock,
    /// This call's body is shown.
    pub expanded: bool,
    /// The transcript folds quiet finished blocks to one row.
    pub fold: bool,
    /// The transcript cursor rests on this call.
    pub selected: bool,
}

/// Rows rendered beneath a tool call that spawned a child session.
pub(super) type ChildRows<'a> = &'a dyn Fn(ToolCallId, usize) -> Vec<Line>;

/// Rows of the approval block for the call awaiting an answer; empty for
/// every other call.
pub(super) type ApprovalRows<'a> = &'a dyn Fn(ToolCallId, usize) -> Vec<Line>;

/// Resolves a call to its cached row and display state.
pub(super) type RowLookup<'a> = &'a dyn Fn(&ToolCallSnapshot) -> ToolRowContext<'a>;

/// Renders one run's tool calls: one gutter line per call, with errors,
/// expansion, and live output adding bounded body rows. In folded detail a
/// block of quiet finished calls is one summary row instead. A running
/// call's live output shows at every detail level: it is the thing the user
/// is waiting for.
pub(super) fn render_tool_calls(
    calls: &[&ToolCallSnapshot],
    live_output: &HashMap<ToolCallId, String>,
    lookup: RowLookup<'_>,
    tick: usize,
    width: usize,
    children: ChildRows<'_>,
    approval: ApprovalRows<'_>,
) -> Vec<Line> {
    let quiet = |call: &ToolCallSnapshot| {
        call.state == ToolCallState::Completed
            && !call.is_error
            && !lookup(call).expanded
            && !lookup(call).selected
            && children(call.id, width).is_empty()
    };
    let fold = calls.first().is_some_and(|call| lookup(call).fold);
    if fold && calls.len() > TOOL_FOLD_THRESHOLD && calls.iter().all(|call| quiet(call)) {
        return vec![tool_fold_line(calls, lookup, width)];
    }
    let mut lines = Vec::with_capacity(calls.len());
    for call in calls {
        let context = lookup(call);
        lines.push(tool_summary_line(call, context, tick, width));
        lines.extend(approval(call.id, width));
        if call.is_error
            && let Some(result) = call.result.as_deref()
        {
            lines.extend(tool_error_lines(strip_header(result, &call.name), width));
        }
        if context.expanded {
            lines.extend(tool_expanded_lines(call, context, width));
        }
        if call.state == ToolCallState::Running
            && let Some(output) = live_output.get(&call.id)
        {
            lines.extend(tool_live_output_lines(output, width));
        }
        // A spawned child renders under the call that created it so the
        // parent transcript shows delegated work in execution order.
        lines.extend(children(call.id, width));
    }
    lines
}

/// `▸ Read ×4  cache.rs, refresh.rs, lib.rs, +1` for a group of quiet calls;
/// mixed verbs list each with its count.
pub(super) fn tool_fold_line(
    calls: &[&ToolCallSnapshot],
    lookup: RowLookup<'_>,
    width: usize,
) -> Line {
    let mut counts: Vec<(String, usize)> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    for call in calls {
        let row = lookup(call).row;
        let label = row.label(call);
        match counts.iter_mut().find(|(name, _)| *name == label) {
            Some((_, count)) => *count += 1,
            None => counts.push((label, 1)),
        }
        if row.subject_is_path
            && let Some(subject) = &row.subject
        {
            let name = subject.rsplit('/').next().unwrap_or(subject).to_owned();
            if !files.contains(&name) {
                files.push(name);
            }
        }
    }
    let mut line = Line::styled("   ", muted());
    line.push("▸ ", accent());
    for (index, (name, count)) in counts.iter().enumerate() {
        if index > 0 {
            line.push("  ", muted());
        }
        line.push(name, normal());
        line.push(format!(" ×{count}"), muted());
    }
    if !files.is_empty() {
        line.push("  ", muted());
        let shown = files
            .iter()
            .take(FOLD_NAMED_FILES)
            .cloned()
            .collect::<Vec<_>>();
        line.push(shown.join(", "), muted());
        if files.len() > FOLD_NAMED_FILES {
            line.push(format!(", +{}", files.len() - FOLD_NAMED_FILES), muted());
        }
    }
    truncate_line(line, width)
}

/// One gutter line: state glyph, verb, subject, metric, and duration. The
/// subject column is fixed so metrics align down a run; paths elide from
/// the middle so the file name always shows.
pub(super) fn tool_summary_line(
    call: &ToolCallSnapshot,
    context: ToolRowContext<'_>,
    tick: usize,
    width: usize,
) -> Line {
    let row = context.row;
    let (glyph, glyph_style) = tool_state_glyph(call, tick);
    let mut line = Line::styled(if context.selected { " ▶ " } else { "   " }, accent());
    line.push(glyph, glyph_style);
    line.push(" ", muted());
    let label = row.label(call);
    let label_width = label.chars().count().max(6);
    line.push(
        format!("{label:<label_width$} "),
        if row.raw_name { muted() } else { normal() },
    );
    let running = call.state == ToolCallState::Running;
    let mut right = Line::default();
    if let Some(metric) = &row.metric {
        right.push(metric.clone(), muted());
        if row.truncated {
            right.push(" · truncated", muted());
        }
    }
    if call.state != ToolCallState::Completed {
        if !right.is_empty() {
            right.push(" · ", muted());
        }
        right.push(
            tool_state_label(call.state),
            match call.state {
                ToolCallState::Failed | ToolCallState::Denied => failure(),
                ToolCallState::AwaitingApproval => warning(),
                ToolCallState::Running => info(),
                ToolCallState::Requested
                | ToolCallState::Interrupted
                | ToolCallState::Completed => muted(),
            },
        );
    }
    if let Some(duration) = context.clock.duration(running) {
        right.push(
            format!("  {}", format_duration_ms(duration)),
            if running { info() } else { muted() },
        );
    }
    // The subject occupies a fixed column so metrics line up down a run;
    // narrow panes shrink the column, and paths give up their middle first.
    let right_width = right.width();
    let available = width
        .saturating_sub(line.width())
        .saturating_sub(if right_width > 0 { right_width + 2 } else { 0 });
    let column = available.clamp(6, TOOL_SUBJECT_WIDTH);
    if let Some(subject) = &row.subject {
        let text = if row.subject_is_path {
            elide_path(subject, column)
        } else {
            preview(subject, column)
        };
        let shown = text.chars().count();
        line.push(text, normal());
        if !right.is_empty() {
            line.push(" ".repeat(column.saturating_sub(shown) + 2), muted());
        }
    } else if !right.is_empty() {
        line.push(" ".repeat(column + 2), muted());
    }
    for span in right.spans {
        line.push(span.text, span.style);
    }
    truncate_line(line, width)
}

pub(super) fn tool_state_glyph(call: &ToolCallSnapshot, tick: usize) -> (&'static str, Style) {
    match call.state {
        ToolCallState::Running => (spinner(tick), info()),
        ToolCallState::Requested => ("○", muted()),
        ToolCallState::Completed => {
            if call.is_error {
                ("✕", failure())
            } else {
                ("●", muted())
            }
        }
        ToolCallState::Failed | ToolCallState::Denied => ("✕", failure()),
        ToolCallState::AwaitingApproval => ("◇", warning()),
        ToolCallState::Interrupted => ("◌", muted()),
    }
}

/// Errors are the one case where content matters by default: show a bounded
/// tail of the error text under the gutter line.
pub(super) fn tool_error_lines(result: &str, width: usize) -> Vec<Line> {
    let text = bounded_tail(result, MAX_TOOL_ERROR_BYTES);
    let total = text.lines().count();
    let mut lines = Vec::new();
    if total > MAX_TOOL_ERROR_ROWS || text.len() < result.len() {
        lines.push(Line::styled("     …", muted()));
    }
    for line in text.lines().skip(total.saturating_sub(MAX_TOOL_ERROR_ROWS)) {
        lines.push(truncate_line(
            Line::styled(format!("     {line}"), failure()),
            width,
        ));
    }
    lines
}

/// `HH:MM:SS` in UTC from server milliseconds. Local-time rendering waits on
/// a timezone source that does not read the environment inside the frame.
pub(crate) fn format_clock(ms: u64) -> String {
    let seconds = ms / 1000;
    format!(
        "{:02}:{:02}:{:02}",
        (seconds / 3600) % 24,
        (seconds / 60) % 60,
        seconds % 60
    )
}

/// Expanded detail: the timing line, then what the call produced. Known
/// tools never show their JSON: reads and searches show the head of the
/// result, edits show the diff head-first with line numbers, and commands
/// show the tail of their output. Unknown and MCP tools list arguments as
/// `key: value` rows before the result tail.
pub(super) fn tool_expanded_lines(
    call: &ToolCallSnapshot,
    context: ToolRowContext<'_>,
    width: usize,
) -> Vec<Line> {
    let mut lines = Vec::new();
    let row = context.row;
    let timing = context.clock.timing;
    let running = call.state == ToolCallState::Running;
    // The timing line answers "is this new, or am I still waiting on the
    // same thing?": wall-clock start, then finish or a live elapsed clock,
    // then when output last arrived.
    let mut when = Line::styled("     ", muted());
    let mut fields = Vec::new();
    if let Some(started) = timing.started_at_ms {
        fields.push(format!("started {}", format_clock(started)));
    }
    match (running, timing.finished_at_ms) {
        (true, _) => {
            if let Some(duration) = context.clock.duration(true) {
                fields.push(format!("running {}", format_duration_ms(duration)));
            }
            if let Some(last) = timing.last_output_at_ms {
                fields.push(format!("last output {}", format_clock(last)));
            }
        }
        (false, Some(finished)) => fields.push(format!("→ {}", format_clock(finished))),
        (false, None) => {}
    }
    if !fields.is_empty() {
        when.push(fields.join(" · "), muted());
        lines.push(truncate_line(when, width));
    }
    if row.raw_name {
        for (shown, (key, value)) in row.arguments.iter().enumerate() {
            if shown == MAX_TOOL_ARGUMENT_ROWS {
                lines.push(Line::styled("     …", muted()));
                break;
            }
            let mut line = Line::styled(format!("     {key}: "), muted());
            line.push(preview(value, width.saturating_sub(line.width())), normal());
            lines.push(truncate_line(line, width));
        }
    }
    if let Some(diff) = &row.diff {
        lines.extend(diff_lines(diff, MAX_TOOL_RESULT_ROWS, width));
        return lines;
    }
    let Some(result) = call.result.as_deref().filter(|_| !call.is_error) else {
        return lines;
    };
    let result = strip_header(result, &call.name);
    match row.body {
        ResultBody::Head => {
            let total = result.lines().count();
            for line in result
                .lines()
                .filter(|line| !is_marker_line(line))
                .take(MAX_TOOL_RESULT_ROWS)
            {
                lines.push(truncate_line(
                    Line::styled(format!("     {}", preview(line, width)), muted()),
                    width,
                ));
            }
            if total > MAX_TOOL_RESULT_ROWS {
                lines.push(Line::styled(
                    format!(
                        "     … {} more",
                        count_noun(total - MAX_TOOL_RESULT_ROWS, "line", "lines")
                    ),
                    muted(),
                ));
            }
        }
        ResultBody::Tail => {
            let text = bounded_tail(result, MAX_TOOL_DETAIL_BYTES);
            let total = text.lines().count();
            if total > MAX_TOOL_RESULT_ROWS || text.len() < result.len() {
                lines.push(Line::styled("     …", muted()));
            }
            for line in text
                .lines()
                .skip(total.saturating_sub(MAX_TOOL_RESULT_ROWS))
            {
                lines.push(truncate_line(
                    Line::styled(format!("     {}", preview(line, width)), muted()),
                    width,
                ));
            }
        }
    }
    lines
}

/// A unified diff, head-first, with new-file line numbers in the gutter and
/// the role tints behind added and removed lines. Shows at most `max_rows`
/// and says how many more there are.
pub(crate) fn diff_lines(diff: &str, max_rows: usize, width: usize) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut new_line: Option<usize> = None;
    let total = diff.lines().count();
    for (shown, text) in diff.lines().enumerate() {
        if shown == max_rows {
            lines.push(Line::styled(
                format!("     … {} more", count_noun(total - shown, "line", "lines")),
                muted(),
            ));
            break;
        }
        if text.starts_with("@@") {
            // `@@ -a,b +c,d @@`: the new-file start is `c`.
            new_line = text
                .split_whitespace()
                .nth(2)
                .and_then(|range| range.strip_prefix('+'))
                .and_then(|range| range.split(',').next())
                .and_then(|start| start.parse().ok());
            lines.push(truncate_line(
                Line::styled(format!("     {text}"), muted()),
                width,
            ));
            continue;
        }
        if text.starts_with("+++") || text.starts_with("---") {
            continue;
        }
        let style = diff_line_style(text);
        let (number, advance) = match text.chars().next() {
            Some('-') => (None, false),
            _ => (new_line, true),
        };
        let gutter = match number {
            Some(number) => format!(" {number:>4} "),
            None => "      ".to_owned(),
        };
        if advance && let Some(number) = new_line.as_mut() {
            *number += 1;
        }
        let mut line = Line::styled(gutter, muted());
        line.push(text, style);
        pad_line(&mut line, width);
        lines.push(truncate_line(line, width));
    }
    lines
}

/// The last few complete lines of a running call's streamed output, muted
/// and literal (character wrap, never reflowed) under the call's one-liner.
/// Only whole lines render: a chunk may end mid-line, and a partial line
/// reads as garbage until its newline arrives.
pub(super) fn tool_live_output_lines(output: &str, width: usize) -> Vec<Line> {
    let complete = &output[..output.rfind('\n').map_or(0, |index| index + 1)];
    let content_width = width.saturating_sub(5).max(1);
    let total = complete.lines().count();
    let mut rows = Vec::new();
    for line in complete
        .lines()
        .skip(total.saturating_sub(MAX_LIVE_TAIL_ROWS))
    {
        let safe = line
            .chars()
            .filter_map(terminal_safe_character)
            .collect::<String>();
        rows.extend(wrap_line_chars(Line::styled(safe, muted()), content_width));
    }
    // Long lines wrap into extra rows; keep the tail bounded regardless.
    let excess = rows.len().saturating_sub(MAX_LIVE_TAIL_ROWS);
    if excess > 0 {
        rows.drain(..excess);
    }
    indent_lines(rows, "     ", muted(), width)
}

/// Whether text is unified-diff-shaped: a hunk header, or both added and
/// removed lines. Prose summaries ("Edited x: replaced 1 occurrence(s).")
/// never match, so diff coloring only applies to an actual diff.
pub(super) fn looks_like_diff(text: &str) -> bool {
    let mut added = false;
    let mut removed = false;
    for line in text.lines() {
        if line.starts_with("@@") {
            return true;
        }
        added |= line.starts_with('+');
        removed |= line.starts_with('-');
        if added && removed {
            return true;
        }
    }
    false
}

pub(super) fn count_noun(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

pub(super) fn format_result_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{}.{} KB", bytes / 1024, (bytes % 1024) * 10 / 1024)
    }
}

pub(super) const fn tool_state_label(state: ToolCallState) -> &'static str {
    match state {
        ToolCallState::Requested => "requested",
        ToolCallState::AwaitingApproval => "awaiting approval",
        ToolCallState::Running => "running",
        ToolCallState::Completed => "completed",
        ToolCallState::Failed => "failed",
        ToolCallState::Denied => "denied",
        ToolCallState::Interrupted => "interrupted",
    }
}

/// How the test entry shows calls: rows, folded, or every body expanded.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SimpleDetail {
    Rows,
    Folded,
    Expanded,
}

/// Test entry: derive rows on the fly and render with one detail level for
/// every call, no selection, no timing, and no approval block.
#[cfg(test)]
pub(super) fn render_tool_calls_simple(
    calls: &[&ToolCallSnapshot],
    live_output: &HashMap<ToolCallId, String>,
    detail: SimpleDetail,
    tick: usize,
    width: usize,
    children: ChildRows<'_>,
) -> Vec<Line> {
    let rows: HashMap<ToolCallId, ToolRow> = calls
        .iter()
        .map(|call| (call.id, ToolRow::derive(call)))
        .collect();
    let lookup = |call: &ToolCallSnapshot| ToolRowContext {
        row: &rows[&call.id],
        clock: RowClock {
            timing: ToolCallTiming::default(),
            now_ms: 0,
        },
        expanded: detail == SimpleDetail::Expanded,
        fold: detail == SimpleDetail::Folded,
        selected: false,
    };
    render_tool_calls(
        calls,
        live_output,
        &lookup,
        tick,
        width,
        children,
        &|_, _| Vec::new(),
    )
}
