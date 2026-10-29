use std::cell::RefCell;

use super::*;
use markdown::{CODE_PANEL_INSET, panel_content_width, panel_rows, panel_rows_at};
use qq_client::state::ToolCallTiming;
use qq_protocol::ToolErrorKind;

pub(super) const TOOL_FOLD_THRESHOLD: usize = 3;
pub(super) const TOOL_MARKER_PREFIX: &str = "…[qq: ";
pub(super) const TOOL_SUBJECT_WIDTH: usize = 48;
pub(super) const MAX_TOOL_ERROR_BYTES: usize = 2 * 1024;
pub(super) const MAX_TOOL_ERROR_ROWS: usize = 6;
pub(super) const MAX_TOOL_DETAIL_BYTES: usize = 4 * 1024;
pub(super) const MAX_TOOL_ARGUMENT_ROWS: usize = 8;
pub(super) const MAX_TOOL_RESULT_ROWS: usize = 12;
pub(super) const MAX_LIVE_TAIL_ROWS: usize = 6;
pub(super) const TOOL_SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];
const FOLD_NAMED_FILES: usize = 3;
const TRUNCATED_SUFFIX: &str = " · truncated";
const TOOL_PANEL_INDENT: &str = "   ";
pub(super) const TOOL_PANEL_PADDING_ROWS: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolRow {
    pub verb: &'static str,
    pub raw_name: bool,
    pub subject: Option<String>,
    pub subject_is_path: bool,
    pub metric: Option<String>,
    pub truncated: bool,
    pub arguments: Vec<(String, String)>,
    pub body: ResultBody,
    pub diff: Option<String>,
    panel: RefCell<Option<(usize, Vec<Line>)>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResultBody {
    Head,
    Tail,
}

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
                has_result.then(|| read_metric(result, content_lines())),
            ),
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
                edit_subject(arguments.as_ref()).or_else(compact),
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
            "shell" | "exec" => {
                let cwd = string_argument("cwd");
                let command = string_argument("command")
                    .or_else(|| {
                        let program = string_argument("program")?;
                        let args = arguments
                            .as_ref()
                            .and_then(|value| value.get("args"))
                            .and_then(|value| value.as_array())
                            .map(|items| {
                                items
                                    .iter()
                                    .filter_map(|item| item.as_str())
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            })
                            .unwrap_or_default();
                        Some(if args.is_empty() {
                            program
                        } else {
                            format!("{program} {args}")
                        })
                    })
                    .map(|command| match cwd {
                        Some(cwd) => format!("{command}  (in {cwd})"),
                        None => command,
                    });
                let exit =
                    header_field(result, &call.name, "exit").map(|code| format!("exit {code}"));
                ("Run", false, command, false, exit)
            }
            "spawn_agent" => (
                "Spawn",
                false,
                string_argument("task").map(|task| first_sentence(&task)),
                false,
                None,
            ),
            "read_tool_result" => (
                "Recall",
                false,
                string_argument("handle").or_else(compact),
                false,
                has_result.then(|| stored_output_metric(result)),
            ),
            "web_fetch" => (
                "Fetch",
                false,
                string_argument("url"),
                false,
                has_result.then(|| format_result_size(result.len())),
            ),
            name => {
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
            "shell" | "exec" => ResultBody::Tail,
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
            panel: RefCell::new(None),
        }
    }

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

pub(super) fn is_marker_line(line: &str) -> bool {
    line.starts_with(TOOL_MARKER_PREFIX)
}

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

fn strip_header<'a>(result: &'a str, tool: &str) -> &'a str {
    let Some(header) = result.lines().next() else {
        return result;
    };
    let Some(rest) = header
        .strip_prefix(tool)
        .filter(|rest| rest.starts_with(' '))
    else {
        return result;
    };
    if rest[1..].split_once(": ").is_some_and(|(index, _)| {
        !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        return result;
    }
    result.get(header.len() + 1..).unwrap_or_default()
}

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

fn header_word(name: &str) -> &str {
    match name {
        "read_file" => "read",
        "edit_file" => "edit",
        "write_file" => "write",
        "list_dir" => "tree",
        other => other,
    }
}

fn edit_subject(arguments: Option<&serde_json::Value>) -> Option<String> {
    let arguments = arguments?;
    if let Some(path) = arguments.get("path").and_then(|value| value.as_str()) {
        return Some(path.to_owned());
    }
    let edits = arguments.get("edits")?.as_array()?;
    let mut paths: Vec<&str> = Vec::new();
    for edit in edits {
        if let Some(path) = edit.get("path").and_then(|value| value.as_str())
            && !paths.contains(&path)
        {
            paths.push(path);
        }
    }
    let first = paths.first()?;
    Some(match paths.len() {
        1 => (*first).to_owned(),
        n => format!("{first} +{} files", n - 1),
    })
}

fn read_metric(result: &str, body_lines: usize) -> String {
    let Some(header) = result.lines().next() else {
        return count_noun(body_lines, "line", "lines");
    };
    let Some(rest) = header.strip_prefix("read ") else {
        return count_noun(body_lines, "line", "lines");
    };
    let mut tokens = rest.split_whitespace().skip(1);
    match tokens.next() {
        Some("unchanged") => "unchanged".to_owned(),
        Some("outline") => {
            match header_field(result, "read", "items").and_then(|value| value.split_once('/')) {
                Some((shown, total)) if shown != total => format!("{shown}/{total} items"),
                Some((shown, _)) => count_noun(shown.parse().unwrap_or(0), "item", "items"),
                None => "outline".to_owned(),
            }
        }
        Some("info") => "info".to_owned(),
        Some(window) => match window.strip_prefix('L').and_then(|w| w.split_once('/')) {
            Some((shown, total)) => {
                let total: usize = total.parse().unwrap_or(0);
                let mut metric = if shown.contains(',') || shown.contains('-') {
                    format!("L{shown} of {}", count_noun(total, "line", "lines"))
                } else {
                    format!("L{shown} of {total}")
                };
                if header_field(result, "read", "truncated").is_some() {
                    metric.push_str(" · truncated");
                }
                metric
            }
            None => count_noun(body_lines.saturating_sub(1), "line", "lines"),
        },
        None => count_noun(body_lines.saturating_sub(1), "line", "lines"),
    }
}

fn stored_output_metric(result: &str) -> String {
    if let Some((shown, total)) =
        header_field(result, "read_tool_result", "matches").and_then(|value| value.split_once('/'))
    {
        return if shown == total {
            count_noun(shown.parse().unwrap_or(0), "hit", "hits")
        } else {
            format!("{shown}/{total} hits")
        };
    }
    let Some(header) = result.lines().next() else {
        return String::new();
    };
    match header
        .split_whitespace()
        .nth(2)
        .and_then(|window| window.strip_prefix('L'))
        .and_then(|window| window.split_once('/'))
    {
        Some((shown, total)) => format!(
            "L{shown} of {}",
            count_noun(total.parse().unwrap_or(0), "line", "lines")
        ),
        None => String::new(),
    }
}

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
    let tail_count = tail.chars().count();
    if tail_count + 2 > width {
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

#[derive(Debug, Clone, Copy)]
pub(crate) struct RowClock {
    pub timing: ToolCallTiming,
    pub now_ms: u64,
}

impl RowClock {
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

#[derive(Debug, Clone, Copy)]
pub(crate) struct ToolRowContext<'a> {
    pub row: &'a ToolRow,
    pub clock: RowClock,
    pub expanded: bool,
    pub inline_detail: bool,
    pub fold: bool,
    pub selected: bool,
}

pub(super) type ChildRows<'a> = &'a dyn Fn(ToolCallId, usize) -> Vec<Line>;

pub(super) type ApprovalRows<'a> = &'a dyn Fn(ToolCallId, usize) -> Vec<Line>;

pub(super) type RowLookup<'a> = &'a dyn Fn(&ToolCallSnapshot) -> ToolRowContext<'a>;

pub(super) fn render_tool_calls(
    calls: &[&ToolCallSnapshot],
    live_output: &HashMap<ToolCallId, String>,
    lookup: RowLookup<'_>,
    tick: usize,
    width: usize,
    children: ChildRows<'_>,
    approval: ApprovalRows<'_>,
) -> Vec<Line> {
    let shown: Vec<&ToolCallSnapshot>;
    let calls = if calls.iter().any(|call| call.is_error) {
        shown = calls
            .iter()
            .enumerate()
            .filter(|&(index, call)| {
                !absorbed_correction(calls, index) || {
                    let context = lookup(call);
                    context.expanded || context.selected || !children(call.id, width).is_empty()
                }
            })
            .map(|(_, call)| *call)
            .collect();
        shown.as_slice()
    } else {
        calls
    };
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
        if let Some(result) = call.result.as_deref() {
            let panel = match (call.error_kind(), call.is_error) {
                (Some(ToolErrorKind::Failure), _) | (None, true) => {
                    Some((MAX_TOOL_ERROR_ROWS, failure()))
                }
                (Some(ToolErrorKind::Outcome), _) if context.expanded && context.inline_detail => {
                    None
                }
                (Some(ToolErrorKind::Outcome), _) => Some((1, warning())),
                (Some(ToolErrorKind::Correction), _) | (None, false) => None,
            };
            if let Some((rows, style)) = panel {
                lines.extend(tool_error_lines(
                    strip_header(result, header_word(&call.name)),
                    rows,
                    MAX_TOOL_ERROR_BYTES,
                    style,
                    width,
                ));
            }
        }
        if context.expanded && context.inline_detail {
            lines.extend(tool_expanded_lines(call, context, width));
        }
        if call.state == ToolCallState::Running
            && let Some(output) = live_output.get(&call.id)
        {
            lines.extend(tool_live_output_lines(output, width));
        }
        lines.extend(children(call.id, width));
    }
    lines
}

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
    let mut line = Line::styled(" ▸ ", accent());
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

pub(super) fn tool_summary_line(
    call: &ToolCallSnapshot,
    context: ToolRowContext<'_>,
    tick: usize,
    width: usize,
) -> Line {
    let row = context.row;
    let (glyph, glyph_style) = tool_state_glyph(call, tick);
    let mut line = Line::styled(if context.selected { "▶" } else { " " }, accent());
    line.push(glyph, glyph_style);
    line.push(" ", muted());
    let label = row.label(call);
    let label_width = label.chars().count().max(6);
    line.push(
        format!("{label:<label_width$} "),
        if row.raw_name { muted() } else { normal() },
    );
    let mut used = 3 + label_width + 1;
    let running = call.state == ToolCallState::Running;
    let metric = row.metric.as_deref().filter(|metric| !metric.is_empty());
    let metric_width = metric.map_or(0, |metric| {
        text_width(metric)
            + if row.truncated {
                text_width(TRUNCATED_SUFFIX)
            } else {
                0
            }
    });
    let exit_in_metric = call.error_kind() == Some(ToolErrorKind::Outcome)
        && metric.is_some()
        && call
            .result
            .as_deref()
            .and_then(|result| header_field(result, &call.name, "exit"))
            .is_some();
    let state = match call.error_kind() {
        Some(kind @ (ToolErrorKind::Correction | ToolErrorKind::Outcome)) => {
            Some(tool_error_label(call, kind))
        }
        Some(ToolErrorKind::Failure) | None => {
            (call.state != ToolCallState::Completed).then(|| {
                (
                    tool_state_label(call.state),
                    match call.state {
                        ToolCallState::Failed | ToolCallState::Denied => failure(),
                        ToolCallState::AwaitingApproval => warning(),
                        ToolCallState::Running => info(),
                        ToolCallState::Requested
                        | ToolCallState::Interrupted
                        | ToolCallState::Completed => muted(),
                    },
                )
            })
        }
    };
    let state_width = state.map_or(0, |(label, _)| label.len());
    let duration = context.clock.duration(running).map(format_duration_ms);
    let duration_width = duration.as_deref().map_or(0, |text| text.len() + 2);
    let mut show_duration = duration.is_some();
    let mut show_metric = metric.is_some();
    let right_width = |show_metric: bool, show_duration: bool| {
        let mut total = if show_metric { metric_width } else { 0 };
        if state.is_some() && !(exit_in_metric && show_metric) {
            total += if show_metric { 3 } else { 0 } + state_width;
        }
        if show_duration {
            total += duration_width;
        }
        total
    };
    let subject_min = if row.subject.is_some() { 6 } else { 0 };
    let fits = |show_metric: bool, show_duration: bool| {
        let right = right_width(show_metric, show_duration);
        let gap = if right > 0 { 2 } else { 0 };
        used + subject_min + gap + right <= width
    };
    if show_duration && !fits(show_metric, show_duration) {
        show_duration = false;
    }
    if show_metric && !fits(show_metric, show_duration) {
        show_metric = false;
    }
    let right_width = right_width(show_metric, show_duration);
    let gap = if right_width > 0 { 2 } else { 0 };
    if let Some(subject) = &row.subject {
        let available = width.saturating_sub(used + gap + right_width);
        let text = if row.subject_is_path {
            elide_path(subject, available)
        } else {
            preview(subject, available)
        };
        used += text_width(&text);
        line.push(text, normal());
    }
    if right_width > 0 {
        line.push(
            " ".repeat(width.saturating_sub(used + right_width).max(gap)),
            muted(),
        );
        if show_metric && let Some(metric) = metric {
            line.push(metric, muted());
            if row.truncated {
                line.push(TRUNCATED_SUFFIX, muted());
            }
        }
        if let Some((label, style)) = state.filter(|_| !(exit_in_metric && show_metric)) {
            if show_metric {
                line.push(" · ", muted());
            }
            line.push(label, style);
        }
        if show_duration && let Some(duration) = duration {
            let style = if running { info() } else { muted() };
            line.push("  ", style);
            line.push(duration, style);
        }
    }
    truncate_line(line, width)
}

pub(super) fn tool_state_glyph(call: &ToolCallSnapshot, tick: usize) -> (&'static str, Style) {
    match call.state {
        ToolCallState::Running => (spinner(tick), info()),
        ToolCallState::Requested => ("○", muted()),
        ToolCallState::Completed | ToolCallState::Failed => match call.error_kind() {
            None => ("●", muted()),
            Some(ToolErrorKind::Correction) => ("↻", muted()),
            Some(ToolErrorKind::Outcome) => ("!", warning()),
            Some(ToolErrorKind::Failure) => ("✕", failure()),
        },
        ToolCallState::Denied => ("✕", failure()),
        ToolCallState::AwaitingApproval => ("◇", warning()),
        ToolCallState::Interrupted => ("◌", muted()),
    }
}

fn tool_error_label(call: &ToolCallSnapshot, kind: ToolErrorKind) -> (&str, Style) {
    match kind {
        ToolErrorKind::Correction => ("corrected", muted()),
        ToolErrorKind::Outcome => {
            let result = call.result.as_deref().unwrap_or_default();
            let result = result
                .strip_prefix("edit ")
                .and_then(|rest| rest.split_once(": "))
                .filter(|(index, _)| {
                    !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit())
                })
                .map_or(result, |(_, rest)| rest);
            let result = result
                .strip_prefix("shell ")
                .or_else(|| result.strip_prefix("exec "))
                .unwrap_or(result);
            let fetch_status = result
                .strip_prefix("fetch ")
                .and_then(|rest| rest.lines().next())
                .and_then(|header| {
                    header
                        .split_whitespace()
                        .find(|token| token.starts_with("status="))
                });
            let code = fetch_status.unwrap_or_else(|| {
                result
                    .split([':', ' ', '\n'])
                    .next()
                    .filter(|code| !code.is_empty() && code.len() <= 24)
                    .unwrap_or("no result")
            });
            (code, warning())
        }
        ToolErrorKind::Failure => (tool_state_label(call.state), failure()),
    }
}

fn absorbed_correction(calls: &[&ToolCallSnapshot], index: usize) -> bool {
    let call = calls[index];
    call.error_kind() == Some(ToolErrorKind::Correction)
        && calls[index + 1..].iter().any(|later| {
            later.name == call.name
                && later.turn_ordinal > call.turn_ordinal
                && later.state == ToolCallState::Completed
                && !later.is_error
        })
}

pub(super) fn tool_error_lines(
    result: &str,
    max_rows: usize,
    max_bytes: usize,
    style: Style,
    width: usize,
) -> Vec<Line> {
    let text = bounded_tail(result, max_bytes);
    let total = text.lines().count();
    let content_width = tool_panel_content_width(width);
    let mut body = Vec::with_capacity(max_rows + 1);
    if total > max_rows || text.len() < result.len() {
        body.push(Line::styled("…", muted()));
    }
    for line in text.lines().skip(total.saturating_sub(max_rows)) {
        body.push(truncate_line(Line::styled(line, style), content_width));
    }
    if body.is_empty() {
        return body;
    }
    tool_panel_rows(body, None, width)
}

fn tool_panel_rows(
    body: impl IntoIterator<Item = Line>,
    label: Option<&str>,
    width: usize,
) -> Vec<Line> {
    let indent = TOOL_PANEL_INDENT.len();
    if width <= indent + CODE_PANEL_INSET {
        return panel_rows(body, label, width);
    }
    panel_rows_at(body, label, width - indent, indent)
}

fn tool_panel_content_width(width: usize) -> usize {
    panel_content_width(width.saturating_sub(TOOL_PANEL_INDENT.len()))
}

pub(crate) fn format_clock(ms: u64) -> String {
    let seconds = ms / 1000;
    format!(
        "{:02}:{:02}:{:02}",
        (seconds / 3600) % 24,
        (seconds / 60) % 60,
        seconds % 60
    )
}

pub(super) fn tool_expanded_lines(
    call: &ToolCallSnapshot,
    context: ToolRowContext<'_>,
    width: usize,
) -> Vec<Line> {
    let mut lines = Vec::new();
    let row = context.row;
    let graded = match call.error_kind() {
        Some(ToolErrorKind::Correction) => tool_error_lines(
            call.result.as_deref().unwrap_or_default(),
            MAX_TOOL_RESULT_ROWS,
            MAX_TOOL_DETAIL_BYTES,
            muted(),
            width,
        ),
        Some(ToolErrorKind::Outcome) => tool_error_lines(
            strip_header(
                call.result.as_deref().unwrap_or_default(),
                header_word(&call.name),
            ),
            MAX_TOOL_RESULT_ROWS,
            MAX_TOOL_DETAIL_BYTES,
            warning(),
            width,
        ),
        Some(ToolErrorKind::Failure) | None => Vec::new(),
    };
    let timing = context.clock.timing;
    let running = call.state == ToolCallState::Running;
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
    if let Some(label) = timing.settled.and_then(|settled| settled.label()) {
        fields.push(label);
    }
    if !fields.is_empty() {
        when.push(fields.join(" · "), muted());
        lines.push(truncate_line(when, width));
    }
    lines.extend(graded);
    let mut cached = row.panel.borrow_mut();
    if !cached
        .as_ref()
        .is_some_and(|(cached_width, _)| *cached_width == width)
    {
        *cached = Some((width, tool_panel(call, row, width)));
    }
    if let Some((_, panel)) = cached.as_ref() {
        lines.extend_from_slice(panel);
    }
    lines
}

fn tool_panel(call: &ToolCallSnapshot, row: &ToolRow, width: usize) -> Vec<Line> {
    let content_width = tool_panel_content_width(width);
    let mut body: Vec<Line> = Vec::new();
    if row.raw_name {
        for (shown, (key, value)) in row.arguments.iter().enumerate() {
            if shown == MAX_TOOL_ARGUMENT_ROWS {
                body.push(Line::styled("…", muted()));
                break;
            }
            let mut line = Line::styled(format!("{key}: "), muted());
            line.push(
                preview(value, content_width.saturating_sub(line.width())),
                normal(),
            );
            body.push(truncate_line(line, content_width));
        }
    }
    let result = if let Some(diff) = &row.diff {
        if !body.is_empty() {
            body.push(Line::default());
        }
        body.extend(diff_lines(diff, MAX_TOOL_RESULT_ROWS, content_width));
        None
    } else {
        call.result.as_deref().filter(|_| !call.is_error)
    };
    if let Some(result) = result {
        let result = strip_header(result, header_word(&call.name));
        if !body.is_empty() && !result.is_empty() {
            body.push(Line::default());
        }
        match row.body {
            ResultBody::Head => {
                let total = result.lines().count();
                for line in result
                    .lines()
                    .filter(|line| !is_marker_line(line))
                    .take(MAX_TOOL_RESULT_ROWS)
                {
                    body.push(Line::styled(preview(line, content_width), muted()));
                }
                if total > MAX_TOOL_RESULT_ROWS {
                    body.push(Line::styled(
                        format!(
                            "… {} more",
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
                    body.push(Line::styled("…", muted()));
                }
                for line in text
                    .lines()
                    .skip(total.saturating_sub(MAX_TOOL_RESULT_ROWS))
                {
                    body.push(Line::styled(preview(line, content_width), muted()));
                }
            }
        }
    }
    if body.is_empty() {
        return Vec::new();
    }
    tool_panel_rows(body, None, width)
}

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

pub(super) fn tool_live_output_lines(output: &str, width: usize) -> Vec<Line> {
    let complete = &output[..output.rfind('\n').map_or(0, |index| index + 1)];
    if complete.is_empty() {
        return Vec::new();
    }
    let total = complete.lines().count();
    let mut rows = tool_panel_rows(
        complete
            .lines()
            .skip(total.saturating_sub(MAX_LIVE_TAIL_ROWS))
            .map(|line| {
                let safe = line
                    .chars()
                    .filter_map(terminal_safe_character)
                    .collect::<String>();
                Line::styled(safe, muted())
            }),
        None,
        width,
    );
    let excess = (rows.len() - TOOL_PANEL_PADDING_ROWS).saturating_sub(MAX_LIVE_TAIL_ROWS);
    if excess > 0 {
        rows.drain(1..1 + excess);
    }
    rows
}

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

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SimpleDetail {
    Rows,
    Folded,
    Expanded,
}

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
        inline_detail: true,
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
