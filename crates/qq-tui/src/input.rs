//! Input modes. Exactly one overlay may be open; it captures keys, paste, and
//! mouse input ahead of the composer and replaces the transcript body while
//! visible. The approval prompt is not an overlay: it is derived from session
//! data (`App::pending_approval`) and modelled as a mode only for dispatch.
//!
//! Every overlay is a [`Picker`] over one item type plus whatever extra state
//! that overlay needs. One key handler ([`Overlay::handle_key`]) moves the
//! cursor and edits the query for all of them; `Enter`, `Esc`, and the
//! overlay-specific chords come back as a [`PickerOutcome`] for `App`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use qq_protocol::{AgentProfileId, ApprovalMode, GuidanceKind, ReasoningEffort, SessionId};

use crate::{
    commands::{Command, CommandSpec},
    picker::{Picker, PickerItem},
};

/// A destructive session-picker action awaiting its inline y/n answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionConfirm {
    Delete(SessionId),
    Prune,
}

/// A row in the model picker: an index into `App::models`, plus the text
/// the query matches so the picker does not need the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelRow {
    /// Index into the authenticated catalog, or `None` for a provider row
    /// that only names a missing credential and cannot be selected.
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

/// A row in the profile picker: one profile the server advertises, copied
/// from the capability document so the picker survives a refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProfileRow {
    pub id: AgentProfileId,
    pub model: Option<String>,
    pub approval_mode: ApprovalMode,
    /// `id@version` of the declaring pack, when there is one.
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

/// A row in the approval-mode picker: one mode the server accepts.
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

/// A row in the effort picker: one pin, or `None` to restore config/profile.
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

/// A row in the skills picker: one indexed command or skill document.
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

/// A row in the theme picker: an index into `App::themes`.
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

/// A row in the session picker. Identity is the id, not the position: the
/// tree reorders when sessions are created or deleted underneath the picker.
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

/// A row in the prompt-history search (`Ctrl-R`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistoryRow {
    pub text: String,
}

impl PickerItem for HistoryRow {
    fn search_text<'a>(&'a self, out: &mut Vec<&'a str>) {
        out.push(&self.text);
    }
}

/// A row in the command palette and help overlay.
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
pub(crate) enum Overlay {
    Models(Picker<ModelRow>),
    Profiles(Picker<ProfileRow>),
    ApprovalModes(Picker<ApprovalModeRow>),
    Effort(Picker<EffortRow>),
    /// The workspace's indexed commands and skills. Enter puts a command in
    /// the composer for its arguments or submits a skill.
    Skills(Picker<SkillRow>),
    /// Theme picker. Moving the cursor previews the highlighted theme live;
    /// `restore` is the theme to put back if the user cancels.
    Themes {
        picker: Picker<ThemeRow>,
        restore: usize,
    },
    Sessions {
        picker: Picker<SessionRow>,
        /// When set, only this session and its descendants are listed: the
        /// `/agents` view of one root's delegated work.
        scope: Option<SessionId>,
        confirm: Option<SessionConfirm>,
    },
    /// Every command with its chord and slash name, filterable. `help` shows
    /// the same list grouped by category with the query hidden.
    Commands {
        picker: Picker<CommandRow>,
        help: bool,
    },
    /// Reverse search over the focused session's prompt history, newest
    /// first. Enter puts the highlighted prompt in the composer.
    History(Picker<HistoryRow>),
}

/// What currently owns keyboard input, from highest to lowest priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
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

/// What a key did to an overlay, for the parts `App` must act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickerOutcome {
    /// The cursor or query changed; the caller redraws (and previews).
    Changed,
    /// Nothing this overlay understands.
    Ignored,
    /// `Esc`: close without choosing.
    Cancel,
    /// `Enter`: accept the highlighted item.
    Accept,
    /// An overlay-specific chord the caller interprets, such as `Ctrl-N` in
    /// the model picker or `Ctrl-D` in the session picker.
    Chord(KeyCode),
}

impl Overlay {
    /// A session picker with the given query, highlight, and confirmation.
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

    /// One key handler for every picker. Navigation and query editing are
    /// applied here; everything else is reported for the caller.
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

    /// Paste into the query.
    pub(crate) fn push_query(&mut self, text: &str) -> bool {
        match self {
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

/// The command palette: every command, in registry order.
pub(crate) fn command_rows() -> Vec<CommandRow> {
    crate::commands::COMMANDS
        .iter()
        .filter(|spec| spec.command != Command::OpenCommands && spec.command != Command::OpenHelp)
        .map(|spec| CommandRow { spec })
        .collect()
}

/// The wire spelling of an approval mode, which is also what `/approval`,
/// the profile picker, and the status row show.
pub(crate) const fn approval_mode_label(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::ReadOnly => "read_only",
        ApprovalMode::Supervised => "supervised",
        ApprovalMode::Ask => "ask",
        ApprovalMode::Auto => "auto",
        ApprovalMode::Full => "full",
    }
}

/// Every approval mode with the one-line meaning the picker shows. Order is
/// least to most permissive.
pub(crate) const fn approval_mode_row(mode: ApprovalMode) -> ApprovalModeRow {
    let (label, summary) = match mode {
        ApprovalMode::ReadOnly => (
            "read_only",
            "deny edits, shell, and external tools without asking",
        ),
        // Never offered by the server for root sessions; rendered for the
        // write children that run under it.
        ApprovalMode::Supervised => (
            "supervised",
            "every edit, shell, and external call is adjudicated by the reviewer model",
        ),
        ApprovalMode::Ask => ("ask", "ask before edits, shell, and external tools"),
        ApprovalMode::Auto => (
            "auto",
            "edits and safe shell run; dangerous shell is held for approval",
        ),
        ApprovalMode::Full => ("full", "every tool call runs without asking"),
    };
    ApprovalModeRow {
        mode,
        label,
        summary,
    }
}

/// The wire spelling of an effort pin, which is also what `/effort` and the
/// status row show. `None` is the unremarkable configured default.
pub(crate) const fn effort_label(effort: Option<ReasoningEffort>) -> &'static str {
    match effort {
        None => "default",
        Some(effort) => effort.as_str(),
    }
}

/// One effort picker row. `None` restores the compiled plan's configured or
/// profile choice.
pub(crate) const fn effort_row(effort: Option<ReasoningEffort>) -> EffortRow {
    let summary = match effort {
        None => "use the configured or profile effort; omit the field",
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
