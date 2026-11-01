mod goal_form;
pub(crate) use goal_form::{GoalForm, GoalFormRefresh};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use qq_protocol::{AgentProfileId, ApprovalMode, GuidanceKind, ReasoningEffort, SessionId};

use crate::{
    commands::{Command, CommandSpec},
    picker::{Picker, PickerItem},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionConfirm {
    Delete(SessionId),
    Prune,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelRow {
    pub index: Option<usize>,
    pub provider: String,
    pub model: String,
    pub name: Option<String>,
}

impl PickerItem for ModelRow {
    fn search_text<'a>(&'a self, out: &mut Vec<&'a str>) {
        out.push(&self.provider);
        out.push(&self.model);
        if let Some(name) = &self.name {
            out.push(name);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProfileRow {
    pub id: AgentProfileId,
    pub model: Option<String>,
    pub approval_mode: ApprovalMode,
    pub pack: Option<String>,
}

impl PickerItem for ProfileRow {
    fn search_text<'a>(&'a self, out: &mut Vec<&'a str>) {
        out.push(self.id.as_str());
        if let Some(model) = &self.model {
            out.push(model);
        }
        if let Some(pack) = &self.pack {
            out.push(pack);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ApprovalModeRow {
    pub mode: ApprovalMode,
    pub label: &'static str,
    pub summary: &'static str,
}

impl PickerItem for ApprovalModeRow {
    fn search_text<'a>(&'a self, out: &mut Vec<&'a str>) {
        out.push(self.label);
        out.push(self.summary);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EffortRow {
    pub effort: Option<ReasoningEffort>,
    pub label: &'static str,
    pub summary: &'static str,
}

impl PickerItem for EffortRow {
    fn search_text<'a>(&'a self, out: &mut Vec<&'a str>) {
        out.push(self.label);
        out.push(self.summary);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillRow {
    pub name: String,
    pub kind: GuidanceKind,
    pub source: String,
    pub description: String,
    pub disclosed: bool,
}

impl PickerItem for SkillRow {
    fn search_text<'a>(&'a self, out: &mut Vec<&'a str>) {
        out.push(&self.name);
        out.push(&self.source);
        out.push(&self.description);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ThemeRow {
    pub index: usize,
    pub name: String,
}

impl PickerItem for ThemeRow {
    fn search_text<'a>(&'a self, out: &mut Vec<&'a str>) {
        out.push(&self.name);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionRow {
    pub id: SessionId,
    pub title: String,
}

impl PickerItem for SessionRow {
    fn search_text<'a>(&'a self, out: &mut Vec<&'a str>) {
        out.push(&self.title);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistoryRow {
    pub text: String,
}

impl PickerItem for HistoryRow {
    fn search_text<'a>(&'a self, out: &mut Vec<&'a str>) {
        out.push(&self.text);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CommandRow {
    pub spec: &'static CommandSpec,
}

impl PickerItem for CommandRow {
    fn search_text<'a>(&'a self, out: &mut Vec<&'a str>) {
        out.push(self.spec.title);
        out.extend(self.spec.slash.iter().copied());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GoalCheckTarget {
    pub session_id: SessionId,
    pub expected: qq_protocol::GoalIdentity,
    pub workspace_id: qq_protocol::WorkspaceId,
    pub workspace_path: String,
    pub command: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GoalCheckRow {
    pub scope: Option<qq_protocol::GoalCheckScope>,
    pub label: &'static str,
    pub description: &'static str,
}

impl PickerItem for GoalCheckRow {
    fn search_text<'a>(&'a self, out: &mut Vec<&'a str>) {
        out.push(self.label);
        out.push(self.description);
    }
}

pub(crate) const GOAL_CHECK_PAGE_ROWS: usize = 1;

pub(crate) const GOAL_CHECK_ROWS: [GoalCheckRow; 3] = [
    GoalCheckRow {
        scope: None,
        label: "deny",
        description: "withhold consent; pause an active goal",
    },
    GoalCheckRow {
        scope: Some(qq_protocol::GoalCheckScope::Once),
        label: "allow once",
        description: "one start; a failure does not refund it",
    },
    GoalCheckRow {
        scope: Some(qq_protocol::GoalCheckScope::ForGoal),
        label: "allow repeated checks",
        description: "this goal and this exact command only",
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Overlay {
    GoalForm(Box<GoalForm>),
    GoalCheck {
        target: GoalCheckTarget,
        picker: Picker<GoalCheckRow>,
        detail_row: usize,
    },
    Models(Picker<ModelRow>),
    Profiles(Picker<ProfileRow>),
    ApprovalModes(Picker<ApprovalModeRow>),
    Effort(Picker<EffortRow>),
    Skills(Picker<SkillRow>),
    Themes {
        picker: Picker<ThemeRow>,
        restore: usize,
    },
    Sessions {
        picker: Picker<SessionRow>,
        scope: Option<SessionId>,
        confirm: Option<SessionConfirm>,
    },
    Commands {
        picker: Picker<CommandRow>,
        help: bool,
    },
    History(Picker<HistoryRow>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    GoalForm,
    GoalCheck,
    Models,
    Profiles,
    ApprovalModes,
    Effort,
    Skills,
    Themes,
    Sessions,
    Commands,
    History,
    Approval,
    Compose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickerOutcome {
    Changed,
    Ignored,
    Cancel,
    Accept,
    Chord(KeyCode),
}

impl Overlay {
    #[cfg(test)]
    pub(crate) fn sessions(
        rows: Vec<SessionRow>,
        query: &str,
        selected: Option<SessionId>,
        confirm: Option<SessionConfirm>,
    ) -> Self {
        let mut picker = Picker::with_items(rows);
        picker.push_query(query);
        if let Some(selected) = selected
            && let Some(index) = picker.items().iter().position(|row| row.id == selected)
        {
            picker.select_item(index);
        }
        Self::Sessions {
            picker,
            scope: None,
            confirm,
        }
    }

    #[cfg(test)]
    pub(crate) fn models(rows: Vec<ModelRow>) -> Self {
        Self::Models(Picker::with_items(rows))
    }

    #[cfg(test)]
    pub(crate) fn set_confirm(&mut self, next: Option<SessionConfirm>) {
        if let Self::Sessions { confirm, .. } = self {
            *confirm = next;
        }
    }

    #[must_use]
    pub(crate) fn mode(&self) -> Mode {
        match self {
            Self::GoalForm(_) => Mode::GoalForm,
            Self::GoalCheck { .. } => Mode::GoalCheck,
            Self::Models(_) => Mode::Models,
            Self::Profiles(_) => Mode::Profiles,
            Self::ApprovalModes(_) => Mode::ApprovalModes,
            Self::Effort(_) => Mode::Effort,
            Self::Skills(_) => Mode::Skills,
            Self::Themes { .. } => Mode::Themes,
            Self::Sessions { .. } => Mode::Sessions,
            Self::Commands { .. } => Mode::Commands,
            Self::History(_) => Mode::History,
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> PickerOutcome {
        fn dispatch<T: PickerItem>(picker: &mut Picker<T>, key: KeyEvent) -> PickerOutcome {
            match key.code {
                KeyCode::Esc => PickerOutcome::Cancel,
                KeyCode::Enter => PickerOutcome::Accept,
                KeyCode::Up => changed(picker.move_up()),
                KeyCode::Down => changed(picker.move_down()),
                KeyCode::Backspace => changed(picker.pop_query()),
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    let mut encoded = [0; 4];
                    changed(picker.push_query(character.encode_utf8(&mut encoded)))
                }
                KeyCode::Char(_) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    PickerOutcome::Chord(key.code)
                }
                KeyCode::Delete => PickerOutcome::Chord(key.code),
                _ => PickerOutcome::Ignored,
            }
        }
        fn changed(changed: bool) -> PickerOutcome {
            if changed {
                PickerOutcome::Changed
            } else {
                PickerOutcome::Ignored
            }
        }
        match self {
            Self::GoalForm(form) => form.handle_key(key),
            Self::GoalCheck {
                picker, detail_row, ..
            } => match key.code {
                KeyCode::PageUp => {
                    *detail_row = detail_row.saturating_sub(GOAL_CHECK_PAGE_ROWS);
                    PickerOutcome::Changed
                }
                KeyCode::PageDown => {
                    *detail_row = detail_row.saturating_add(GOAL_CHECK_PAGE_ROWS);
                    PickerOutcome::Changed
                }
                _ => dispatch(picker, key),
            },
            Self::Models(picker) => dispatch(picker, key),
            Self::Profiles(picker) => dispatch(picker, key),
            Self::ApprovalModes(picker) => dispatch(picker, key),
            Self::Effort(picker) => dispatch(picker, key),
            Self::Skills(picker) => dispatch(picker, key),
            Self::Themes { picker, .. } => dispatch(picker, key),
            Self::Sessions { picker, .. } => dispatch(picker, key),
            Self::Commands { picker, .. } => dispatch(picker, key),
            Self::History(picker) => dispatch(picker, key),
        }
    }

    pub(crate) fn push_query(&mut self, text: &str) -> bool {
        match self {
            Self::GoalForm(form) => form.push_text(text),
            Self::GoalCheck { picker, .. } => picker.push_query(text),
            Self::Models(picker) => picker.push_query(text),
            Self::Profiles(picker) => picker.push_query(text),
            Self::ApprovalModes(picker) => picker.push_query(text),
            Self::Effort(picker) => picker.push_query(text),
            Self::Skills(picker) => picker.push_query(text),
            Self::Themes { picker, .. } => picker.push_query(text),
            Self::Sessions { picker, .. } => picker.push_query(text),
            Self::Commands { picker, .. } => picker.push_query(text),
            Self::History(picker) => picker.push_query(text),
        }
    }
}

pub(crate) fn command_rows() -> Vec<CommandRow> {
    crate::commands::COMMANDS
        .iter()
        .filter(|spec| spec.command != Command::OpenCommands && spec.command != Command::OpenHelp)
        .map(|spec| CommandRow { spec })
        .collect()
}

pub(crate) const fn approval_mode_label(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::ReadOnly => "read_only",
        ApprovalMode::Ask => "ask",
        ApprovalMode::Auto => "auto",
        ApprovalMode::Full => "full",
    }
}

pub(crate) const fn approval_mode_row(mode: ApprovalMode) -> ApprovalModeRow {
    let (label, summary) = match mode {
        ApprovalMode::ReadOnly => (
            "read_only",
            "deny edits, shell, and external tools without asking",
        ),
        ApprovalMode::Ask => (
            "ask",
            "require your approval for every non-read action, every time",
        ),
        ApprovalMode::Auto => (
            "auto",
            "run in the workspace without asking; fixed stops are denied",
        ),
        ApprovalMode::Full => (
            "full",
            "run everything without asking, fixed stops included",
        ),
    };
    ApprovalModeRow {
        mode,
        label,
        summary,
    }
}

pub(crate) const fn effort_label(effort: Option<ReasoningEffort>) -> &'static str {
    match effort {
        None => "configured",
        Some(effort) => effort.as_str(),
    }
}

pub(crate) const fn effort_row(effort: Option<ReasoningEffort>) -> EffortRow {
    let summary = match effort {
        None => "use the configured or profile effort; omit the field",
        Some(ReasoningEffort::Default) => "use the provider default, overriding configured effort",
        Some(ReasoningEffort::None) => "disable reasoning; distinct from omission",
        Some(ReasoningEffort::Minimal) => "lowest reasoning spend the model accepts",
        Some(ReasoningEffort::Low) => "light reasoning",
        Some(ReasoningEffort::Medium) => "balanced reasoning",
        Some(ReasoningEffort::High) => "heavier reasoning",
        Some(ReasoningEffort::Xhigh) => "extra-high reasoning spend",
        Some(ReasoningEffort::Max) => "maximum reasoning effort",
    };
    EffortRow {
        effort,
        label: effort_label(effort),
        summary,
    }
}
