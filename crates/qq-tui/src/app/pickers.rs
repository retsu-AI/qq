//! Overlay pickers: models, profiles, approval modes, skills, themes,
//! sessions, and the command palette. One
//! key handler in `Overlay` moves the cursor and edits the query; this module
//! owns opening each picker with its rows and interpreting `Accept`,
//! `Cancel`, and picker-specific chords.

use crossterm::event::{KeyCode, KeyEvent};
use qq_protocol::{
    AgentProfileId, ApprovalMode, GuidanceKind, ModelDescriptor, ModelSelection,
    ServerCapabilities, SessionCommand, SessionId, SessionStatus,
};

use super::{App, PendingIntent};
use crate::{
    commands::{Command, SlashAction},
    effect::{Effects, Redraw},
    input::{
        ApprovalModeRow, CommandRow, HistoryRow, ModelRow, Overlay, PickerOutcome, ProfileRow,
        SessionConfirm, SessionRow, SkillRow, ThemeRow, approval_mode_label, approval_mode_row,
        command_rows,
    },
    picker::Picker,
    theme::Theme,
};

impl App {
    /// Route a key to the open overlay. Called only while one is open.
    pub(super) fn handle_overlay_key(&mut self, key: KeyEvent) -> Effects {
        // A pending yes/no question in the session picker captures every key.
        if let Some(confirm) = self.session_picker_confirm() {
            return self.handle_session_picker_confirm_key(key, confirm);
        }
        let Some(overlay) = self.overlay.as_mut() else {
            return Effects::none();
        };
        let outcome = overlay.handle_key(key);
        match (outcome, overlay) {
            (PickerOutcome::Ignored, _) => Effects::none(),
            (PickerOutcome::Changed, Overlay::Themes { picker, .. }) => {
                // Moving the cursor previews the highlighted theme live.
                if let Some(row) = picker.current() {
                    let index = row.index;
                    self.set_theme(index);
                }
                Effects::redraw(Redraw::Immediate)
            }
            (PickerOutcome::Changed, _) => Effects::redraw(Redraw::Immediate),
            (PickerOutcome::Cancel, Overlay::Themes { restore, .. }) => {
                let restore = *restore;
                self.overlay = None;
                self.set_theme(restore);
                Effects::redraw(Redraw::Immediate)
            }
            (PickerOutcome::Cancel, _) => {
                self.overlay = None;
                Effects::redraw(Redraw::Immediate)
            }
            (PickerOutcome::Accept, Overlay::Models(picker)) => {
                let Some(model) = picker.current().map(|row| row.index) else {
                    return Effects::none();
                };
                self.accept_model(model, false)
            }
            (PickerOutcome::Accept, Overlay::Profiles(picker)) => {
                let Some(profile) = picker.current().map(|row| row.id.clone()) else {
                    return Effects::none();
                };
                self.accept_profile(profile)
            }
            (PickerOutcome::Accept, Overlay::ApprovalModes(picker)) => {
                let Some(mode) = picker.current().map(|row| row.mode) else {
                    return Effects::none();
                };
                self.accept_approval_mode(mode)
            }
            (PickerOutcome::Accept, Overlay::Skills(picker)) => {
                let Some((name, kind)) = picker
                    .current()
                    .map(|row| (format!("/{}", row.name), row.kind))
                else {
                    return Effects::none();
                };
                self.overlay = None;
                let action = match kind {
                    GuidanceKind::Command => SlashAction::WorkspaceCommand,
                    GuidanceKind::Skill => SlashAction::Skill,
                };
                self.accept_slash(action, &name)
            }
            (PickerOutcome::Accept, Overlay::Themes { .. }) => {
                let name = self.theme().name.clone();
                self.overlay = None;
                self.set_info(format!(
                    "theme `{name}`; set `theme: \"{name}\"` in tui.ron to keep it"
                ));
                Effects::redraw(Redraw::Immediate)
            }
            (PickerOutcome::Accept, Overlay::Sessions { picker, .. }) => {
                let Some(id) = picker.current().map(|row| row.id) else {
                    return Effects::none();
                };
                self.overlay = None;
                self.focus_session(id)
            }
            (PickerOutcome::Accept, Overlay::History(picker)) => {
                let Some(text) = picker.current().map(|row| row.text.clone()) else {
                    return Effects::none();
                };
                self.overlay = None;
                self.composer.replace(text);
                Effects::redraw(Redraw::Immediate)
            }
            (PickerOutcome::Accept, Overlay::Commands { picker, .. }) => {
                let Some(command) = picker.current().map(|row| row.spec.command) else {
                    return Effects::none();
                };
                self.overlay = None;
                self.execute(command)
            }
            // Ctrl-N in the model picker always creates a session with the
            // highlighted model, even when a session is focused.
            (PickerOutcome::Chord(KeyCode::Char('n' | 'N')), Overlay::Models(picker)) => {
                let Some(model) = picker.current().map(|row| row.index) else {
                    return Effects::none();
                };
                self.accept_model(model, true)
            }
            (
                PickerOutcome::Chord(KeyCode::Delete | KeyCode::Char('d' | 'D')),
                Overlay::Sessions { picker, .. },
            ) => {
                let selected = picker.current().map(|row| row.id);
                self.request_delete_confirmation(selected)
            }
            (PickerOutcome::Chord(_), _) => Effects::none(),
        }
    }

    // --- models ---

    pub(super) fn open_models(&mut self) -> Effects {
        if self.models.is_empty() {
            self.set_warning("no authenticated providers have selectable models".to_owned());
            return Effects::redraw(Redraw::Immediate);
        }
        self.overlay = Some(Overlay::Models(Picker::with_items(self.model_rows())));
        Effects::redraw(Redraw::Immediate)
    }

    fn model_rows(&self) -> Vec<ModelRow> {
        self.models
            .iter()
            .enumerate()
            .map(|(index, option)| ModelRow {
                index,
                provider: option.provider.clone(),
                model: option.model.clone(),
                name: option.name.clone(),
            })
            .collect()
    }

    /// Apply the model at `index`: to the focused session, or by creating a
    /// session when `create` is set or nothing is focused.
    fn accept_model(&mut self, index: usize, create: bool) -> Effects {
        let Some(model) = self
            .models
            .get(index)
            .map(|option| option.selection.clone())
        else {
            return Effects::none();
        };
        let focused = self
            .focused()
            .filter(|session_id| self.sessions.contains_key(session_id));
        let result = match (create, focused) {
            (false, Some(session_id)) => self.set_session_model(session_id, model),
            (true, _) | (false, None) => self.create_session_with_model(None, model),
        };
        if result.requests_anything() {
            self.overlay = None;
        }
        result
    }

    pub(super) fn apply_models(
        &mut self,
        models: Vec<ModelDescriptor>,
        selected_model: Option<ModelSelection>,
    ) {
        self.models = models.into_iter().map(Into::into).collect();
        self.models.sort_by(|left, right| {
            (&left.provider, &left.name, &left.model).cmp(&(
                &right.provider,
                &right.name,
                &right.model,
            ))
        });
        if let Some(selected_model) = selected_model {
            self.model = selected_model;
        }
        for session in self.sessions.values_mut() {
            session.context_window =
                super::model_context_window(&self.models, session.summary.model.as_deref());
        }
        // A refreshed catalog keeps the open picker's cursor on the same model.
        let rows = self.model_rows();
        if let Some(Overlay::Models(picker)) = &mut self.overlay {
            picker.replace_items(rows, |row| (row.provider.clone(), row.model.clone()));
        }
    }

    // --- profiles ---

    pub(crate) fn open_profiles(&mut self) -> Effects {
        let Some(capabilities) = self.capabilities.as_deref() else {
            self.set_warning(
                "profiles are not available until the server's capabilities arrive".to_owned(),
            );
            return Effects::redraw(Redraw::Immediate);
        };
        // The server always lists `default`; an absent list means it answered
        // without a workspace, which this client never asks for.
        let rows = profile_rows(capabilities);
        if rows.is_empty() {
            self.set_warning("the server advertised no profiles for this workspace".to_owned());
            return Effects::redraw(Redraw::Immediate);
        }
        let mut picker = Picker::with_items(rows);
        // Start on the profile in effect: the focused session's, or the
        // default for the next session.
        let current = self
            .focused()
            .and_then(|session_id| self.sessions.get(&session_id))
            .map_or(&self.profile, |session| &session.summary.profile);
        if let Some(index) = picker.items().iter().position(|row| row.id == *current) {
            picker.select_item(index);
        }
        self.overlay = Some(Overlay::Profiles(picker));
        Effects::redraw(Redraw::Immediate)
    }

    /// Apply `profile` to the focused idle session, or record it as the
    /// default for sessions created next when nothing is focused.
    fn accept_profile(&mut self, profile: AgentProfileId) -> Effects {
        let focused = self
            .focused()
            .and_then(|session_id| self.sessions.get(&session_id).map(|s| (session_id, s)));
        match focused {
            Some((_, session)) if session.summary.status == SessionStatus::Running => {
                // The runtime refuses a profile change mid-run; say so here
                // rather than round-tripping a command that will fail.
                self.set_warning(
                    "wait for the run to finish before changing the profile".to_owned(),
                );
                Effects::redraw(Redraw::Immediate)
            }
            Some((_, session)) if session.summary.profile == profile => {
                self.overlay = None;
                self.set_info(format!("session already uses profile {}", profile.as_str()));
                self.profile = profile;
                Effects::redraw(Redraw::Immediate)
            }
            Some((session_id, _)) => {
                self.overlay = None;
                self.profile = profile.clone();
                self.send(
                    PendingIntent::SetProfile { session_id },
                    SessionCommand::SetSessionProfile {
                        session_id,
                        profile,
                    },
                )
            }
            None => {
                self.overlay = None;
                self.set_info(format!(
                    "new sessions will use profile {}",
                    profile.as_str()
                ));
                self.profile = profile;
                Effects::redraw(Redraw::Immediate)
            }
        }
    }

    /// A refreshed capability document keeps an open profile picker current.
    pub(super) fn refresh_profile_picker(&mut self) {
        let Some(Overlay::Profiles(picker)) = &mut self.overlay else {
            return;
        };
        let Some(capabilities) = self.capabilities.as_deref() else {
            return;
        };
        picker.replace_items(profile_rows(capabilities), |row| row.id.clone());
    }

    // --- approval modes ---

    pub(crate) fn open_approval_modes(&mut self) -> Effects {
        let Some(capabilities) = self.capabilities.as_deref() else {
            self.set_warning(
                "approval modes are not available until the server's capabilities arrive"
                    .to_owned(),
            );
            return Effects::redraw(Redraw::Immediate);
        };
        let rows: Vec<ApprovalModeRow> = capabilities
            .approval_modes
            .iter()
            .copied()
            .map(approval_mode_row)
            .collect();
        if rows.is_empty() {
            self.set_warning("the server advertised no approval modes".to_owned());
            return Effects::redraw(Redraw::Immediate);
        }
        let mut picker = Picker::with_items(rows);
        let current = self.effective_approval_mode();
        if let Some(index) = picker.items().iter().position(|row| row.mode == current) {
            picker.select_item(index);
        }
        self.overlay = Some(Overlay::ApprovalModes(picker));
        Effects::redraw(Redraw::Immediate)
    }

    /// The approval mode in effect: the focused session's, or the default
    /// for the next session.
    pub(crate) fn effective_approval_mode(&self) -> ApprovalMode {
        self.focused()
            .and_then(|session_id| self.sessions.get(&session_id))
            .map_or(self.approval_mode, |session| session.summary.approval_mode)
    }

    /// Apply `mode` to the focused session (the runtime reads it at the next
    /// held call, so a running session may change too), or record it as the
    /// default for sessions created next when nothing is focused.
    fn accept_approval_mode(&mut self, mode: ApprovalMode) -> Effects {
        self.overlay = None;
        let focused = self
            .focused()
            .and_then(|session_id| self.sessions.get(&session_id).map(|s| (session_id, s)));
        match focused {
            Some((_, session)) if session.summary.approval_mode == mode => {
                self.approval_mode = mode;
                self.set_info(format!(
                    "session already uses approval mode {}",
                    approval_mode_label(mode)
                ));
                Effects::redraw(Redraw::Immediate)
            }
            Some((session_id, _)) => {
                self.approval_mode = mode;
                self.send(
                    PendingIntent::SetApprovalMode { session_id },
                    SessionCommand::SetApprovalMode { session_id, mode },
                )
            }
            None => {
                self.approval_mode = mode;
                self.set_info(format!(
                    "new sessions will use approval mode {}",
                    approval_mode_label(mode)
                ));
                Effects::redraw(Redraw::Immediate)
            }
        }
    }

    // --- skills ---

    pub(crate) fn open_skills(&mut self) -> Effects {
        let Some(capabilities) = self.capabilities.as_deref() else {
            self.set_warning(
                "skills are not available until the server's capabilities arrive".to_owned(),
            );
            return Effects::redraw(Redraw::Immediate);
        };
        let Some(tools) = capabilities.workspace_tools.as_ref() else {
            self.set_warning(
                "the server could not compile this workspace's plan; see `qq config check`"
                    .to_owned(),
            );
            return Effects::redraw(Redraw::Immediate);
        };
        let rows: Vec<SkillRow> = tools
            .skills
            .entries
            .iter()
            .map(|entry| SkillRow {
                name: entry.name.clone(),
                kind: entry.kind,
                source: entry.source.clone(),
                description: entry.description.clone(),
                disclosed: entry.disclosed,
            })
            .collect();
        if rows.is_empty() {
            self.set_info(
                "no commands or skills are indexed; add .qq/commands/<name>.md or .qq/skills/<name>/SKILL.md"
                    .to_owned(),
            );
            return Effects::redraw(Redraw::Immediate);
        }
        self.overlay = Some(Overlay::Skills(Picker::with_items(rows)));
        Effects::redraw(Redraw::Immediate)
    }

    // --- themes ---

    pub(super) fn open_themes(&mut self) -> Effects {
        if self.themes.len() < 2 {
            self.set_info(
                "only the compiled `terminal` theme is available; add themes/<name>.ron to choose"
                    .to_owned(),
            );
            return Effects::redraw(Redraw::Immediate);
        }
        let rows = self
            .themes
            .iter()
            .enumerate()
            .map(|(index, theme)| ThemeRow {
                index,
                name: theme.name.clone(),
            })
            .collect();
        let mut picker = Picker::with_items(rows);
        picker.select_item(self.theme);
        self.overlay = Some(Overlay::Themes {
            picker,
            restore: self.theme,
        });
        Effects::redraw(Redraw::Immediate)
    }

    /// The active theme. The theme picker previews by moving `theme`, so
    /// this is always what the next frame should paint with.
    pub(crate) fn theme(&self) -> &Theme {
        &self.themes[self.theme.min(self.themes.len() - 1)]
    }

    pub(super) fn set_theme(&mut self, index: usize) -> bool {
        if index >= self.themes.len() || index == self.theme {
            return false;
        }
        self.theme = index;
        self.theme_generation += 1;
        true
    }

    // --- sessions ---

    pub(super) fn open_sessions(&mut self) -> Effects {
        self.open_session_picker(None)
    }

    /// `/agents`: the focused session's root and every descendant, so the
    /// user can see and jump between the agents one task fanned out into.
    pub(super) fn open_agents(&mut self) -> Effects {
        let Some(focused) = self.focused() else {
            self.set_warning("focus a session to view its agents".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        let mut root = focused;
        while let Some(parent) = self
            .sessions
            .get(&root)
            .and_then(|session| session.summary.parent_id)
        {
            root = parent;
        }
        self.open_session_picker(Some(root))
    }

    fn open_session_picker(&mut self, scope: Option<SessionId>) -> Effects {
        let mut picker = Picker::with_items(self.session_rows(scope));
        if let Some(focused) = self.focused()
            && let Some(index) = picker.items().iter().position(|row| row.id == focused)
        {
            picker.select_item(index);
        }
        self.overlay = Some(Overlay::Sessions {
            picker,
            scope,
            confirm: None,
        });
        Effects::redraw(Redraw::Immediate)
    }

    /// Rows for the session picker in tree order, restricted to `scope`'s
    /// subtree when set.
    fn session_rows(&self, scope: Option<SessionId>) -> Vec<SessionRow> {
        self.sessions
            .thread_order()
            .iter()
            .copied()
            .filter(|session_id| {
                scope.is_none_or(|root| self.is_descendant_or_self(*session_id, root))
            })
            .map(|id| SessionRow {
                id,
                title: self.sessions[&id].summary.title.clone(),
            })
            .collect()
    }

    /// Rebuild the open session picker's rows after the tree changed,
    /// keeping the cursor on the same session.
    pub(super) fn refresh_session_picker(&mut self) {
        let scope = match &self.overlay {
            Some(Overlay::Sessions { scope, .. }) => *scope,
            _ => return,
        };
        let rows = self.session_rows(scope);
        if let Some(Overlay::Sessions { picker, .. }) = &mut self.overlay {
            picker.replace_items(rows, |row| row.id);
        }
    }

    fn is_descendant_or_self(&self, session_id: SessionId, root: SessionId) -> bool {
        let mut cursor = Some(session_id);
        while let Some(current) = cursor {
            if current == root {
                return true;
            }
            cursor = self
                .sessions
                .get(&current)
                .and_then(|session| session.summary.parent_id);
        }
        false
    }

    /// The highlighted session in the picker, if any.
    #[cfg(test)]
    pub(crate) fn session_picker_selected(&self) -> Option<SessionId> {
        match &self.overlay {
            Some(Overlay::Sessions { picker, .. }) => picker.current().map(|row| row.id),
            _ => None,
        }
    }

    pub(crate) fn session_picker_confirm(&self) -> Option<SessionConfirm> {
        match &self.overlay {
            Some(Overlay::Sessions { confirm, .. }) => *confirm,
            _ => None,
        }
    }

    fn request_delete_confirmation(&mut self, selected: Option<SessionId>) -> Effects {
        let Some(selected) = selected else {
            return Effects::none();
        };
        if self
            .sessions
            .get(&selected)
            .is_some_and(|session| session.summary.active_run_id.is_some())
        {
            self.set_warning("cancel the active run before deleting".to_owned());
            return Effects::redraw(Redraw::Immediate);
        }
        if let Some(Overlay::Sessions { confirm, .. }) = &mut self.overlay {
            *confirm = Some(SessionConfirm::Delete(selected));
        }
        Effects::redraw(Redraw::Immediate)
    }

    /// `/prune` from the composer: opens the session picker with the prune
    /// question armed so the user sees what the workspace holds first.
    pub(super) fn request_prune_confirmation(&mut self) -> Effects {
        if !matches!(self.overlay, Some(Overlay::Sessions { .. })) {
            self.open_session_picker(None);
        }
        if let Some(Overlay::Sessions { confirm, .. }) = &mut self.overlay {
            *confirm = Some(SessionConfirm::Prune);
        }
        Effects::redraw(Redraw::Immediate)
    }

    fn handle_session_picker_confirm_key(
        &mut self,
        key: KeyEvent,
        confirm: SessionConfirm,
    ) -> Effects {
        match key.code {
            KeyCode::Char('y' | 'Y') => {
                self.clear_session_picker_confirm();
                match confirm {
                    SessionConfirm::Delete(session_id) => self.delete_session(session_id),
                    SessionConfirm::Prune => self.prune_sessions(),
                }
            }
            KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                self.clear_session_picker_confirm();
                Effects::redraw(Redraw::Immediate)
            }
            _ => Effects::none(),
        }
    }

    pub(super) fn clear_session_picker_confirm(&mut self) {
        if let Some(Overlay::Sessions { confirm, .. }) = &mut self.overlay {
            *confirm = None;
        }
    }

    pub(super) fn delete_session(&mut self, session_id: SessionId) -> Effects {
        self.send(
            PendingIntent::Delete { session_id },
            qq_protocol::SessionCommand::DeleteSession { session_id },
        )
    }

    pub(super) fn prune_sessions(&mut self) -> Effects {
        let Some(workspace_id) = self.workspace_id else {
            self.set_warning("workspace is still connecting".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        self.send(
            PendingIntent::Prune,
            qq_protocol::SessionCommand::PruneSessions { workspace_id },
        )
    }

    // --- history ---

    /// `Ctrl-R`: search the focused session's prompts, newest first.
    pub(super) fn open_history(&mut self) -> Effects {
        let Some(session) = self.focused().and_then(|id| self.sessions.get(&id)) else {
            self.set_info("no session history to search".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        if session.prompt_history.is_empty() {
            self.set_info("no prompts yet in this session".to_owned());
            return Effects::redraw(Redraw::Immediate);
        }
        let rows = session
            .prompt_history
            .iter()
            .rev()
            .map(|text| HistoryRow { text: text.clone() })
            .collect();
        self.overlay = Some(Overlay::History(Picker::with_items(rows)));
        Effects::redraw(Redraw::Immediate)
    }

    /// Rows of the open history search, for the renderer.
    pub(crate) fn history_picker(&self) -> Option<&Picker<HistoryRow>> {
        match &self.overlay {
            Some(Overlay::History(picker)) => Some(picker),
            _ => None,
        }
    }

    // --- commands ---

    /// The command palette (`help` false) or the help overlay (`help` true):
    /// the same rows, the help view grouped by category with no query.
    pub(super) fn open_commands(&mut self, help: bool) -> Effects {
        self.overlay = Some(Overlay::Commands {
            picker: Picker::with_items(command_rows()),
            help,
        });
        Effects::redraw(Redraw::Immediate)
    }

    /// Rows of the open command palette, for the renderer.
    pub(crate) fn command_picker(&self) -> Option<(&Picker<CommandRow>, bool)> {
        match &self.overlay {
            Some(Overlay::Commands { picker, help }) => Some((picker, *help)),
            _ => None,
        }
    }

    /// Hint label for `command`: its chord under the current settings.
    pub(crate) fn chord_label(&self, command: Command) -> Option<String> {
        crate::commands::chord_label(&self.settings, command)
    }
}

#[cfg(test)]
impl App {
    /// Sessions visible in the open session picker, in row order.
    pub(crate) fn filtered_sessions(&self) -> Vec<SessionId> {
        match &self.overlay {
            Some(Overlay::Sessions { picker, .. }) => {
                picker.filtered().map(|(_, row)| row.id).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Indexes into `models` visible in the open model picker.
    pub(crate) fn filtered_models(&self) -> Vec<usize> {
        match &self.overlay {
            Some(Overlay::Models(picker)) => picker.filtered().map(|(_, row)| row.index).collect(),
            _ => Vec::new(),
        }
    }

    /// Indexes into `themes` visible in the open theme picker.
    pub(crate) fn filtered_themes(&self) -> Vec<usize> {
        match &self.overlay {
            Some(Overlay::Themes { picker, .. }) => {
                picker.filtered().map(|(_, row)| row.index).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Open the session picker with `query` typed and `selected` highlighted.
    pub(crate) fn open_session_picker_with(
        &mut self,
        query: &str,
        selected: Option<SessionId>,
        confirm: Option<SessionConfirm>,
    ) {
        let rows = self.session_rows(None);
        self.overlay = Some(Overlay::sessions(rows, query, selected, confirm));
    }

    pub(crate) fn open_model_picker_for_test(&mut self) {
        self.overlay = Some(Overlay::models(self.model_rows()));
    }
}

/// Rows for every advertised profile; `default` first, the rest in server
/// order (sorted by name).
fn profile_rows(capabilities: &ServerCapabilities) -> Vec<ProfileRow> {
    capabilities
        .profiles
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|profile| ProfileRow {
            id: profile.id.clone(),
            model: profile.model.clone(),
            approval_mode: profile.approval_mode,
            pack: profile
                .pack
                .as_ref()
                .map(|pack| format!("{}@{}", pack.id, pack.version)),
        })
        .collect()
}
