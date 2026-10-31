use crossterm::style::Color;
use unicode_width::UnicodeWidthStr;

use crate::theme::Palette;

mod scenes;

pub use scenes::DesignScene;

pub const MARGIN: usize = 2;
const LANE: usize = 2;
const SIDEBAR_REGULAR: usize = 32;
const SIDEBAR_WIDE: usize = 42;
const SIDEBAR_PADDING: usize = 2;
const REGULAR: usize = 90;
const WIDE: usize = 160;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Text,
    Muted,
    Faint,
    Accent,
    Info,
    Warning,
    Error,
    Add,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub role: Role,
    pub bold: bool,
    pub inverse: bool,
    pub fill: bool,
}

pub type Row = Vec<Span>;

fn span(text: impl Into<String>, role: Role) -> Span {
    Span {
        text: text.into(),
        role,
        bold: false,
        inverse: false,
        fill: false,
    }
}

fn bold(text: impl Into<String>, role: Role) -> Span {
    Span {
        bold: true,
        ..span(text, role)
    }
}

fn inverse(text: impl Into<String>, role: Role) -> Span {
    Span {
        inverse: true,
        ..span(text, role)
    }
}

fn width_of(row: &[Span]) -> usize {
    row.iter().map(|span| span.text.width()).sum()
}

fn lane(glyph: &str, role: Role) -> Span {
    span(format!("{glyph} "), role)
}

fn blank_lane() -> Span {
    span(" ".repeat(LANE), Role::Text)
}

#[must_use]
pub fn fit(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut kept = String::new();
    for character in text.chars() {
        if kept.width() + UnicodeWidthStr::width(character.encode_utf8(&mut [0; 4]) as &str) + 1
            > width
        {
            break;
        }
        kept.push(character);
    }
    let trimmed = kept.trim_end().to_owned();
    format!("{trimmed}…")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Running,
    NeedsYou,
    Done,
    Failed,
}

impl State {
    pub const ALL: [Self; 5] = [
        Self::Idle,
        Self::Running,
        Self::NeedsYou,
        Self::Done,
        Self::Failed,
    ];

    #[must_use]
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Idle => "○",
            Self::Running => "◐",
            Self::NeedsYou => "◇",
            Self::Done => "●",
            Self::Failed => "✕",
        }
    }

    #[must_use]
    pub const fn role(self) -> Role {
        match self {
            Self::Idle | Self::Done => Role::Faint,
            Self::Running => Role::Info,
            Self::NeedsYou => Role::Warning,
            Self::Failed => Role::Error,
        }
    }

    const fn row_role(self) -> Role {
        match self {
            Self::Idle | Self::Done => Role::Muted,
            other => other.role(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    ReadOnly,
    Ask,
    Auto,
    Full,
}

impl Mode {
    #[must_use]
    pub const fn rail_role(self) -> Role {
        match self {
            Self::ReadOnly => Role::Faint,
            Self::Ask | Self::Auto => Role::Accent,
            Self::Full => Role::Warning,
        }
    }

    const fn label(self) -> Option<&'static str> {
        match self {
            Self::ReadOnly => Some("read-only"),
            Self::Ask => Some("ask"),
            Self::Auto => None,
            Self::Full => Some("full"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Block {
    Banner(Vec<Row>),
    Prompt(String),
    Answer {
        text: String,
        streaming: bool,
    },
    Error(String),
    Note(String),
    Reasoning(String),
    Code {
        language: String,
        lines: Vec<String>,
    },
    Tool(Tool),
    Agents {
        lanes: Vec<Lane>,
    },
    Receipt(String),
    Raw(Vec<Row>),
}

#[derive(Debug, Clone)]
pub struct Tool {
    pub state: State,
    pub verb: String,
    pub subject: String,
    pub metric: String,
    pub tail: Vec<String>,
    pub diff: Vec<DiffLine>,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub number: u32,
    pub sign: char,
    pub text: String,
    pub changed: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Lane {
    pub state: State,
    pub title: String,
    pub activity: String,
}

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub state: State,
    pub title: String,
    pub agents: usize,
    pub focused: bool,
    pub activity: String,
    pub cost: String,
}

#[derive(Debug, Clone)]
pub struct Dock {
    pub state: State,
    pub activity: String,
    pub attention: String,
    pub compact_attention: String,
    pub panel: Vec<Row>,
    pub queued: Vec<String>,
    pub composer: Composer,
    pub mode: Mode,
    pub title: String,
    pub model: String,
    pub context: String,
    pub cost: String,
    pub place: String,
    pub hints: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub enum Composer {
    Placeholder,
    Draft(String),
    Keys(Vec<(String, String)>),
}

#[derive(Debug, Clone)]
pub struct Scene {
    pub blocks: Vec<Block>,
    pub sessions: Vec<SessionRow>,
    pub dock: Dock,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub rows: Vec<Row>,
    pub width: usize,
    pub body: usize,
    pub sidebar: usize,
}

impl Frame {
    #[must_use]
    pub fn plain(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|row| {
                let text: String = row.iter().map(|span| span.text.as_str()).collect();
                text.trim_end().to_owned()
            })
            .collect()
    }

    #[must_use]
    pub fn ansi(&self, palette: &Palette) -> Vec<u8> {
        let mut out = String::from("\x1b[H\x1b[2J");
        for (index, row) in self.rows.iter().enumerate() {
            if index > 0 {
                out.push_str("\r\n");
            }
            for span in row {
                let mut codes = vec![sgr(color(palette, span.role), 30)];
                if span.fill {
                    codes.push(sgr(palette.surface, 40));
                }
                if span.bold {
                    codes.push("1".to_owned());
                }
                if span.inverse {
                    codes.push("7".to_owned());
                }
                if span.role == Role::Faint && faint_is_dim(palette) {
                    codes.push("2".to_owned());
                }
                out.push_str(&format!("\x1b[{}m{}\x1b[0m", codes.join(";"), span.text));
            }
        }
        out.into_bytes()
    }
}

fn sgr(color: Color, base: u8) -> String {
    let ansi = |index: u8| (base + index).to_string();
    let bright = |index: u8| (base + 60 + index).to_string();
    match color {
        Color::Rgb { r, g, b } => format!("{};2;{r};{g};{b}", base + 8),
        Color::AnsiValue(value) => format!("{};5;{value}", base + 8),
        Color::Black => ansi(0),
        Color::DarkRed => ansi(1),
        Color::DarkGreen => ansi(2),
        Color::DarkYellow => ansi(3),
        Color::DarkBlue => ansi(4),
        Color::DarkMagenta => ansi(5),
        Color::DarkCyan => ansi(6),
        Color::Grey => ansi(7),
        Color::DarkGrey => bright(0),
        Color::Red => bright(1),
        Color::Green => bright(2),
        Color::Yellow => bright(3),
        Color::Blue => bright(4),
        Color::Magenta => bright(5),
        Color::Cyan => bright(6),
        Color::White => bright(7),
        Color::Reset => (base + 9).to_string(),
    }
}

const fn faint_is_dim(palette: &Palette) -> bool {
    !matches!(palette.muted, Color::Rgb { .. })
}

#[must_use]
pub fn faint(palette: &Palette) -> Color {
    match (palette.muted, palette.surface) {
        (
            Color::Rgb { r, g, b },
            Color::Rgb {
                r: sr,
                g: sg,
                b: sb,
            },
        ) => {
            let toward = |value: u8, surface: u8| {
                let background = u16::from(surface) / 2;
                ((u16::from(value) + background) / 2) as u8
            };
            Color::Rgb {
                r: toward(r, sr),
                g: toward(g, sg),
                b: toward(b, sb),
            }
        }
        (muted, _) => muted,
    }
}

#[must_use]
pub fn color(palette: &Palette, role: Role) -> Color {
    match role {
        Role::Text => palette.text,
        Role::Muted => palette.muted,
        Role::Faint => faint(palette),
        Role::Accent => palette.accent,
        Role::Info => palette.info,
        Role::Warning => palette.warning,
        Role::Error => palette.error,
        Role::Add => palette.success,
        Role::Remove => palette.error,
    }
}

#[must_use]
pub fn render(scene: &Scene, size: (u16, u16)) -> Frame {
    let width = usize::from(size.0);
    let height = usize::from(size.1);
    let wide = width >= WIDE;
    let sidebar = match (width >= REGULAR && scene.sessions.len() >= 2, wide) {
        (false, _) => 0,
        (true, false) => SIDEBAR_REGULAR,
        (true, true) => SIDEBAR_WIDE,
    };
    let pane = width - sidebar;
    let content = pane - 2 * MARGIN;
    let dock = dock_rows(&scene.dock, content, sidebar > 0);
    let body = height - dock.len();
    let mut transcript = transcript_rows(&scene.blocks, content - LANE);
    if transcript.len() > body {
        transcript.drain(..transcript.len() - body);
    }
    let mut left = Vec::with_capacity(height);
    for index in 0..body {
        left.push(transcript.get(index).cloned().unwrap_or_default());
    }
    left.extend(dock);
    let side = sidebar_rows(&scene.sessions, sidebar, wide);
    let rows = left
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let mut row = vec![span(" ".repeat(MARGIN), Role::Text)];
            row.extend(clip(line, content));
            if sidebar > 0 {
                let row = pad(row, pane);
                let mut row = row;
                let entry = side.get(index).cloned().unwrap_or_default();
                row.extend(
                    pad(entry, sidebar)
                        .into_iter()
                        .map(|span| Span { fill: true, ..span }),
                );
                row
            } else {
                row
            }
        })
        .collect();
    Frame {
        rows,
        width,
        body,
        sidebar,
    }
}

fn pad(row: Row, width: usize) -> Row {
    let mut row = clip(row, width);
    let used = width_of(&row);
    if used < width {
        row.push(span(" ".repeat(width - used), Role::Text));
    }
    row
}

fn sidebar_rows(sessions: &[SessionRow], sidebar: usize, wide: bool) -> Vec<Row> {
    if sidebar == 0 {
        return Vec::new();
    }
    let inner = sidebar - 2 * SIDEBAR_PADDING;
    let indent = || span(" ".repeat(SIDEBAR_PADDING), Role::Text);
    let mut rows = vec![Vec::new()];
    for entry in column_rows(sessions, inner, wide) {
        let mut row = vec![indent()];
        row.extend(clip(entry, inner));
        rows.push(row);
    }
    rows
}

fn clip(row: Row, width: usize) -> Row {
    let mut used = 0;
    let mut out = Vec::with_capacity(row.len());
    for mut span in row {
        let span_width = span.text.width();
        if used + span_width <= width {
            used += span_width;
            out.push(span);
            continue;
        }
        span.text = fit(&span.text, width - used);
        out.push(span);
        break;
    }
    out
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split(' ') {
            let candidate = if line.is_empty() {
                word.width()
            } else {
                line.width() + 1 + word.width()
            };
            if candidate > width && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        lines.push(line);
    }
    lines
}

fn right_aligned(left: Row, right: Row, width: usize) -> Row {
    let gap = width
        .saturating_sub(width_of(&left) + width_of(&right))
        .max(1);
    let mut row = left;
    row.push(span(" ".repeat(gap), Role::Text));
    row.extend(right);
    row
}

fn keys_row(keys: &[(String, String)]) -> Row {
    let mut row = Vec::new();
    for (index, (key, label)) in keys.iter().enumerate() {
        if index > 0 {
            row.push(span("   ", Role::Text));
        }
        row.push(span(key.clone(), Role::Text));
        row.push(span(format!(" {label}"), Role::Faint));
    }
    row
}

fn attached(block: &Block) -> bool {
    matches!(block, Block::Note(_))
}

fn grouped(block: &Block) -> bool {
    matches!(
        block,
        Block::Tool(_) | Block::Agents { .. } | Block::Receipt(_)
    )
}

fn transcript_rows(blocks: &[Block], text: usize) -> Vec<Row> {
    let page = LANE + text;
    let mut rows: Vec<Row> = Vec::new();
    let mut previous: Option<&Block> = None;
    for block in blocks {
        let joins =
            attached(block) || previous.is_some_and(|before| grouped(before) && grouped(block));
        if previous.is_some() && !joins {
            rows.push(Vec::new());
        }
        previous = Some(block);
        match block {
            Block::Banner(lines) | Block::Raw(lines) => {
                for line in lines {
                    let mut row = vec![blank_lane()];
                    row.extend(line.iter().cloned());
                    rows.push(clip(row, page));
                }
            }
            Block::Prompt(prompt) => {
                for line in wrap(prompt, text) {
                    rows.push(vec![lane("┃", Role::Accent), bold(line, Role::Text)]);
                }
            }
            Block::Answer {
                text: answer,
                streaming,
            } => {
                let lines = wrap(answer, text);
                let last = lines.len() - 1;
                for (index, line) in lines.into_iter().enumerate() {
                    let mut row = vec![blank_lane(), span(line, Role::Text)];
                    if *streaming && index == last {
                        row.push(span("▍", Role::Accent));
                    }
                    rows.push(row);
                }
            }
            Block::Error(message) => {
                rows.push(vec![
                    lane("✕", Role::Error),
                    span(message.clone(), Role::Text),
                ]);
            }
            Block::Note(note) => {
                rows.push(vec![blank_lane(), span(note.clone(), Role::Faint)]);
            }
            Block::Reasoning(elapsed) => {
                rows.push(vec![
                    lane("∴", Role::Faint),
                    span(format!("thought for {elapsed}"), Role::Faint),
                ]);
            }
            Block::Code { language, lines } => {
                let panel = |line: String, role: Role| {
                    let body = format!(" {line}");
                    let pad = text.saturating_sub(body.width());
                    vec![
                        blank_lane(),
                        Span {
                            fill: true,
                            ..span(format!("{body}{}", " ".repeat(pad)), role)
                        },
                    ]
                };
                rows.push(panel(
                    format!("{language:>width$}", width = text.saturating_sub(2)),
                    Role::Faint,
                ));
                for line in lines {
                    rows.push(panel(line.clone(), Role::Text));
                }
                rows.push(panel(String::new(), Role::Text));
            }
            Block::Tool(tool) => tool_rows(tool, text, &mut rows),
            Block::Agents { lanes } => agent_rows(lanes, text, &mut rows),
            Block::Receipt(summary) => {
                rows.push(vec![
                    lane("●", State::Done.role()),
                    span(fit(summary, text), Role::Muted),
                ]);
            }
        }
    }
    rows
}

const VERB: usize = 9;

fn tool_rows(tool: &Tool, text: usize, rows: &mut Vec<Row>) {
    let metric_role = if tool.state == State::Failed {
        Role::Error
    } else {
        Role::Faint
    };
    let room = text.saturating_sub(VERB + tool.metric.width() + 2);
    let mut row = vec![
        lane(tool.state.glyph(), tool.state.row_role()),
        span(format!("{:<VERB$}", tool.verb), Role::Text),
        span(fit(&tool.subject, room), Role::Muted),
    ];
    if !tool.metric.is_empty() {
        row.push(span(format!("  {}", tool.metric), metric_role));
    }
    rows.push(row);
    for line in &tool.tail {
        rows.push(vec![
            lane("│", Role::Faint),
            span(fit(line, text), Role::Muted),
        ]);
    }
    for line in &tool.diff {
        let (role, sign) = match line.sign {
            '+' => (Role::Add, "+"),
            '-' => (Role::Remove, "-"),
            _ => (Role::Text, " "),
        };
        let mut row = vec![
            lane("│", Role::Faint),
            span(format!("{:>4} ", line.number), Role::Faint),
            span(format!("{sign}  "), role),
        ];
        match &line.changed {
            Some(changed) => {
                let (before, after) = line
                    .text
                    .split_once(changed.as_str())
                    .unwrap_or((&line.text, ""));
                row.push(span(before.to_owned(), role));
                row.push(inverse(changed.clone(), role));
                row.push(span(after.to_owned(), role));
            }
            None => row.push(span(line.text.clone(), role)),
        }
        rows.push(clip(row, LANE + text));
    }
}

const FOLDED_LANES: usize = 6;

fn agent_rows(lanes: &[Lane], text: usize, rows: &mut Vec<Row>) {
    let count = |state: State| lanes.iter().filter(|lane| lane.state == state).count();
    let mut tally = Vec::new();
    for state in [State::Running, State::NeedsYou, State::Done, State::Failed] {
        let number = count(state);
        if number > 0 {
            if !tally.is_empty() {
                tally.push(span("  ", Role::Text));
            }
            tally.push(span(format!("{} {number}", state.glyph()), state.role()));
        }
    }
    let noun = if lanes.len() == 1 { "agent" } else { "agents" };
    let mut header = vec![
        lane("●", State::Done.role()),
        span(format!("{:<VERB$}", "Spawned"), Role::Text),
        span(format!("{} {noun}", lanes.len()), Role::Muted),
    ];
    if !tally.is_empty() {
        header.push(span("   ", Role::Text));
        header.extend(tally);
    }
    rows.push(header);
    let urgency = |state: State| match state {
        State::NeedsYou => 0,
        State::Failed => 1,
        State::Running => 2,
        State::Done => 3,
        State::Idle => 4,
    };
    let mut shown: Vec<&Lane> = lanes.iter().collect();
    let folded = shown.len() > FOLDED_LANES;
    if folded {
        shown.sort_by_key(|lane| urgency(lane.state));
        shown.truncate(FOLDED_LANES);
    }
    let title_width = shown
        .iter()
        .map(|lane| lane.title.width())
        .max()
        .unwrap_or(0);
    let room = text.saturating_sub(2 + title_width + 3);
    for (index, entry) in shown.iter().enumerate() {
        let connector = if index + 1 == shown.len() && !folded {
            "└"
        } else {
            "├"
        };
        rows.push(vec![
            lane(connector, Role::Faint),
            span(format!("{} ", entry.state.glyph()), entry.state.row_role()),
            span(format!("{:<title_width$}   ", entry.title), Role::Text),
            span(fit(&entry.activity, room), Role::Muted),
        ]);
    }
    if folded {
        rows.push(vec![
            lane("└", Role::Faint),
            span(format!("+{} more", lanes.len() - FOLDED_LANES), Role::Muted),
            span("   ^T all agents", Role::Faint),
        ]);
    }
}

fn column_rows(sessions: &[SessionRow], width: usize, wide: bool) -> Vec<Row> {
    if width == 0 {
        return Vec::new();
    }
    let groups = [
        ("NEEDS YOU", [State::NeedsYou, State::Failed]),
        ("WORKING", [State::Running, State::Running]),
        ("RECENT", [State::Idle, State::Done]),
    ];
    let mut rows = Vec::new();
    let mut number = 0;
    for (label, states) in groups {
        let members: Vec<&SessionRow> = sessions
            .iter()
            .filter(|session| states.contains(&session.state))
            .collect();
        if members.is_empty() {
            continue;
        }
        if !rows.is_empty() {
            rows.push(Vec::new());
        }
        rows.push(vec![blank_lane(), span(label, Role::Faint)]);
        for session in members {
            number += 1;
            let marker = if session.focused {
                lane("▎", Role::Accent)
            } else {
                blank_lane()
            };
            let digit = if number <= 9 {
                format!("{number} ")
            } else {
                "  ".to_owned()
            };
            let title_role = if session.focused {
                Role::Text
            } else {
                Role::Muted
            };
            let badge = if session.agents > 0 {
                format!("  ↳{}", session.agents)
            } else {
                String::new()
            };
            let room = width.saturating_sub(6 + badge.width());
            rows.push(vec![
                marker,
                span(digit, Role::Faint),
                span(
                    format!("{} ", session.state.glyph()),
                    session.state.row_role(),
                ),
                span(fit(&session.title, room), title_role),
                span(badge, Role::Faint),
            ]);
            if wide && !session.activity.is_empty() {
                let indent = span(" ".repeat(6), Role::Text);
                let cost = span(session.cost.clone(), Role::Faint);
                let room = width.saturating_sub(6 + session.cost.width() + 2);
                let activity = span(fit(&session.activity, room), Role::Faint);
                rows.push(right_aligned(vec![indent, activity], vec![cost], width));
            }
        }
    }
    rows
}

fn status_row(dock: &Dock, width: usize, compact: bool) -> Row {
    let mut parts = vec![dock.model.clone()];
    if !compact {
        parts.push(dock.context.clone());
        parts.push(dock.cost.clone());
    }
    if let Some(mode) = dock.mode.label() {
        parts.push(mode.to_owned());
    }
    parts.retain(|part| !part.is_empty());
    let mut left = vec![blank_lane(), span(dock.title.clone(), Role::Text)];
    for part in parts {
        left.push(span(" · ", Role::Faint));
        left.push(span(part, Role::Muted));
    }
    let right = vec![span(dock.place.clone(), Role::Faint)];
    if compact || width_of(&left) + width_of(&right) + 2 > width {
        return clip(left, width);
    }
    right_aligned(left, right, width)
}

fn rule_row(dock: &Dock, width: usize, column: bool) -> Row {
    let role = dock.state.role();
    let mut left = Vec::new();
    if dock.state != State::Idle || !dock.activity.is_empty() {
        left.push(lane(dock.state.glyph(), role));
    }
    if !dock.activity.is_empty() {
        left.push(span(format!("{} ", dock.activity), Role::Text));
    }
    let attention = if column || dock.compact_attention.is_empty() {
        &dock.attention
    } else {
        &dock.compact_attention
    };
    let mut right = Vec::new();
    if !attention.is_empty() {
        let attention_role = if attention.contains(State::NeedsYou.glyph()) {
            Role::Warning
        } else {
            Role::Faint
        };
        right.push(span(format!(" {attention}"), attention_role));
    }
    let fill = width.saturating_sub(width_of(&left) + width_of(&right));
    left.push(span("─".repeat(fill), role));
    left.extend(right);
    left
}

fn dock_rows(dock: &Dock, width: usize, column: bool) -> Vec<Row> {
    let mut rows = vec![rule_row(dock, width, column)];
    for row in &dock.panel {
        rows.push(clip(row.clone(), width));
    }
    for draft in &dock.queued {
        for line in wrap(draft, width - LANE).into_iter().take(2) {
            rows.push(vec![lane("┋", Role::Faint), span(line, Role::Muted)]);
        }
    }
    let rail = lane("┃", dock.mode.rail_role());
    match &dock.composer {
        Composer::Placeholder => rows.push(vec![rail, span("Ask QQ…", Role::Faint)]),
        Composer::Draft(text) => {
            for line in wrap(text, width - LANE) {
                rows.push(vec![rail.clone(), span(line, Role::Text)]);
            }
            if let Some(last) = rows.last_mut() {
                last.push(span("▍", Role::Accent));
            }
        }
        Composer::Keys(keys) => {
            let mut row = vec![rail];
            row.extend(keys_row(keys));
            rows.push(clip(row, width));
        }
    }
    if dock.hints.is_empty() {
        rows.push(status_row(dock, width, !column && width < REGULAR));
    } else {
        let mut row = vec![blank_lane()];
        row.extend(keys_row(&dock.hints));
        rows.push(clip(row, width));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_has_its_own_glyph() {
        for (index, state) in State::ALL.iter().enumerate() {
            for other in &State::ALL[index + 1..] {
                assert_ne!(state.glyph(), other.glyph(), "{state:?} and {other:?}");
            }
        }
    }

    #[test]
    fn rule_colors_follow_the_focused_state() {
        assert_eq!(State::Idle.role(), Role::Faint);
        assert_eq!(State::Running.role(), Role::Info);
        assert_eq!(State::NeedsYou.role(), Role::Warning);
        assert_eq!(State::Failed.role(), Role::Error);
    }

    #[test]
    fn composer_rail_color_follows_the_mode() {
        assert_eq!(Mode::ReadOnly.rail_role(), Role::Faint);
        assert_eq!(Mode::Ask.rail_role(), Role::Accent);
        assert_eq!(Mode::Auto.rail_role(), Role::Accent);
        assert_eq!(Mode::Full.rail_role(), Role::Warning);
    }

    #[test]
    fn faint_sits_between_muted_and_the_background() {
        let palette = Palette::derive([
            Color::Rgb {
                r: 216,
                g: 222,
                b: 233,
            },
            Color::Rgb {
                r: 123,
                g: 132,
                b: 151,
            },
            Color::Rgb {
                r: 143,
                g: 184,
                b: 232,
            },
            Color::Rgb {
                r: 224,
                g: 160,
                b: 113,
            },
            Color::Rgb {
                r: 230,
                g: 192,
                b: 123,
            },
            Color::Rgb {
                r: 236,
                g: 123,
                b: 141,
            },
            Color::Rgb {
                r: 143,
                g: 211,
                b: 166,
            },
            Color::Rgb {
                r: 32,
                g: 36,
                b: 44,
            },
        ]);
        assert_eq!(
            faint(&palette),
            Color::Rgb {
                r: 69,
                g: 75,
                b: 86
            }
        );
        assert_eq!(faint(&Palette::TERMINAL), Color::DarkGrey);
    }

    #[test]
    fn ansi_frames_end_on_their_last_row_without_scrolling() {
        let frame = render(&DesignScene::GoldenPath.scene(), (80, 24));
        let bytes = frame.ansi(&Palette::TERMINAL);
        let text = String::from_utf8(bytes).expect("utf-8");
        assert_eq!(text.matches("\r\n").count(), 23);
        assert!(!text.ends_with('\n'));
    }

    #[test]
    fn long_rows_are_clipped_to_the_width() {
        let row = clip(vec![span("abcdef", Role::Text), span("ghi", Role::Text)], 4);
        assert_eq!(width_of(&row), 4);
    }
}
