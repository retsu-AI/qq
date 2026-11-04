use super::*;
use crate::{
    commands::Category,
    input::{
        ApprovalModeRow, CommandRow, EffortRow, ModelRow, Overlay, ProfileRow, SessionRow,
        SkillRow, ThemeRow,
    },
    picker::{Picker, PickerItem},
};

struct PickerHeader<'a> {
    title: &'a str,
    hint: &'a str,
    placeholder: &'a str,
}

struct PickerChrome<'a> {
    header: Option<PickerHeader<'a>>,
    question: Option<Line>,
    empty: &'a str,
    windowed: bool,
}

fn picker_frame<T: PickerItem>(
    picker: &Picker<T>,
    chrome: PickerChrome<'_>,
    width: usize,
    height: usize,
    mut row: impl FnMut(&T, bool, &mut Vec<Line>),
) -> Vec<Line> {
    let PickerChrome {
        header,
        question,
        empty,
        windowed,
    } = chrome;
    let mut lines = Vec::with_capacity(height);
    let header_rows = if header.is_some() { 2 } else { 0 };
    let chrome_rows = header_rows + usize::from(question.is_some());
    if let Some(PickerHeader {
        title,
        hint,
        placeholder,
    }) = header
    {
        if height > chrome_rows {
            lines.push(section(title, hint));
        }
        lines.push(search_line(&picker.query, placeholder));
    }
    if let Some(question) = question {
        lines.push(truncate_line(question, width));
    }
    if header_rows > 0 && height > chrome_rows + PICKER_SPACER_MIN_RESULTS {
        lines.push(Line::default());
    }
    if picker.filtered().len() == 0 {
        lines.push(Line::styled(empty, muted().italic()));
        return fit_height(lines, height);
    }
    let cursor = picker.cursor();
    let mut results = Vec::with_capacity(picker.filtered().len());
    let mut selected_row = 0;
    let (first, last) = if windowed {
        (cursor.saturating_sub(height), cursor.saturating_add(height))
    } else {
        (0, usize::MAX)
    };
    for (position, (_, item)) in picker.filtered().enumerate() {
        if position < first || position > last {
            continue;
        }
        let selected = position == cursor;
        if selected {
            selected_row = results.len();
        }
        row(item, selected, &mut results);
    }
    lines.extend(selection_viewport(
        results,
        height.saturating_sub(lines.len()),
        selected_row,
    ));
    fit_height(lines, height)
}

const PICKER_SPACER_MIN_RESULTS: usize = 3;

pub(super) fn search_line(query: &str, placeholder: &str) -> Line {
    Line::styled(
        format!(
            "  search: {}",
            if query.is_empty() { placeholder } else { query }
        ),
        if query.is_empty() { muted() } else { accent() },
    )
}

fn cursor_prefix(selected: bool) -> Line {
    Line::styled(if selected { "  > " } else { "    " }, muted())
}

fn finish_row(mut line: Line, selected: bool, width: usize) -> Line {
    line = truncate_line(line, width);
    if selected {
        pad_line(&mut line, width);
        for span in &mut line.spans {
            span.style = selection(span.style);
        }
    }
    line
}

const SWITCHER_PREVIEW_ROWS: usize = 3;

fn session_age(now_ms: u64, updated_at_ms: u64) -> String {
    let seconds = now_ms.saturating_sub(updated_at_ms) / 1000;
    match seconds {
        0..=9 => "now".to_owned(),
        10..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

fn last_line(text: &str) -> &str {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
}

pub(super) fn session_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
    let Some(Overlay::Sessions {
        picker, confirm, ..
    }) = &app.overlay
    else {
        return fit_height(Vec::new(), height);
    };
    let question = confirm.map(|confirm| {
        let text = match confirm {
            SessionConfirm::Delete(session_id) => {
                let title = app
                    .sessions
                    .get(&session_id)
                    .map_or("this session", |session| session.summary.title.as_str());
                format!("  ◇ delete '{title}'? y deletes, n keeps")
            }
            SessionConfirm::Prune => {
                "  ◇ delete every empty session in this workspace? y deletes, n keeps".to_owned()
            }
        };
        Line::styled(text, warning().bold())
    });
    let create = app
        .chord_label(crate::commands::Command::NewRootSession)
        .unwrap_or_else(|| "/new".to_owned());
    let empty = if app.sessions.is_empty() {
        format!("  {create} creates a root session.")
    } else {
        "  No matching sessions.".to_owned()
    };
    let title_width = picker
        .filtered()
        .map(|(_, row)| text_width(&row.title) + if row.is_agent() { 4 } else { 0 })
        .max()
        .unwrap_or(0)
        .clamp(12, 28);
    picker_frame(
        picker,
        PickerChrome {
            header: None,
            question,
            empty: &empty,
            windowed: true,
        },
        width,
        height,
        |row: &SessionRow, selected, out| {
            let (glyph, glyph_style) = rail_glyph(app, row.id);
            let mut line = Line::styled(if selected { "  ▎ " } else { "    " }, accent());
            line.push(format!("{glyph} "), glyph_style);
            let title = if row.is_agent() {
                format!(
                    "{} › {}",
                    app.sessions[&row.session].summary.title, row.title
                )
            } else {
                row.title.clone()
            };
            let used = line.width();
            line.push(
                format!("{:<title_width$}", fit_text(&title, title_width + 8)),
                if selected { normal() } else { muted() },
            );
            let badge = if row.agents > 0 {
                format!("  ↳{}", row.agents)
            } else {
                String::new()
            };
            line.push(format!("{badge:<5}"), faint());
            line.push(
                format!("{:>4}   ", session_age(app.now_ms, row.updated_at_ms)),
                faint(),
            );
            let room = width.saturating_sub(line.width());
            let live = live_status_line(app, row.id).map(|(text, _)| text);
            let tail = live.unwrap_or_else(|| last_line(&row.answer).to_owned());
            let _ = used;
            line.push(fit_text(&tail, room), muted());
            out.push(finish_row(line, selected, width));
            if selected && !row.answer.is_empty() {
                let preview = row
                    .answer
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .take(SWITCHER_PREVIEW_ROWS);
                for text in preview {
                    out.push(truncate_line(
                        Line::styled(
                            format!("        {}", fit_text(text, width.saturating_sub(10))),
                            faint(),
                        ),
                        width,
                    ));
                }
            }
        },
    )
}

pub(super) fn switcher_rule(app: &App, width: usize) -> Option<Line> {
    let Some(Overlay::Sessions { picker, scope, .. }) = &app.overlay else {
        return None;
    };
    let mut left = Line::styled("○ ", faint());
    left.push(
        if scope.is_some() {
            "agents "
        } else {
            "switch "
        },
        normal(),
    );
    let total = picker.items().len();
    let shown = picker.filtered().len();
    let count = if picker.query.is_empty() {
        let agents = scope.is_some();
        let listed = picker
            .items()
            .iter()
            .filter(|row| row.is_agent() == agents)
            .count();
        if agents {
            count_noun(listed, "agent", "agents")
        } else {
            count_noun(listed, "session", "sessions")
        }
    } else {
        format!("{shown} of {total}")
    };
    let right = format!(" {count}");
    let mut rule = truncate_line(left, width.saturating_sub(text_width(&right) + 1));
    let fill = width.saturating_sub(rule.width() + text_width(&right));
    rule.push("─".repeat(fill), faint());
    rule.push(right, faint());
    Some(truncate_line(rule, width))
}

pub(super) fn switcher_query(app: &App, width: usize) -> Option<(Line, usize)> {
    let Some(Overlay::Sessions { picker, .. }) = &app.overlay else {
        return None;
    };
    let mut line = Line::styled("┃ ", accent());
    if picker.query.is_empty() {
        line.push("type to search sessions…", faint());
        return Some((truncate_line(line, width), 2));
    }
    line.push(picker.query.clone(), normal());
    let column = (2 + text_width(&picker.query)).min(width.saturating_sub(1));
    Some((truncate_line(line, width), column))
}

pub(super) fn model_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
    let Some(Overlay::Models(picker)) = &app.overlay else {
        return fit_height(Vec::new(), height);
    };
    let unauthenticated_only = picker.items().iter().all(|row| row.index.is_none());
    let mut provider: Option<String> = None;
    picker_frame(
        picker,
        PickerChrome {
            header: Some(PickerHeader {
                title: "MODELS",
                hint: if unauthenticated_only {
                    "no provider has a credential yet; Esc closes"
                } else if app.focused().is_some() {
                    "type to search, Enter sets the session model, Ctrl-N creates a session, Esc closes"
                } else {
                    "type to search, Up/Down select, Enter creates session, Esc closes"
                },
                placeholder: "all models",
            }),
            question: None,
            empty: "  No matching models.",
            windowed: false,
        },
        width,
        height,
        |row: &ModelRow, selected, out| {
            if row.index.is_none() {
                let mut line = cursor_prefix(selected);
                line.push(row.provider.clone(), muted());
                line.push("  needs credential", warning());
                line.push(format!("  {}", row.model), muted());
                out.push(finish_row(line, selected, width));
                return;
            }
            if provider.as_deref() != Some(row.provider.as_str()) {
                provider = Some(row.provider.clone());
                out.push(Line::styled(
                    format!("  {}", row.provider.to_ascii_uppercase()),
                    accent().bold(),
                ));
            }
            let mut line = cursor_prefix(selected);
            line.push(
                row.name.as_deref().unwrap_or(&row.model),
                if selected { normal().bold() } else { normal() },
            );
            if row.name.as_deref() != Some(row.model.as_str()) {
                line.push(format!("  {}", row.model), muted());
            }
            out.push(finish_row(line, selected, width));
        },
    )
}

pub(super) fn profile_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
    let Some(Overlay::Profiles(picker)) = &app.overlay else {
        return fit_height(Vec::new(), height);
    };
    let focused = app
        .focused()
        .and_then(|session_id| app.sessions.get(&session_id));
    let current = focused.map_or(&app.profile, |session| &session.summary.profile);
    picker_frame(
        picker,
        PickerChrome {
            header: Some(PickerHeader {
                title: "PROFILES",
                hint: if focused.is_some() {
                    "type to search, Enter sets the session profile, Esc closes"
                } else {
                    "type to search, Enter sets the profile for new sessions, Esc closes"
                },
                placeholder: "all profiles",
            }),
            question: None,
            empty: "  No matching profiles.",
            windowed: false,
        },
        width,
        height,
        |row: &ProfileRow, selected, out| {
            let mut line = cursor_prefix(selected);
            line.push(
                format!("{:<16}", row.id.as_str()),
                if selected { normal().bold() } else { normal() },
            );
            line.push(
                format!("{:<10}", approval_mode_label(row.approval_mode)),
                muted(),
            );
            if let Some(model) = &row.model {
                line.push(format!("  {model}"), muted());
            }
            if row.id == *current {
                line.push("  active", accent());
            }
            out.push(finish_row(line, selected, width));
        },
    )
}

pub(super) fn approval_mode_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
    let Some(Overlay::ApprovalModes(picker)) = &app.overlay else {
        return fit_height(Vec::new(), height);
    };
    let current = app.effective_approval_mode();
    picker_frame(
        picker,
        PickerChrome {
            header: Some(PickerHeader {
                title: "APPROVAL MODE",
                hint: if app.focused().is_some() {
                    "type to search, Enter sets the session's mode, Esc closes"
                } else {
                    "type to search, Enter sets the mode for new sessions, Esc closes"
                },
                placeholder: "all modes",
            }),
            question: None,
            empty: "  No matching modes.",
            windowed: false,
        },
        width,
        height,
        |row: &ApprovalModeRow, selected, out| {
            let mut line = cursor_prefix(selected);
            line.push(
                format!("{:<11}", row.label),
                if selected { normal().bold() } else { normal() },
            );
            line.push(row.summary, muted());
            if row.mode == current {
                line.push("  active", accent());
            }
            out.push(finish_row(line, selected, width));
        },
    )
}

pub(super) fn effort_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
    let Some(Overlay::Effort(picker)) = &app.overlay else {
        return fit_height(Vec::new(), height);
    };
    let current = app.effective_effort();
    let advertised = !app.focused_model_efforts().is_empty();
    picker_frame(
        picker,
        PickerChrome {
            header: Some(PickerHeader {
                title: "EFFORT",
                hint: match (app.focused().is_some(), advertised) {
                    (true, true) => {
                        "levels this model advertises; Enter sets the session's effort, Esc closes"
                    }
                    (true, false) => {
                        "model advertises no ladder, showing every level; Enter sets the session's effort, Esc closes"
                    }
                    (false, true) => {
                        "levels the default model advertises; Enter sets the effort for new sessions, Esc closes"
                    }
                    (false, false) => {
                        "type to search, Enter sets the effort for new sessions, Esc closes"
                    }
                },
                placeholder: if advertised {
                    "advertised effort levels"
                } else {
                    "all effort levels"
                },
            }),
            question: None,
            empty: "  No matching effort levels.",
            windowed: false,
        },
        width,
        height,
        |row: &EffortRow, selected, out| {
            let mut line = cursor_prefix(selected);
            line.push(
                format!("{:<10}", row.label),
                if selected { normal().bold() } else { normal() },
            );
            line.push(row.summary, muted());
            if row.effort == current {
                line.push("  active", accent());
            }
            out.push(finish_row(line, selected, width));
        },
    )
}

pub(super) fn skill_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
    let Some(Overlay::Skills(picker)) = &app.overlay else {
        return fit_height(Vec::new(), height);
    };
    let mut kind: Option<qq_protocol::GuidanceKind> = None;
    let host_warning = app.tool_host_warning().map(|message| {
        let mut line = Line::styled("  MCP: ", warning());
        line.push(message, muted());
        line
    });
    picker_frame(
        picker,
        PickerChrome {
            header: Some(PickerHeader {
                title: "SKILLS",
                hint: "type to search, Enter inserts a command or runs a skill, Esc closes",
                placeholder: "all commands and skills",
            }),
            question: host_warning,
            empty: "  No matching commands or skills.",
            windowed: false,
        },
        width,
        height,
        |row: &SkillRow, selected, out| {
            if kind != Some(row.kind) {
                kind = Some(row.kind);
                out.push(Line::styled(
                    match row.kind {
                        qq_protocol::GuidanceKind::Command => "  COMMANDS",
                        qq_protocol::GuidanceKind::Skill => "  SKILLS",
                    },
                    accent().bold(),
                ));
            }
            let mut line = cursor_prefix(selected);
            line.push(
                format!("/{:<22}", row.name),
                if selected { normal().bold() } else { normal() },
            );
            if !row.description.is_empty() {
                line.push(row.description.as_str(), muted());
            }
            if !row.disclosed {
                line.push("  explicit only", warning());
            }
            out.push(finish_row(line, selected, width));
            let mut source = Line::styled("      ", muted());
            source.push(row.source.as_str(), muted().italic());
            out.push(finish_row(source, false, width));
        },
    )
}

pub(super) fn theme_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
    let Some(Overlay::Themes { picker, .. }) = &app.overlay else {
        return fit_height(Vec::new(), height);
    };
    picker_frame(
        picker,
        PickerChrome {
            header: Some(PickerHeader {
                title: "THEMES",
                hint: "Up/Down preview live, Enter keeps, Esc restores the previous theme",
                placeholder: "all themes",
            }),
            question: None,
            empty: "  No matching themes.",
            windowed: false,
        },
        width,
        height,
        |row: &ThemeRow, selected, out| {
            let theme = &app.themes[row.index];
            let mut line = cursor_prefix(selected);
            line.push(
                format!("{:<18}", theme.name),
                if selected { normal().bold() } else { normal() },
            );
            let palette = theme.palette;
            for color in [
                palette.text,
                palette.muted,
                palette.accent,
                palette.brand,
                palette.warning,
                palette.error,
                palette.success,
            ] {
                line.push("██", Style::color(color).on(palette.surface));
            }
            if row.index == app.theme {
                line.push("  active", accent());
            }
            out.push(finish_row(line, selected, width));
        },
    )
}

pub(super) fn command_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
    let Some((picker, help)) = app.command_picker() else {
        return fit_height(Vec::new(), height);
    };
    let mut category: Option<Category> = None;
    let chord_column = 14;
    picker_frame(
        picker,
        PickerChrome {
            header: Some(PickerHeader {
                title: if help { "HELP" } else { "COMMANDS" },
                hint: if help {
                    "every key and command; type to filter, Enter runs, Esc closes"
                } else {
                    "type to search, Enter runs the command, Esc closes"
                },
                placeholder: "all commands",
            }),
            question: None,
            empty: "  No matching commands.",
            windowed: false,
        },
        width,
        height,
        |row: &CommandRow, selected, out| {
            if help && category != Some(row.spec.category) {
                if category.is_some() {
                    out.push(Line::default());
                }
                category = Some(row.spec.category);
                out.push(Line::styled(
                    format!("  {}", row.spec.category.label()),
                    accent().bold(),
                ));
            }
            let mut line = cursor_prefix(selected);
            let chord = app.chord_label(row.spec.command).unwrap_or_default();
            line.push(
                format!("{chord:<chord_column$}"),
                if chord.is_empty() { muted() } else { accent() },
            );
            line.push(
                row.spec.title,
                if selected { normal().bold() } else { normal() },
            );
            if let Some(slash) = row.spec.slash.first() {
                line.push(format!("  {slash}"), muted());
            }
            out.push(finish_row(line, selected, width));
        },
    )
}

pub(super) fn approval_block(app: &App, width: usize, limit: usize) -> Vec<Line> {
    let mut lines = Vec::new();
    if let Some(question) = app.pending_question() {
        let answered = app.question_answers.len();
        let current = question.questions.get(answered);
        let current_rows = current.map_or(0, |item| 2 + item.options.len());
        let heading_rows = usize::from(question.questions.len() > 1);
        let history_room = limit.saturating_sub(heading_rows + current_rows);
        let shown_history = answered.min(history_room);
        if question.questions.len() > 1 {
            let mut heading = format!(
                "  question {}/{}",
                (answered + 1).min(question.questions.len()),
                question.questions.len()
            );
            if shown_history < answered {
                heading.push_str(&format!(" · {} answered", answered - shown_history));
            }
            lines.push(Line::styled(heading, faint()));
        }
        for (index, item) in question.questions.iter().enumerate() {
            if index < answered {
                if index < answered - shown_history {
                    continue;
                }
                let mut line = Line::styled("  ", normal());
                line.push(item.prompt.as_str(), muted());
                line.push(" → ", faint());
                line.push(app.question_answers[index].as_str(), normal());
                lines.push(truncate_line(line, width));
                continue;
            }
            if index > answered {
                break;
            }
            let mut line = Line::styled("  ", normal());
            line.push(item.prompt.as_str(), normal().bold());
            lines.push(truncate_line(line, width));
            let option_room = limit.saturating_sub(lines.len() + 1).max(1);
            let options: Vec<String> = item
                .options
                .iter()
                .enumerate()
                .map(|(option_index, option)| format!("{} {option}", option_index + 1))
                .collect();
            if options.len() <= option_room {
                for option in &options {
                    let (number, label) = option.split_once(' ').unwrap_or((option, ""));
                    let mut line = Line::styled(format!("  {number} "), normal());
                    line.push(label, muted());
                    lines.push(truncate_line(line, width));
                }
            } else {
                let per_row = options.len().div_ceil(option_room);
                for chunk in options.chunks(per_row) {
                    let mut line = Line::styled("  ", normal());
                    for (position, option) in chunk.iter().enumerate() {
                        if position > 0 {
                            line.push("   ", normal());
                        }
                        let (number, label) = option.split_once(' ').unwrap_or((option, ""));
                        line.push(format!("{number} "), normal());
                        line.push(label, muted());
                    }
                    lines.push(truncate_line(line, width));
                }
            }
            let mut hint = Line::styled("  ", normal());
            hint.push(
                if item.free_text && !item.options.is_empty() {
                    "type an answer or a number"
                } else if item.free_text {
                    "type an answer"
                } else {
                    "press a number"
                },
                faint(),
            );
            hint.push("   esc", normal());
            hint.push(" decline", faint());
            lines.push(truncate_line(hint, width));
        }
        return lines;
    }
    let preview = app.pending_approval_preview();
    if let Some(shell) = preview.and_then(|preview| preview.shell.as_ref()) {
        let mut line = Line::styled("  $ ", faint());
        line.push(shell.command.as_str(), normal());
        if let Some(cwd) = &shell.cwd {
            line.push(format!("  in {cwd}"), faint());
        }
        lines.push(truncate_line(line, width));
    }
    if let Some(fetch) = preview.and_then(|preview| preview.fetch.as_ref()) {
        let mut line = Line::styled("  ", normal());
        line.push(fetch.method.as_deref().unwrap_or("GET"), faint());
        line.push(" ", normal());
        line.push(fetch.url.as_str(), normal());
        lines.push(truncate_line(line, width));
    }
    if let Some(edit) = preview.and_then(|preview| preview.edit.as_ref()) {
        match app.approval_diff_start() {
            Some(start) => return full_diff_window(&edit.diff, start, width, limit),
            None => lines.extend(dock_diff_lines(&edit.diff, MAX_APPROVAL_DIFF_ROWS, width)),
        }
    }
    if lines.is_empty()
        && let Some(call) = app.pending_approval()
    {
        let row = ToolRow::derive(call);
        let mut line = Line::styled("  ", normal());
        line.push(row.verb, normal());
        if let Some(subject) = row.subject {
            line.push(format!("  {subject}"), muted());
        }
        lines.push(truncate_line(line, width));
    }
    lines
}

struct DiffRow {
    number: Option<usize>,
    sign: char,
    text: String,
    changed: Option<Range<usize>>,
}

fn diff_rows(diff: &str) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    let mut old_line: Option<usize> = None;
    let mut new_line: Option<usize> = None;
    for text in diff
        .lines()
        .filter(|line| !line.starts_with("+++") && !line.starts_with("---"))
    {
        if let Some(header) = text.strip_prefix("@@") {
            let mut ranges = header.split_whitespace();
            let parse = |range: Option<&str>, sign: char| {
                range
                    .and_then(|range| range.strip_prefix(sign))
                    .and_then(|range| range.split(',').next())
                    .and_then(|start| start.parse().ok())
            };
            old_line = parse(ranges.next(), '-');
            new_line = parse(ranges.next(), '+');
            continue;
        }
        let safe: String = text.chars().filter_map(terminal_safe_character).collect();
        let (sign, rest) = match safe.chars().next() {
            Some(sign @ ('-' | '+')) => (sign, &safe[1..]),
            Some(other) => (' ', &safe[other.len_utf8()..]),
            None => (' ', ""),
        };
        let advance = |line: &mut Option<usize>| {
            let current = *line;
            *line = current.map(|number| number + 1);
            current
        };
        let number = match sign {
            '-' => advance(&mut old_line),
            '+' => advance(&mut new_line),
            _ => {
                advance(&mut old_line);
                advance(&mut new_line)
            }
        };
        rows.push(DiffRow {
            number,
            sign,
            text: rest.to_owned(),
            changed: None,
        });
    }
    mark_changed_words(&mut rows);
    rows
}

fn mark_changed_words(rows: &mut [DiffRow]) {
    let mut index = 0;
    while index < rows.len() {
        let removed = rows[index..]
            .iter()
            .take_while(|row| row.sign == '-')
            .count();
        let added = rows[index + removed..]
            .iter()
            .take_while(|row| row.sign == '+')
            .count();
        if removed > 0 && removed == added {
            for offset in 0..removed {
                let (old, new) = rows.split_at_mut(index + removed);
                let (old, new) = (&mut old[index + offset], &mut new[offset]);
                if let Some((old_range, new_range)) = changed_words(&old.text, &new.text) {
                    old.changed = Some(old_range);
                    new.changed = Some(new_range);
                }
            }
        }
        index += (removed + added).max(1);
    }
}

fn changed_words(old: &str, new: &str) -> Option<(Range<usize>, Range<usize>)> {
    let old_tokens = word_tokens(old);
    let new_tokens = word_tokens(new);
    let same = |a: &Range<usize>, b: &Range<usize>| old[a.clone()] == new[b.clone()];
    let prefix = old_tokens
        .iter()
        .zip(&new_tokens)
        .take_while(|(a, b)| same(a, b))
        .count();
    let suffix = old_tokens[prefix..]
        .iter()
        .rev()
        .zip(new_tokens[prefix..].iter().rev())
        .take_while(|(a, b)| same(a, b))
        .count();
    let span = |text: &str, tokens: &[Range<usize>]| {
        let middle = &tokens[prefix..tokens.len() - suffix];
        match (middle.first(), middle.last()) {
            (Some(first), Some(last)) => first.start..last.end,
            _ => {
                let at = tokens.get(prefix).map_or(text.len(), |token| token.start);
                at..at
            }
        }
    };
    let old_range = span(old, &old_tokens);
    let new_range = span(new, &new_tokens);
    let whole = |text: &str, range: &Range<usize>| {
        text[..range.start].trim().is_empty() && text[range.end..].trim().is_empty()
    };
    if (old_range.is_empty() && new_range.is_empty())
        || (whole(old, &old_range) && whole(new, &new_range))
    {
        return None;
    }
    Some((old_range, new_range))
}

fn word_tokens(text: &str) -> Vec<Range<usize>> {
    #[derive(PartialEq)]
    enum Class {
        Word,
        Space,
        Mark,
    }
    let class = |character: char| {
        if character.is_alphanumeric() || character == '_' {
            Class::Word
        } else if character.is_whitespace() {
            Class::Space
        } else {
            Class::Mark
        }
    };
    let mut tokens: Vec<Range<usize>> = Vec::new();
    let mut previous: Option<Class> = None;
    for (at, character) in text.char_indices() {
        let current = class(character);
        match tokens.last_mut() {
            Some(last) if previous.as_ref() == Some(&current) => {
                last.end = at + character.len_utf8();
            }
            _ => tokens.push(at..at + character.len_utf8()),
        }
        previous = Some(current);
    }
    tokens
}

fn diff_style(row: &DiffRow) -> (Style, &'static str) {
    match row.sign {
        '-' => (failure(), "-"),
        '+' => (success(), "+"),
        _ => (normal(), " "),
    }
}

fn diff_number(row: &DiffRow) -> String {
    row.number
        .map_or_else(|| "      ".to_owned(), |number| format!("{number:>4} "))
}

fn diff_text(row: &DiffRow, style: Style) -> Line {
    let mut line = Line::default();
    match &row.changed {
        Some(changed) => {
            line.push(&row.text[..changed.start], style);
            line.push(&row.text[changed.clone()], style.reversed());
            line.push(&row.text[changed.end..], style);
        }
        None => line.push(row.text.as_str(), style),
    }
    line
}

fn diff_line(row: &DiffRow, width: usize) -> Line {
    let (style, sign) = diff_style(row);
    let mut line = Line::styled(diff_number(row), faint());
    line.push(format!("{sign}  "), style);
    for span in diff_text(row, style).spans {
        line.push(span.text, span.style);
    }
    truncate_line(line, width)
}

pub(super) fn dock_diff_lines(diff: &str, max_rows: usize, width: usize) -> Vec<Line> {
    let rows = diff_rows(&trim_leading_context(diff, DIFF_LEADING_CONTEXT));
    let mut lines: Vec<Line> = rows
        .iter()
        .take(max_rows)
        .map(|row| diff_line(row, width))
        .collect();
    if rows.len() > max_rows {
        lines.push(Line::styled(
            format!(
                "  … {}",
                count_noun(rows.len() - max_rows, "more line", "more lines")
            ),
            faint(),
        ));
    }
    lines
}

pub(super) fn whole_diff_lines(diff: &str, width: usize) -> Vec<Line> {
    let rows = diff_rows(diff);
    let digits = rows
        .iter()
        .filter_map(|row| row.number)
        .max()
        .map_or(0, |number| number.to_string().len())
        .max(4);
    let gutter = digits + 4;
    let content = width.saturating_sub(gutter).max(1);
    let mut lines = Vec::new();
    for row in rows {
        let (style, sign) = diff_style(&row);
        for (index, piece) in wrap_line_chars(diff_text(&row, style), content)
            .into_iter()
            .enumerate()
        {
            let mut line = if index == 0 {
                let number = row
                    .number
                    .map_or_else(String::new, |number| number.to_string());
                let mut line = Line::styled(format!("{number:>digits$} "), faint());
                line.push(format!("{sign}  "), style);
                line
            } else {
                Line::styled(" ".repeat(gutter), faint())
            }
            .with_chrome(gutter)
            .joined(piece.join);
            for span in piece.spans {
                line.push(span.text, span.style);
            }
            lines.push(line);
        }
    }
    lines
}

fn diff_window(total: usize, start: usize, limit: usize) -> Option<(usize, usize)> {
    if total <= limit {
        return None;
    }
    let window = limit.saturating_sub(1).max(1);
    Some((start.min(total - window), window))
}

pub(super) fn full_diff_window(diff: &str, start: usize, width: usize, limit: usize) -> Vec<Line> {
    let rows = diff_rows(diff);
    let Some((start, window)) = diff_window(rows.len(), start, limit) else {
        return rows.iter().map(|row| diff_line(row, width)).collect();
    };
    let end = start + window;
    let mut lines: Vec<Line> = rows[start..end]
        .iter()
        .map(|row| diff_line(row, width))
        .collect();
    lines.push(truncate_line(
        Line::styled(
            format!("  {}–{end} of {} · PgUp PgDn", start + 1, rows.len()),
            faint(),
        ),
        width,
    ));
    lines
}

pub(super) fn approval_diff_window(app: &App, limit: usize) -> Option<(usize, usize)> {
    let start = app.approval_diff_start()?;
    let diff = &app.pending_approval_preview()?.edit.as_ref()?.diff;
    Some(diff_window(diff_rows(diff).len(), start, limit).unwrap_or((0, limit)))
}

const MAX_APPROVAL_DIFF_ROWS: usize = 8;
const DIFF_LEADING_CONTEXT: usize = 1;

fn trim_leading_context(diff: &str, keep: usize) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut lead: Vec<&str> = Vec::new();
    let mut header: Option<usize> = None;
    let flush = |out: &mut Vec<String>, lead: &mut Vec<&str>, header: Option<usize>| {
        let skipped = lead.len().saturating_sub(keep);
        if let Some(index) = header
            && skipped > 0
        {
            out[index] = shift_hunk(&out[index], skipped);
        }
        out.extend(lead.drain(skipped..).map(str::to_owned));
        lead.clear();
    };
    for line in diff.lines() {
        if line.starts_with("@@") {
            flush(&mut out, &mut lead, header);
            header = Some(out.len());
            out.push(line.to_owned());
            continue;
        }
        if header.is_some() && line.starts_with(' ') {
            lead.push(line);
            continue;
        }
        flush(&mut out, &mut lead, header);
        header = None;
        out.push(line.to_owned());
    }
    out.extend(lead.into_iter().map(str::to_owned));
    out.join("\n")
}

fn shift_hunk(header: &str, skipped: usize) -> String {
    header
        .split(' ')
        .enumerate()
        .map(|(index, part)| {
            let sign = match index {
                1 => '-',
                2 => '+',
                _ => return part.to_owned(),
            };
            let Some(range) = part.strip_prefix(sign) else {
                return part.to_owned();
            };
            let (start, rest) = range.split_once(',').unwrap_or((range, ""));
            match start.parse::<usize>() {
                Ok(start) if rest.is_empty() => format!("{sign}{}", start + skipped),
                Ok(start) => format!("{sign}{},{rest}", start + skipped),
                Err(_) => part.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn history_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
    let Some(picker) = app.history_picker() else {
        return fit_height(Vec::new(), height);
    };
    picker_frame(
        picker,
        PickerChrome {
            header: Some(PickerHeader {
                title: app.turn_picker_title.unwrap_or("HISTORY"),
                hint: if app.turn_picker_title.is_some() {
                    "type to search, Enter selects, Esc closes"
                } else {
                    "type to search, Enter edits the prompt, Esc closes"
                },
                placeholder: "recent prompts",
            }),
            question: None,
            empty: "  No matching prompts.",
            windowed: false,
        },
        width,
        height,
        |row: &crate::input::HistoryRow, selected, out| {
            let mut line = cursor_prefix(selected);
            line.push(
                preview(&row.text, width.saturating_sub(6)),
                if selected { normal().bold() } else { normal() },
            );
            out.push(finish_row(line, selected, width));
        },
    )
}
