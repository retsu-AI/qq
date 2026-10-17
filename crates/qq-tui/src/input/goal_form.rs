use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use qq_protocol::{CommandId, GoalBudget, GoalIdentity, SessionId, WorkspaceId};

use super::PickerOutcome;
use crate::{app::GoalDefaults, composer::Composer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GoalFormRefresh {
    Ready,
    Pending,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GoalForm {
    pub session: SessionId,
    pub workspace: WorkspaceId,
    pub expected: Option<GoalIdentity>,
    pub previous_objective: Option<String>,
    pub fields: [Composer; 2],
    pub field: usize,
    pub scroll: isize,
    pub defaults: GoalDefaults,
    pub original_budget: GoalBudget,
    pub check_timeout_ms: u64,
    pub preset: usize,
    pub replace: bool,
    pub confirm: bool,
    pub sending: Option<CommandId>,
    pub refresh: GoalFormRefresh,
    pub error: Option<String>,
}

impl GoalForm {
    pub fn budget(&self, now: u64) -> GoalBudget {
        let mut budget = self.original_budget.clone();
        let (duration, runs) = match self.preset {
            0 => return budget,
            1 => (self.defaults.duration_ms, self.defaults.default_runs),
            2 => (
                self.defaults.duration_ms.min(3_600_000),
                self.defaults.default_runs.min(20),
            ),
            _ => (self.defaults.max_duration_ms, self.defaults.max_runs),
        };
        budget.max_goal_runs = runs.min(self.defaults.max_runs);
        budget.deadline_ms = now.saturating_add(duration.min(self.defaults.max_duration_ms));
        budget
    }

    pub fn preset_label(&self) -> &'static str {
        match self.preset {
            0 => "Keep existing deadline and limits",
            1 => "Configured defaults",
            2 => "Focused (up to 1 hour / 20 runs)",
            _ => "Extended (workspace maximum)",
        }
    }

    pub fn push_text(&mut self, text: &str) -> bool {
        if self.confirm || self.sending.is_some() || self.field >= 2 {
            return false;
        }
        let limit = if self.field == 0 { 2048 } else { 1024 };
        if self.fields[self.field]
            .text
            .len()
            .saturating_add(text.len())
            > limit
        {
            self.error = Some(format!(
                "This field is limited to {limit} bytes; paste was not inserted."
            ));
            return true;
        }
        self.scroll = 0;
        self.fields[self.field].insert_str(text);
        self.error = None;
        !text.is_empty()
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PickerOutcome {
        if key.code == KeyCode::Esc {
            return PickerOutcome::Cancel;
        }
        if self.sending.is_some() {
            return PickerOutcome::Ignored;
        }
        match key.code {
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(10);
                return PickerOutcome::Changed;
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(10);
                return PickerOutcome::Changed;
            }
            _ => {}
        }
        if self.confirm {
            return match key.code {
                KeyCode::Char('y') if key.modifiers.is_empty() => PickerOutcome::Accept,
                KeyCode::Char('n') if key.modifiers.is_empty() => {
                    self.confirm = false;
                    PickerOutcome::Changed
                }
                _ => PickerOutcome::Ignored,
            };
        }
        match key.code {
            KeyCode::Enter => return PickerOutcome::Accept,
            KeyCode::Tab | KeyCode::Down => {
                self.field = (self.field + 1) % 3;
                self.scroll = 0;
                return PickerOutcome::Changed;
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.field = (self.field + 2) % 3;
                self.scroll = 0;
                return PickerOutcome::Changed;
            }
            KeyCode::Left | KeyCode::Right if self.field == 2 => {
                let first = usize::from(self.replace || self.expected.is_none());
                self.preset = if key.code == KeyCode::Right {
                    if self.preset == 3 {
                        first
                    } else {
                        self.preset + 1
                    }
                } else if self.preset == first {
                    3
                } else {
                    self.preset - 1
                };
                return PickerOutcome::Changed;
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let mut bytes = [0; 4];
                return if self.push_text(ch.encode_utf8(&mut bytes)) {
                    PickerOutcome::Changed
                } else {
                    PickerOutcome::Ignored
                };
            }
            _ => {}
        }
        let Some(editor) = self.fields.get_mut(self.field) else {
            return PickerOutcome::Ignored;
        };
        let changed = match key.code {
            KeyCode::Backspace => editor.backspace(),
            KeyCode::Delete => editor.delete(),
            KeyCode::Left => editor.move_left(),
            KeyCode::Right => editor.move_right(),
            KeyCode::Home => editor.move_line_start(),
            KeyCode::End => editor.move_line_end(),
            KeyCode::Char('z') if key.modifiers == KeyModifiers::CONTROL => editor.undo(),
            _ => false,
        };
        if changed {
            self.scroll = 0;
            PickerOutcome::Changed
        } else {
            PickerOutcome::Ignored
        }
    }
}
