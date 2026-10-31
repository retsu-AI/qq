use std::cell::RefCell;

use super::*;
use qq_client::state::ToolCallTiming;
use qq_protocol::ToolErrorKind;

pub(super) const TOOL_FOLD_THRESHOLD: usize = 3;
pub(super) const TOOL_MARKER_PREFIX: &str = "…[qq: ";
pub(super) const TOOL_SUBJECT_WIDTH: usize = 48;
pub(super) const MAX_TOOL_ERROR_BYTES: usize = 2 * 1024;
pub(super) const MAX_TOOL_ERROR_ROWS: usize = 3;
pub(super) const MAX_TOOL_DETAIL_BYTES: usize = 4 * 1024;
pub(super) const MAX_TOOL_ARGUMENT_ROWS: usize = 8;
pub(super) const MAX_TOOL_RESULT_ROWS: usize = 3;
pub(super) const MAX_LIVE_TAIL_ROWS: usize = 3;
pub(super) const TOOL_SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];
const TRUNCATED_SUFFIX: &str = " · truncated";

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

pub(super) type RowLookup<'a> = &'a dyn Fn(&ToolCallSnapshot) -> ToolRowContext<'a>;

pub(super) fn render_tool_calls(
    calls: &[&ToolCallSnapshot],
    live_output: &HashMap<ToolCallId, String>,
    lookup: RowLookup<'_>,
    tick: usize,
    width: usize,
    children: ChildRows<'_>,
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
            && !nonzero_exit(call)
            && !lookup(call).expanded
            && !lookup(call).selected
            && children(call.id, width).is_empty()
    };
    let fold = calls.first().is_some_and(|call| lookup(call).fold);
    let settled = calls.iter().all(|call| {
        !matches!(
            call.state,
            ToolCallState::Running | ToolCallState::Requested | ToolCallState::AwaitingApproval
        )
    });
    let mut lines = Vec::with_capacity(calls.len());
    let mut folded: Vec<&ToolCallSnapshot> = Vec::new();
    if fold && settled {
        let routine: Vec<&ToolCallSnapshot> = calls
            .iter()
            .copied()
            .filter(|call| quiet(call) && !is_edit(call))
            .collect();
        if routine.len() >= TOOL_FOLD_THRESHOLD.min(calls.len()) && !routine.is_empty() {
            lines.push(tool_fold_line(&routine, calls, lookup, width));
            folded = routine;
        }
    }
    for call in calls {
        if folded.iter().any(|folded| folded.id == call.id) {
            continue;
        }
        let context = lookup(call);
        lines.push(tool_summary_line(call, context, tick, width));
        if let Some(result) = call.result.as_deref() {
            let panel = match (call.error_kind(), call.is_error) {
                (Some(ToolErrorKind::Failure), _) | (None, true) => {
                    Some((MAX_TOOL_ERROR_ROWS, failure()))
                }
                (Some(ToolErrorKind::Outcome), _) if context.expanded && context.inline_detail => {
                    None
                }
                (Some(ToolErrorKind::Outcome), _) => Some((1, warning())),
                (None, false) if nonzero_exit(call) && !context.expanded => {
                    Some((MAX_TOOL_ERROR_ROWS, muted()))
                }
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

fn is_edit(call: &ToolCallSnapshot) -> bool {
    matches!(call.name.as_str(), "edit_file" | "write_file")
}

pub(super) fn tool_fold_line(
    folded: &[&ToolCallSnapshot],
    turn: &[&ToolCallSnapshot],
    lookup: RowLookup<'_>,
    width: usize,
) -> Line {
    let mut counts: Vec<(&'static str, usize)> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut added = 0;
    let mut removed = 0;
    let mut started: Option<u64> = None;
    let mut finished: Option<u64> = None;
    for call in turn {
        let context = lookup(call);
        let row = context.row;
        let timing = context.clock.timing;
        if let Some(start) = timing.started_at_ms {
            started = Some(started.map_or(start, |current| current.min(start)));
        }
        if let Some(end) = timing.finished_at_ms {
            finished = Some(finished.map_or(end, |current| current.max(end)));
        }
        if !folded.iter().any(|candidate| candidate.id == call.id) {
            if is_edit(call) {
                if let Some(subject) = &row.subject
                    && !files.contains(subject)
                {
                    files.push(subject.clone());
                }
                if let Some((plus, minus)) = row.metric.as_deref().and_then(|metric| {
                    let (plus, minus) = metric.split_once(' ')?;
                    Some((
                        plus.strip_prefix('+')?.parse::<usize>().ok()?,
                        minus.strip_prefix('−')?.parse::<usize>().ok()?,
                    ))
                }) {
                    added += plus;
                    removed += minus;
                }
            }
            continue;
        }
        let noun = match row.verb {
            "Read" => "read",
            "List" => "listed",
            "Search" => "searched",
            "Edit" | "Write" => "edited",
            "Run" => "ran",
            "Spawn" => "spawned",
            "Fetch" => "fetched",
            _ => "called",
        };
        match counts.iter_mut().find(|(name, _)| *name == noun) {
            Some((_, count)) => *count += 1,
            None => counts.push((noun, 1)),
        }
    }
    let mut summary = count_noun(folded.len(), "call", "calls");
    for (name, count) in counts {
        summary.push_str(&format!(" · {name} {count}"));
    }
    let mut line = Line::styled("● ", faint());
    line.push(summary, muted());
    if !files.is_empty() {
        let mut changed = count_noun(files.len(), "file", "files");
        if added + removed > 0 {
            changed.push_str(&format!(" +{added} −{removed}"));
        }
        line.push(format!("   {changed}"), faint());
    }
    if let (Some(start), Some(end)) = (started, finished)
        && end.saturating_sub(start) >= 1000
    {
        line.push(format!("   {}", format_duration_ms(end - start)), faint());
    }
    truncate_line(line, width)
}

pub(super) const TOOL_VERB_WIDTH: usize = 9;

fn verb_for(row: &ToolRow, call: &ToolCallSnapshot) -> String {
    if row.raw_name {
        return row.label(call);
    }
    let running = matches!(
        call.state,
        ToolCallState::Running | ToolCallState::Requested
    );
    let waiting = call.state == ToolCallState::AwaitingApproval;
    let (past, present) = match row.verb {
        "Read" => ("Read", "Reading"),
        "List" => ("Listed", "Listing"),
        "Search" => ("Searched", "Searching"),
        "Edit" => ("Edited", "Editing"),
        "Write" => ("Wrote", "Writing"),
        "Run" => ("Ran", "Running"),
        "Spawn" => ("Spawned", "Spawning"),
        "Recall" => ("Recalled", "Recalling"),
        "Fetch" => ("Fetched", "Fetching"),
        other => (other, other),
    };
    if waiting {
        row.verb.to_owned()
    } else if running {
        present.to_owned()
    } else {
        past.to_owned()
    }
}

pub(super) fn tool_summary_line(
    call: &ToolCallSnapshot,
    context: ToolRowContext<'_>,
    tick: usize,
    width: usize,
) -> Line {
    let row = context.row;
    let (glyph, glyph_style) = tool_state_glyph(call, tick);
    let mut line = Line::styled(format!("{glyph} "), glyph_style);
    if context.selected {
        line = Line::styled(format!("{glyph} "), accent());
    }
    let verb = verb_for(row, call);
    let verb_width = text_width(&verb).max(TOOL_VERB_WIDTH - 1) + 1;
    line.push(
        format!("{verb:<verb_width$}"),
        if context.selected {
            normal().bold()
        } else {
            normal()
        },
    );
    let running = call.state == ToolCallState::Running;
    let mut tail: Vec<(String, Style)> = Vec::new();
    match call.error_kind() {
        Some(kind @ (ToolErrorKind::Correction | ToolErrorKind::Outcome)) => {
            let exit = call
                .result
                .as_deref()
                .and_then(|result| header_field(result, &call.name, "exit"));
            match (kind, exit) {
                (ToolErrorKind::Outcome, Some(code)) => {
                    tail.push((format!("exit {code}"), failure()));
                }
                _ => {
                    let (label, style) = tool_error_label(call, kind);
                    tail.push((label.to_owned(), style));
                }
            }
        }
        Some(ToolErrorKind::Failure) | None => {
            if let Some(metric) = row.metric.as_deref().filter(|metric| !metric.is_empty()) {
                let mut metric = metric.to_owned();
                if row.truncated {
                    metric.push_str(TRUNCATED_SUFFIX);
                }
                let style = if nonzero_exit(call) {
                    failure()
                } else {
                    faint()
                };
                tail.push((metric, style));
            }
            match call.state {
                ToolCallState::Completed | ToolCallState::Running => {}
                ToolCallState::AwaitingApproval => tail.push(("waiting".to_owned(), warning())),
                ToolCallState::Failed | ToolCallState::Denied => {
                    tail.push((tool_state_label(call.state).to_owned(), failure()));
                }
                ToolCallState::Requested | ToolCallState::Interrupted => {
                    tail.push((tool_state_label(call.state).to_owned(), faint()));
                }
            }
        }
    }
    if let Some(duration) = context.clock.duration(running).filter(|ms| *ms >= 1000) {
        tail.push((format_duration_ms(duration), faint()));
    }
    let used = line.width();
    let mut tail_width: usize = tail.iter().map(|(text, _)| text_width(text) + 2).sum();
    while used + 6 + tail_width > width {
        let Some(index) = tail.iter().rposition(|(_, style)| *style != failure()) else {
            break;
        };
        let (text, _) = tail.remove(index);
        tail_width -= text_width(&text) + 2;
    }
    if let Some(subject) = &row.subject {
        let available = width.saturating_sub(used + tail_width);
        let text = if row.subject_is_path {
            elide_path(subject, available)
        } else {
            preview(subject, available)
        };
        line.push(text, muted());
    }
    for (text, style) in tail {
        line.push("  ", normal());
        line.push(text, style);
    }
    truncate_line(line, width)
}

pub(super) fn nonzero_exit(call: &ToolCallSnapshot) -> bool {
    matches!(call.name.as_str(), "shell" | "exec")
        && call
            .result
            .as_deref()
            .and_then(|result| header_field(result, &call.name, "exit"))
            .is_some_and(|code| code != "0")
}

pub(super) fn tool_state_glyph(call: &ToolCallSnapshot, tick: usize) -> (&'static str, Style) {
    if call.state == ToolCallState::Completed && nonzero_exit(call) {
        return ("✕", failure());
    }
    match call.state {
        ToolCallState::Running => (spinner(tick), info()),
        ToolCallState::Requested => ("○", faint()),
        ToolCallState::Completed | ToolCallState::Failed => match call.error_kind() {
            None => ("●", faint()),
            Some(ToolErrorKind::Correction) => ("↻", faint()),
            Some(ToolErrorKind::Outcome | ToolErrorKind::Failure) => ("✕", failure()),
        },
        ToolCallState::Denied => ("✕", failure()),
        ToolCallState::AwaitingApproval => ("◇", warning()),
        ToolCallState::Interrupted => ("◌", faint()),
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
    let rows: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let shown = rows.len().min(max_rows);
    let mut body = Vec::with_capacity(shown + 1);
    if rows.len() > shown || text.len() < result.len() {
        body.push(tail_row("…", faint(), width));
    }
    for line in &rows[rows.len() - shown..] {
        body.push(tail_row(line, style, width));
    }
    body
}

pub(super) fn tail_row(text: &str, style: Style, width: usize) -> Line {
    let safe: String = text.chars().filter_map(terminal_safe_character).collect();
    let mut line = Line::styled("│ ", faint());
    line.push(safe, style);
    truncate_line(line, width)
}

fn tool_panel_content_width(width: usize) -> usize {
    width.saturating_sub(2).max(1)
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
        body.extend(super::overlay::dock_diff_lines(
            diff,
            MAX_TOOL_RESULT_ROWS,
            content_width,
        ));
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
                            "… {}",
                            count_noun(total - MAX_TOOL_RESULT_ROWS, "more line", "more lines")
                        ),
                        faint(),
                    ));
                }
            }
            ResultBody::Tail => {
                let text = bounded_tail(result, MAX_TOOL_DETAIL_BYTES);
                let total = text.lines().count();
                if total > MAX_TOOL_RESULT_ROWS || text.len() < result.len() {
                    body.push(Line::styled("…", faint()));
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
    body.into_iter()
        .map(|line| {
            let mut row = Line::styled("│ ", faint());
            for span in line.spans {
                row.push(span.text, span.style);
            }
            truncate_line(row, width)
        })
        .collect()
}

pub(super) fn tool_live_output_lines(output: &str, width: usize) -> Vec<Line> {
    let complete = &output[..output.rfind('\n').map_or(0, |index| index + 1)];
    let rows: Vec<&str> = complete.lines().collect();
    rows[rows.len().saturating_sub(MAX_LIVE_TAIL_ROWS)..]
        .iter()
        .map(|line| tail_row(line, muted(), width))
        .collect()
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
    render_tool_calls(calls, live_output, &lookup, tick, width, children)
}
