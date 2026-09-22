use super::*;
use crate::{
    commands::Category,
    input::{
        ApprovalModeRow, CommandRow, EffortRow, ModelRow, Overlay, ProfileRow, SessionRow,
        SkillRow, ThemeRow,
    },
    picker::{Picker, PickerItem},
};

/// Static text around a picker's rows.
struct PickerChrome<'a> {
    title: &'a str,
    hint: &'a str,
    placeholder: &'a str,
    /// A pending yes/no question shown under the search row.
    question: Option<Line>,
    /// Shown instead of rows when nothing matches.
    empty: &'a str,
}

/// The frame every picker shares: a section header with hints, the search
/// row, an optional question, then the filtered rows scrolled so the cursor
/// stays visible. `row` draws one item; the bool marks the cursor.
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

/// The `search:` row shared by every picker.
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

/// Finish a picker row: pad to `width` and, when selected, paint the whole
/// row on the selection background so it reads without the `>` alone.
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
    let mut provider: Option<String> = None;
    picker_frame(
        picker,
        PickerChrome {
            title: "MODELS",
            hint: if app.focused().is_some() {
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

/// Profile picker: every profile the server advertises for this workspace
/// with its approval mode, model override, and declaring pack. The row in
/// effect (the focused session's, or the default for new sessions) is marked.
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

/// Approval-mode picker: every mode the server accepts with what it holds
/// for approval. The mode in effect is marked.
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

/// Effort picker: `default` (restore config/profile) plus the levels the
/// focused model advertises, or every level when the catalog is silent. The
/// pin in effect is marked.
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

/// Skills picker: the workspace's indexed commands and skills with their
/// source and description. Commands are grouped before skills.
pub(super) fn skill_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
    let Some(Overlay::Skills(picker)) = &app.overlay else {
        return fit_height(Vec::new(), height);
    };
    let mut kind: Option<qq_protocol::GuidanceKind> = None;
    picker_frame(
        picker,
        PickerChrome {
            title: "SKILLS",
            hint: "type to search, Enter inserts a command or runs a skill, Esc closes",
            placeholder: "all commands and skills",
            question: None,
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

/// Theme picker. Each row shows the theme name and a swatch of its roles,
/// painted in that theme's own colors so the list doubles as a preview
/// strip; the whole frame is already drawn in the highlighted theme.
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

/// The command palette and the help overlay: every command with its chord
/// and slash name. Help groups rows under category headers.
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

/// The approval block drawn under a tool call awaiting an answer: the
/// command or the diff head, then the four choices. Rendered inline in the
/// transcript so the decision is made with the run's context on screen.
pub(super) fn approval_block(app: &App, width: usize) -> Vec<Line> {
    let mut lines = Vec::new();
    if let Some(question) = app.pending_question() {
        // A question, not a permission: the current question with numbered
        // options; earlier answers stay visible so a multi-question hold
        // reads as a form being filled in.
        let answered = app.question_answers.len();
        let mut title = Line::styled("     ◇ ", warning());
        title.push("question", warning().bold());
        if question.questions.len() > 1 {
            title.push(
                format!(
                    "  {}/{}",
                    (answered + 1).min(question.questions.len()),
                    question.questions.len()
                ),
                muted(),
            );
        }
        lines.push(truncate_line(title, width));
        for (index, item) in question.questions.iter().enumerate() {
            if index < answered {
                let mut line = Line::styled("       ", muted());
                line.push(item.prompt.as_str(), muted());
                line.push(" → ", muted());
                line.push(app.question_answers[index].as_str(), normal());
                lines.push(truncate_line(line, width));
                continue;
            }
            if index > answered {
                break;
            }
            let mut line = Line::styled("       ", muted());
            line.push(item.prompt.as_str(), normal().bold());
            lines.push(truncate_line(line, width));
            for (option_index, option) in item.options.iter().enumerate() {
                let mut line = Line::styled("         ", muted());
                line.push(format!("{}", option_index + 1), accent().bold());
                line.push(format!(" {option}"), normal());
                lines.push(truncate_line(line, width));
            }
            let mut hint = Line::styled("       ", muted());
            if item.free_text {
                hint.push("type an answer and press Enter", muted());
                if !item.options.is_empty() {
                    hint.push(", or a number to pick", muted());
                }
            } else {
                hint.push("press a number to pick", muted());
            }
            hint.push("   ", muted());
            hint.push("Esc", accent().bold());
            hint.push(" decline", muted());
            lines.push(truncate_line(hint, width));
        }
        return lines;
    }
    let mut title = Line::styled("     ◇ ", warning());
    title.push("approval needed", warning().bold());
    lines.push(truncate_line(title, width));
    // Both previews come from the approval event: the server states exactly
    // what it will run or write, so the client never re-derives it from the
    // call's arguments.
    let preview = app.pending_approval_preview();
    if let Some(shell) = preview.and_then(|preview| preview.shell.as_ref()) {
        let mut line = Line::styled("       $ ", muted());
        line.push(shell.command.as_str(), normal().bold());
        if let Some(cwd) = &shell.cwd {
            line.push(format!("  (in {cwd})"), muted());
        }
        lines.push(truncate_line(line, width));
        // Why the gate is asking: the classifier's rule ids, as words.
        if !shell.reasons.is_empty() {
            let reasons: Vec<String> = shell
                .reasons
                .iter()
                .take(3)
                .map(|reason| reason.replace('_', " "))
                .collect();
            let mut line = Line::styled("         ", muted());
            line.push(format!("asks because: {}", reasons.join(", ")), muted());
            lines.push(truncate_line(line, width));
        }
    }
    if let Some(fetch) = preview.and_then(|preview| preview.fetch.as_ref()) {
        let mut line = Line::styled("       ", muted());
        line.push(fetch.method.as_deref().unwrap_or("GET"), muted());
        line.push(" ", muted());
        line.push(fetch.url.as_str(), normal().bold());
        lines.push(truncate_line(line, width));
        let mut line = Line::styled("         ", muted());
        line.push(
            format!("session/workspace grants cover host {}", fetch.host),
            muted(),
        );
        lines.push(truncate_line(line, width));
    }
    if let Some(edit) = preview.and_then(|preview| preview.edit.as_ref()) {
        let mut line = Line::styled("       ", muted());
        line.push(
            elide_path(&edit.path, width.saturating_sub(9)),
            normal().bold(),
        );
        lines.push(truncate_line(line, width));
        lines.extend(
            diff_lines(&edit.diff, MAX_APPROVAL_DIFF_ROWS, width.saturating_sub(2))
                .into_iter()
                .map(|line| {
                    let mut indented = Line::styled("  ", muted());
                    for span in line.spans {
                        indented.push(span.text, span.style);
                    }
                    indented
                }),
        );
    }
    let mut choices = Line::styled("       ", muted());
    // A grant the server cannot store is not offered. The keys still approve
    // this call once and say why; the prompt just does not pretend a session
    // grant is available.
    let grantable = app
        .pending_approval()
        .is_some_and(|tool_call| approval_grant_recordable(tool_call, preview));
    let offered: &[(&str, &str)] = if grantable {
        &[
            ("y", "once"),
            ("a", "session"),
            ("w", "workspace"),
            ("n", "deny"),
        ]
    } else {
        &[("y", "once"), ("n", "deny")]
    };
    for (index, (key, label)) in offered.iter().copied().enumerate() {
        if index > 0 {
            choices.push("   ", muted());
        }
        choices.push(key, accent().bold());
        choices.push(format!(" {label}"), muted());
    }
    if !grantable {
        choices.push("   ", muted());
        choices.push("command too long to grant for the session", muted());
    }
    lines.push(truncate_line(choices, width));
    lines
}

/// Diff rows an inline approval shows before offering to scroll.
const MAX_APPROVAL_DIFF_ROWS: usize = 12;

/// Prompt-history search: newest first, fuzzy filtered by what the user types.
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
