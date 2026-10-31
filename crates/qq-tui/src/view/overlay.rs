use super::*;
use crate::{
    commands::Category,
    input::{
        ApprovalModeRow, CommandRow, EffortRow, ModelRow, Overlay, ProfileRow, SessionRow,
        SkillRow, ThemeRow,
    },
    picker::{Picker, PickerItem},
};

struct PickerChrome<'a> {
    title: &'a str,
    hint: &'a str,
    placeholder: &'a str,
    question: Option<Line>,
    empty: &'a str,
}

fn picker_frame<T: PickerItem>(
    picker: &Picker<T>,
    chrome: PickerChrome<'_>,
    width: usize,
    height: usize,
    mut row: impl FnMut(&T, bool, &mut Vec<Line>),
) -> Vec<Line> {
    let PickerChrome {
        title,
        hint,
        placeholder,
        question,
        empty,
    } = chrome;
    let mut lines = vec![section(title, hint)];
    lines.push(search_line(&picker.query, placeholder));
    if let Some(question) = question {
        lines.push(truncate_line(question, width));
    }
    lines.push(Line::default());
    if picker.filtered().len() == 0 {
        lines.push(Line::styled(empty, muted().italic()));
        return fit_height(lines, height);
    }
    let cursor = picker.cursor();
    let mut results = Vec::with_capacity(picker.filtered().len());
    let mut selected_row = 0;
    for (position, (_, item)) in picker.filtered().enumerate() {
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

pub(super) fn session_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
    let Some(Overlay::Sessions {
        picker,
        scope,
        confirm,
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
    picker_frame(
        picker,
        PickerChrome {
            title: if scope.is_some() {
                "AGENTS"
            } else {
                "SESSIONS"
            },
            hint: if confirm.is_some() {
                "y confirms, n or Esc cancels"
            } else {
                "type to search, Enter focuses, Ctrl-D deletes, Esc closes"
            },
            placeholder: "all sessions",
            question,
            empty: &empty,
        },
        width,
        height,
        |row: &SessionRow, selected, out| {
            let depth = app.sessions.depth(row.id);
            let prefix = format!(
                "  {}{} ",
                "  ".repeat(depth),
                if selected { ">" } else { " " }
            );
            out.push(finish_row(
                session_line(app, row.id, width, &prefix),
                selected,
                width,
            ));
        },
    )
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
            title: "MODELS",
            hint: if unauthenticated_only {
                "no provider has a credential yet; Esc closes"
            } else if app.focused().is_some() {
                "type to search, Enter sets the session model, Ctrl-N creates a session, Esc closes"
            } else {
                "type to search, Up/Down select, Enter creates session, Esc closes"
            },
            placeholder: "all models",
            question: None,
            empty: "  No matching models.",
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
            title: "PROFILES",
            hint: if focused.is_some() {
                "type to search, Enter sets the session profile, Esc closes"
            } else {
                "type to search, Enter sets the profile for new sessions, Esc closes"
            },
            placeholder: "all profiles",
            question: None,
            empty: "  No matching profiles.",
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
            if let Some(pack) = &row.pack {
                line.push(format!("  pack {pack}"), accent());
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
            title: "APPROVAL MODE",
            hint: if app.focused().is_some() {
                "type to search, Enter sets the session's mode, Esc closes"
            } else {
                "type to search, Enter sets the mode for new sessions, Esc closes"
            },
            placeholder: "all modes",
            question: None,
            empty: "  No matching modes.",
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
            question: None,
            empty: "  No matching effort levels.",
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
            title: "SKILLS",
            hint: "type to search, Enter inserts a command or runs a skill, Esc closes",
            placeholder: "all commands and skills",
            question: host_warning,
            empty: "  No matching commands or skills.",
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
            title: "THEMES",
            hint: "Up/Down preview live, Enter keeps, Esc restores the previous theme",
            placeholder: "all themes",
            question: None,
            empty: "  No matching themes.",
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
            title: if help { "HELP" } else { "COMMANDS" },
            hint: if help {
                "every key and command; type to filter, Enter runs, Esc closes"
            } else {
                "type to search, Enter runs the command, Esc closes"
            },
            placeholder: "all commands",
            question: None,
            empty: "  No matching commands.",
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
        lines.extend(dock_diff_lines(&edit.diff, MAX_APPROVAL_DIFF_ROWS, width));
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

pub(super) fn dock_diff_lines(diff: &str, max_rows: usize, width: usize) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut old_line: Option<usize> = None;
    let mut new_line: Option<usize> = None;
    let body: Vec<&str> = diff
        .lines()
        .filter(|line| !line.starts_with("+++") && !line.starts_with("---"))
        .collect();
    let total = body.iter().filter(|line| !line.starts_with("@@")).count();
    for text in body {
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
        if lines.len() == max_rows {
            lines.push(Line::styled(
                format!(
                    "  … {}",
                    count_noun(total - max_rows, "more line", "more lines")
                ),
                faint(),
            ));
            break;
        }
        let safe: String = text.chars().filter_map(terminal_safe_character).collect();
        let (sign, rest) = safe.split_at(safe.chars().next().map_or(0, char::len_utf8));
        let (number, style, sign) = match sign {
            "-" => (old_line, failure(), "-"),
            "+" => (new_line, success(), "+"),
            _ => (new_line, normal(), " "),
        };
        let mut line = Line::styled(
            number.map_or_else(|| "      ".to_owned(), |number| format!("{number:>4} ")),
            faint(),
        );
        line.push(format!("{sign}  "), style);
        line.push(rest, style);
        match sign {
            "-" => old_line = old_line.map(|number| number + 1),
            "+" => new_line = new_line.map(|number| number + 1),
            _ => {
                old_line = old_line.map(|number| number + 1);
                new_line = new_line.map(|number| number + 1);
            }
        }
        lines.push(truncate_line(line, width));
    }
    lines
}

const MAX_APPROVAL_DIFF_ROWS: usize = 8;

pub(super) fn history_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
    let Some(picker) = app.history_picker() else {
        return fit_height(Vec::new(), height);
    };
    picker_frame(
        picker,
        PickerChrome {
            title: "HISTORY",
            hint: "type to search, Enter edits the prompt, Esc closes",
            placeholder: "recent prompts",
            question: None,
            empty: "  No matching prompts.",
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

pub(super) fn goal_check_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
    let Some(Overlay::GoalCheck {
        target,
        picker,
        detail_row,
    }) = &app.overlay
    else {
        return Vec::new();
    };
    let width = width.max(1);
    let mut rows = Vec::new();
    let command: String = target
        .command
        .chars()
        .filter_map(terminal_safe_character)
        .collect();
    let lines: Vec<&str> = command.lines().collect();
    let fits_on_rule = lines.len() == 1
        && text_width(&command) <= super::chrome::GOAL_CHECK_RULE_COMMAND
        && text_width(&command) + 24 <= width;
    if !fits_on_rule {
        let wrapped: Vec<Line> = lines
            .iter()
            .flat_map(|line| {
                wrap_line_chars(Line::styled(*line, muted()), width.saturating_sub(4).max(1))
            })
            .collect();
        let visible = height.saturating_sub(3).max(1);
        let start = (*detail_row).min(wrapped.len().saturating_sub(visible));
        for line in wrapped.into_iter().skip(start).take(visible) {
            let mut row = Line::styled("  $ ", faint());
            for span in line.spans {
                row.push(span.text, span.style);
            }
            rows.push(truncate_line(row, width));
        }
    }
    for (index, (_, row)) in picker.filtered().enumerate() {
        let selected = index == picker.cursor();
        let mut line = if selected {
            Line::styled("▎ ", accent())
        } else {
            Line::styled("  ", normal())
        };
        line.push(
            format!("{:<36}", row.label),
            if selected { normal() } else { muted() },
        );
        line.push(row.description, faint());
        rows.push(truncate_line(line, width));
    }
    if picker.filtered().len() == 0 {
        rows.push(Line::styled("  No matching choice. esc cancels.", faint()));
    }
    rows
}

pub(super) fn goal_form(app: &App, width: usize, height: usize) -> Vec<Line> {
    let Some(Overlay::GoalForm(form)) = &app.overlay else {
        return Vec::new();
    };
    let width = width.max(1);
    let safe =
        |text: &str| -> String { text.chars().filter_map(terminal_safe_character).collect() };
    let mut out = vec![Line::styled(
        format!(
            "  {}",
            if form.replace {
                "Replace goal"
            } else if form.expected.is_some() {
                "Edit goal"
            } else {
                "New goal"
            }
        ),
        normal().bold(),
    )];
    if let Some(previous) = &form.previous_objective {
        let mut line = Line::styled("  replaces  ", faint());
        line.push(safe(previous), muted());
        out.push(truncate_line(line, width));
    }
    let marker = |selected: bool| {
        if selected {
            Line::styled("▎ ", accent())
        } else {
            Line::styled("  ", normal())
        }
    };
    let mut selected = 0;
    for (index, (label, note)) in [("Objective", ""), ("Check command", "  optional")]
        .into_iter()
        .enumerate()
    {
        let mut line = marker(form.field == index);
        line.push(label, muted());
        line.push(note, faint());
        if form.field == index {
            selected = out.len();
        }
        out.push(truncate_line(line, width));
        let editor = &form.fields[index];
        let text = &editor.text;
        let cursor = editor.cursor();
        let editing = form.field == index && !form.confirm && form.sending.is_none();
        let shown = if editing {
            format!("{}▏{}", safe(&text[..cursor]), safe(&text[cursor..]))
        } else {
            safe(text)
        };
        let field_width = width.saturating_sub(2).max(1);
        if editing {
            let before = format!("{}▏", safe(&text[..cursor])).replace('\n', " ");
            let caret_row = wrap_line_chars(Line::styled(before, normal()), field_width)
                .len()
                .saturating_sub(1);
            selected = out.len() + caret_row;
        }
        if shown.is_empty() {
            out.push(Line::styled("  (empty)", faint()));
        } else {
            for row in wrap_line_chars(
                Line::styled(shown.replace('\n', " "), normal()),
                field_width,
            ) {
                let mut line = Line::styled("  ", normal());
                for span in row.spans {
                    line.push(span.text, span.style);
                }
                out.push(truncate_line(line, width));
            }
        }
    }
    let mut line = marker(form.field == 2);
    line.push("Budget", muted());
    if form.field == 2 {
        selected = out.len();
    }
    out.push(truncate_line(line, width));
    let budget = form.budget(0);
    let window = if form.preset == 0 {
        format!("until {}", goal_timestamp(budget.deadline_ms))
    } else {
        goal_duration(budget.deadline_ms)
    };
    let mut summary = format!(
        "{} · {} runs · {window}",
        form.preset_label().to_lowercase(),
        budget.max_goal_runs
    );
    if let Some(turns) = budget.per_run.max_model_turns {
        summary.push_str(&format!(" · {turns} turns per run"));
    }
    if let Some(tokens) = budget.max_total_tokens {
        summary.push_str(&format!(" · {tokens} tokens"));
    }
    if let Some(cost) = budget.max_cost_usd_nanos {
        summary.push_str(&format!(" · {}", format_cost(cost)));
    }
    out.push(truncate_line(
        Line::styled(format!("  {summary}"), normal()),
        width,
    ));
    let note = if form.sending.is_some() {
        Some(("submitting…", faint()))
    } else if form.refresh == crate::input::GoalFormRefresh::Pending {
        Some(("refreshing the current goal; the draft is kept", faint()))
    } else if form.refresh == crate::input::GoalFormRefresh::Failed {
        Some(("refresh failed; enter retries", warning()))
    } else {
        Some((
            "A check command runs only after /goal check consent.",
            faint(),
        ))
    };
    if let Some((text, style)) = note {
        out.push(truncate_line(
            Line::styled(format!("  {text}"), style),
            width,
        ));
    }
    if let Some(error) = &form.error {
        let mut line = Line::styled("  ✕ ", failure());
        line.push(safe(error), normal());
        out.push(truncate_line(line, width));
    }
    if out.len() > height {
        let anchor = if form.confirm { 0 } else { selected };
        return selection_viewport(out, height, anchor.saturating_add_signed(form.scroll));
    }
    out
}
