use crossterm::style::Color;
use unicode_width::UnicodeWidthStr;

use crate::theme::Palette;

mod scenes;

pub use scenes::DesignScene;

const MEASURE: usize = 100;
const COLUMN_WIDTH: usize = 30;
const COLUMN_GUTTER: usize = 3;
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
    pub hints: Option<Row>,
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
        out.push_str("\r\n");
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
    let column = width >= REGULAR && scene.sessions.len() >= 2;
    let wide = width >= WIDE;
    let column_width = if column { COLUMN_WIDTH } else { 0 };
    let transcript_area = width - column_width - if column { COLUMN_GUTTER } else { 0 };
    let content = transcript_area.saturating_sub(2).min(MEASURE);
    let inset = if transcript_area >= 200 {
        (transcript_area - content) / 3
    } else {
        2
    };
    let dock = dock_rows(&scene.dock, width, height, column);
    let body_height = height - dock.len();
    let mut transcript = transcript_rows(&scene.blocks, content);
    if transcript.len() > body_height {
        transcript.drain(..transcript.len() - body_height);
    }
    let column_rows = if column {
        column_rows(&scene.sessions, column_width, wide)
    } else {
        Vec::new()
    };
    let mut rows = Vec::with_capacity(height);
    for index in 0..body_height {
        let mut row = vec![span(" ".repeat(inset), Role::Text)];
        let line = transcript.get(index).cloned().unwrap_or_default();
        let used = inset + width_of(&line);
        row.extend(line);
        if column {
            let start = transcript_area + COLUMN_GUTTER;
            row.push(span(" ".repeat(start.saturating_sub(used)), Role::Text));
            if let Some(entry) = column_rows.get(index) {
                row.extend(entry.iter().cloned());
            }
        }
        rows.push(clip(row, width));
    }
    rows.extend(dock.into_iter().map(|row| clip(row, width)));
    Frame { rows, width }
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
        let mut kept = String::new();
        for character in span.text.chars() {
            let character_width =
                UnicodeWidthStr::width(character.encode_utf8(&mut [0; 4]) as &str);
            if used + character_width > width {
                break;
            }
            used += character_width;
            kept.push(character);
        }
        span.text = kept;
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

fn transcript_rows(blocks: &[Block], width: usize) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let mut previous_was_tool = false;
    for block in blocks {
        let is_tool = matches!(
            block,
            Block::Tool(_) | Block::Agents { .. } | Block::Receipt(_)
        );
        if !rows.is_empty() && !(is_tool && previous_was_tool) {
            rows.push(Vec::new());
        }
        previous_was_tool = is_tool;
        match block {
            Block::Banner(lines) | Block::Raw(lines) => rows.extend(lines.iter().cloned()),
            Block::Prompt(text) => {
                for line in wrap(text, width.saturating_sub(2)) {
                    rows.push(vec![span("┃ ", Role::Accent), bold(line, Role::Text)]);
                }
            }
            Block::Answer { text, streaming } => {
                let mut lines = wrap(text, width.saturating_sub(2));
                let last = lines.len() - 1;
                for (index, line) in lines.drain(..).enumerate() {
                    let mut row = vec![span(line, Role::Text)];
                    if *streaming && index == last {
                        row.push(span("▍", Role::Accent));
                    }
                    rows.push(row);
                }
            }
            Block::Error(text) => {
                rows.push(vec![
                    span("✕ ", Role::Error),
                    span(text.clone(), Role::Text),
                ]);
            }
            Block::Reasoning(elapsed) => {
                rows.push(vec![span(format!("∴ {elapsed}"), Role::Faint)]);
            }
            Block::Code { language, lines } => {
                let fill_width = width.saturating_sub(2);
                let panel = |text: String, role: Role| {
                    let pad = fill_width.saturating_sub(text.width());
                    vec![Span {
                        fill: true,
                        ..span(format!("  {text}{}", " ".repeat(pad)), role)
                    }]
                };
                let label = format!("{:>width$}", language, width = fill_width.saturating_sub(2));
                rows.push(panel(label, Role::Faint));
                for line in lines {
                    rows.push(panel(line.clone(), Role::Text));
                }
                rows.push(panel(String::new(), Role::Text));
            }
            Block::Tool(tool) => tool_rows(tool, width, &mut rows),
            Block::Agents { lanes } => agent_rows(lanes, width, &mut rows),
            Block::Receipt(text) => {
                rows.push(vec![
                    span("● ", State::Done.role()),
                    span(text.clone(), Role::Muted),
                ]);
            }
        }
    }
    rows
}

fn tool_rows(tool: &Tool, width: usize, rows: &mut Vec<Row>) {
    let left = vec![
        span(format!("{} ", tool.state.glyph()), tool.state.row_role()),
        span(format!("{:<8} ", tool.verb), Role::Text),
        span(tool.subject.clone(), Role::Muted),
    ];
    let metric_role = if tool.state == State::Failed {
        Role::Error
    } else {
        Role::Faint
    };
    rows.push(right_aligned(
        left,
        vec![span(tool.metric.clone(), metric_role)],
        width,
    ));
    for line in &tool.tail {
        rows.push(vec![
            span("│ ", Role::Faint),
            span(line.clone(), Role::Muted),
        ]);
    }
    for line in &tool.diff {
        let (role, sign) = match line.sign {
            '+' => (Role::Add, "+"),
            '-' => (Role::Remove, "-"),
            _ => (Role::Text, " "),
        };
        let mut row = vec![
            span("│ ", Role::Faint),
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
        rows.push(row);
    }
}

fn agent_rows(lanes: &[Lane], width: usize, rows: &mut Vec<Row>) {
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
    let header = vec![
        span("● ", State::Done.role()),
        span(format!("{:<8} ", "Spawned"), Role::Text),
        span(format!("{} {noun}", lanes.len()), Role::Muted),
    ];
    rows.push(right_aligned(header, tally, width));
    let urgency = |state: State| match state {
        State::NeedsYou => 0,
        State::Failed => 1,
        State::Running => 2,
        State::Done => 3,
        State::Idle => 4,
    };
    let mut shown: Vec<&Lane> = lanes.iter().collect();
    let folded = shown.len() > 6;
    if folded {
        shown.sort_by_key(|lane| urgency(lane.state));
        shown.truncate(6);
    }
    let title_width = shown
        .iter()
        .map(|lane| lane.title.width())
        .max()
        .unwrap_or(0);
    for (index, lane) in shown.iter().enumerate() {
        let connector = if index + 1 == shown.len() && !folded {
            "└ "
        } else {
            "├ "
        };
        rows.push(vec![
            span(connector, Role::Faint),
            span(format!("{} ", lane.state.glyph()), lane.state.row_role()),
            span(format!("{:<title_width$}   ", lane.title), Role::Text),
            span(lane.activity.clone(), Role::Muted),
        ]);
    }
    if folded {
        rows.push(vec![
            span("└ ", Role::Faint),
            span(format!("+{} more  ", lanes.len() - 6), Role::Muted),
            span("^T", Role::Faint),
        ]);
    }
}

fn column_rows(sessions: &[SessionRow], width: usize, wide: bool) -> Vec<Row> {
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
        rows.push(vec![span(format!("  {label}"), Role::Faint)]);
        for session in members {
            number += 1;
            let marker = if session.focused {
                span("▎ ", Role::Accent)
            } else {
                span("  ", Role::Text)
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
            let mut row = vec![
                marker,
                span(digit, Role::Faint),
                span(
                    format!("{} ", session.state.glyph()),
                    session.state.row_role(),
                ),
                span(session.title.clone(), title_role),
            ];
            if session.agents > 0 {
                row.push(span(format!("  ↳{}", session.agents), Role::Faint));
            }
            rows.push(clip(row, width));
            if wide && !session.activity.is_empty() {
                let mut detail = session.activity.clone();
                if !session.cost.is_empty() {
                    detail.push_str(" · ");
                    detail.push_str(&session.cost);
                }
                rows.push(clip(
                    vec![span(format!("      {detail}"), Role::Faint)],
                    width,
                ));
            }
        }
    }
    rows
}

fn dock_rows(dock: &Dock, width: usize, height: usize, column: bool) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut panel = dock.panel.clone();
    let cap = height / 2;
    if panel.len() > cap {
        panel.truncate(cap);
    }
    let rule_role = dock.state.role();
    let left_text = if dock.activity.is_empty() {
        if dock.state == State::Idle {
            String::new()
        } else {
            format!("{} ", dock.state.glyph())
        }
    } else {
        format!("{} {} ", dock.state.glyph(), dock.activity)
    };
    let attention = if column || dock.compact_attention.is_empty() {
        &dock.attention
    } else {
        &dock.compact_attention
    };
    let right_text = if attention.is_empty() {
        String::new()
    } else {
        format!(" {attention}")
    };
    let fill = (width - 1).saturating_sub(1 + left_text.width() + right_text.width());
    rows.push(vec![
        span(" ─", rule_role),
        span(left_text, rule_role),
        span("─".repeat(fill), rule_role),
        span(right_text, Role::Warning),
    ]);
    for row in panel {
        let mut indented = vec![span("   ", Role::Text)];
        indented.extend(row);
        rows.push(indented);
    }
    for draft in &dock.queued {
        for line in wrap(draft, width.saturating_sub(4)).into_iter().take(2) {
            rows.push(vec![span(" ┋ ", Role::Faint), span(line, Role::Muted)]);
        }
    }
    let rail = span(" ┃ ", dock.mode.rail_role());
    match &dock.composer {
        Composer::Placeholder => rows.push(vec![rail, span("Ask QQ…", Role::Faint)]),
        Composer::Draft(text) => {
            for line in wrap(text, width.saturating_sub(4)) {
                rows.push(vec![rail.clone(), span(line, Role::Text)]);
            }
            if let Some(last) = rows.last_mut() {
                last.push(span("▍", Role::Accent));
            }
        }
        Composer::Keys(keys) => {
            let mut row = vec![rail];
            for (index, (key, label)) in keys.iter().enumerate() {
                if index > 0 {
                    row.push(span("   ", Role::Text));
                }
                row.push(bold(key.clone(), Role::Text));
                row.push(span(format!(" {label}"), Role::Muted));
            }
            rows.push(row);
        }
    }
    if let Some(hints) = &dock.hints {
        let mut row = vec![span("   ", Role::Text)];
        row.extend(hints.iter().cloned());
        rows.push(row);
        return rows;
    }
    let mut status = vec![dock.title.clone()];
    for part in [&dock.model, &dock.context, &dock.cost] {
        if !part.is_empty() && width >= REGULAR || part == &dock.model {
            status.push(part.clone());
        }
    }
    if let Some(mode) = dock.mode.label() {
        status.push(mode.to_owned());
    }
    status.retain(|part| !part.is_empty());
    let left = vec![span(format!("   {}", status.join(" · ")), Role::Muted)];
    let right = if width >= REGULAR && !dock.place.is_empty() {
        vec![span(format!("{} ", dock.place), Role::Faint)]
    } else {
        Vec::new()
    };
    rows.push(right_aligned(left, right, width));
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
    fn long_rows_are_clipped_to_the_width() {
        let row = clip(vec![span("abcdef", Role::Text), span("ghi", Role::Text)], 4);
        assert_eq!(width_of(&row), 4);
    }
}
