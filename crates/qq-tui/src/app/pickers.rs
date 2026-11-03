use std::collections::BTreeSet;

use crossterm::event::{KeyCode, KeyEvent};
use qq_protocol::{
    AgentProfileId, ApprovalMode, GuidanceKind, ModelDescriptor, ModelSelection, ReasoningEffort,
    ServerCapabilities, SessionCommand, SessionId, SessionStatus,
};

use super::{App, PendingIntent, ProviderRemedy};
use crate::{
    commands::{Command, SlashAction},
    effect::{Effects, Redraw},
    input::{
        ApprovalModeRow, CommandRow, EffortRow, HistoryRow, ModelRow, Overlay, PickerOutcome,
        ProfileRow, SessionConfirm, SessionRow, SkillRow, ThemeRow, approval_mode_label,
        approval_mode_row, command_rows, effort_label, effort_row,
    },
    picker::Picker,
    theme::Theme,
};
use qq_client::state::SessionView;

impl App {
    pub(super) fn handle_overlay_key(&mut self, key: KeyEvent) -> Effects {
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
                if let Some(row) = picker.current() {
                    let index = row.index;
                    self.set_theme(index);
                }
                Effects::redraw(Redraw::Immediate)
            }
            (PickerOutcome::Changed, Overlay::Sessions { picker, .. }) => {
                let selected = picker.current().map(|row| row.id);
                self.cancel_prefetch();
                let mut effects = Effects::redraw(Redraw::Immediate);
                if let Some(selected) = selected {
                    effects.extend(self.prefetch_session(selected));
                }
                effects
            }
            (PickerOutcome::Changed, _) => Effects::redraw(Redraw::Immediate),
            (PickerOutcome::Cancel, Overlay::Themes { restore, .. }) => {
                let restore = *restore;
                self.overlay = None;
                self.set_theme(restore);
                Effects::redraw(Redraw::Immediate)
            }
            (PickerOutcome::Cancel, _) => {
                let closing_sessions = matches!(self.overlay, Some(Overlay::Sessions { .. }));
                self.overlay = None;
                if closing_sessions {
                    self.cancel_prefetch();
                }
                Effects::redraw(Redraw::Immediate)
            }
            (PickerOutcome::Accept, Overlay::Find(_)) => Effects::none(),
            (PickerOutcome::Accept, Overlay::Models(picker)) => {
                let Some(row) = picker.current().cloned() else {
                    return Effects::none();
                };
                self.accept_model(&row, false)
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
            (PickerOutcome::Accept, Overlay::Effort(picker)) => {
                let Some(effort) = picker.current().map(|row| row.effort) else {
                    return Effects::none();
                };
                self.accept_effort(effort)
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
                self.cancel_prefetch();
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
            (PickerOutcome::Chord(KeyCode::Char('n' | 'N')), Overlay::Models(picker)) => {
                let Some(row) = picker.current().cloned() else {
                    return Effects::none();
                };
                self.accept_model(&row, true)
            }
            (
                PickerOutcome::Chord(KeyCode::Delete | KeyCode::Char('d' | 'D')),
                Overlay::Sessions { picker, .. },
            ) => {
                let selected = picker.current().map(|row| row.id);
                self.request_delete_confirmation(selected)
            }
            (PickerOutcome::Chord(KeyCode::Tab), Overlay::Sessions { .. }) => {
                self.toggle_session_agents()
            }
            (PickerOutcome::Chord(KeyCode::Char('a' | 'A')), Overlay::Sessions { .. }) => {
                self.toggle_all_session_agents()
            }
            (PickerOutcome::Chord(_), _) => Effects::none(),
        }
    }

    pub(super) fn open_models(&mut self) -> Effects {
        if self.models.is_empty() && self.unauthenticated_providers.is_empty() {
            self.set_warning("no authenticated providers have selectable models".to_owned());
            return Effects::redraw(Redraw::Immediate);
        }
        self.overlay = Some(Overlay::Models(Picker::with_items(self.model_rows())));
        Effects::redraw(Redraw::Immediate)
    }

    fn model_rows(&self) -> Vec<ModelRow> {
        if self.models.is_empty() {
            return self
                .unauthenticated_providers
                .iter()
                .map(|remedy| ModelRow {
                    index: None,
                    provider: remedy.provider.clone(),
                    model: remedy.remedy.clone(),
                    name: None,
                })
                .collect();
        }
        self.models
            .iter()
            .enumerate()
            .map(|(index, option)| ModelRow {
                index: Some(index),
                provider: option.provider.clone(),
                model: option.model.clone(),
                name: option.name.clone(),
            })
            .collect()
    }

    fn accept_model(&mut self, row: &ModelRow, create: bool) -> Effects {
        let Some(index) = row.index else {
            let remedy = self
                .unauthenticated_providers
                .iter()
                .find(|remedy| remedy.provider == row.provider)
                .map(ProviderRemedy::message);
            if let Some(message) = remedy {
                self.set_warning(message);
            }
            return Effects::redraw(Redraw::Immediate);
        };
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
        let rows = self.model_rows();
        if let Some(Overlay::Models(picker)) = &mut self.overlay {
            picker.replace_items(rows, |row| (row.provider.clone(), row.model.clone()));
        }
    }

    pub(crate) fn open_profiles(&mut self) -> Effects {
        let Some(capabilities) = self.capabilities.as_deref() else {
            self.set_warning(
                "profiles are not available until the server's capabilities arrive".to_owned(),
            );
            return Effects::redraw(Redraw::Immediate);
        };
        let rows = profile_rows(capabilities);
        if rows.is_empty() {
            self.set_warning("the server advertised no profiles for this workspace".to_owned());
            return Effects::redraw(Redraw::Immediate);
        }
        let mut picker = Picker::with_items(rows);
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

    fn accept_profile(&mut self, profile: AgentProfileId) -> Effects {
        let focused = self
            .focused()
            .and_then(|session_id| self.sessions.get(&session_id).map(|s| (session_id, s)));
        match focused {
            Some((_, session)) if session.summary.status == SessionStatus::Running => {
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

    pub(super) fn refresh_profile_picker(&mut self) {
        let Some(Overlay::Profiles(picker)) = &mut self.overlay else {
            return;
        };
        let Some(capabilities) = self.capabilities.as_deref() else {
            return;
        };
        picker.replace_items(profile_rows(capabilities), |row| row.id.clone());
    }

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

    pub(crate) fn effective_approval_mode(&self) -> ApprovalMode {
        self.focused()
            .and_then(|session_id| self.sessions.get(&session_id))
            .map_or(self.approval_mode, |session| session.summary.approval_mode)
    }

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

    pub(crate) fn open_effort(&mut self) -> Effects {
        let advertised = self.focused_model_efforts();
        let levels = advertised.iter().copied().map(Some);
        let rows: Vec<EffortRow> = [None, Some(ReasoningEffort::Default)]
            .into_iter()
            .chain(levels)
            .map(effort_row)
            .collect();
        let mut picker = Picker::with_items(rows);
        let current = self.effective_effort();
        if let Some(index) = picker.items().iter().position(|row| row.effort == current) {
            picker.select_item(index);
        }
        self.overlay = Some(Overlay::Effort(picker));
        Effects::redraw(Redraw::Immediate)
    }

    pub(crate) fn focused_model_efforts(&self) -> &[ReasoningEffort] {
        let model = self
            .focused()
            .and_then(|session_id| self.sessions.get(&session_id))
            .map_or(self.model.model.as_deref(), |session| {
                session.summary.model.as_deref()
            });
        super::model_reasoning_efforts(&self.models, model)
    }

    pub(crate) fn effective_effort(&self) -> Option<ReasoningEffort> {
        self.focused()
            .and_then(|session_id| self.sessions.get(&session_id))
            .map_or(self.reasoning_effort, |session| {
                session.summary.reasoning_effort
            })
    }

    fn accept_effort(&mut self, effort: Option<ReasoningEffort>) -> Effects {
        let focused = self
            .focused()
            .and_then(|session_id| self.sessions.get(&session_id).map(|s| (session_id, s)));
        match focused {
            Some((_, session)) if session.summary.status == SessionStatus::Running => {
                self.set_warning("wait for the run to finish before changing effort".to_owned());
                Effects::redraw(Redraw::Immediate)
            }
            Some((_, session)) if session.summary.reasoning_effort == effort => {
                self.overlay = None;
                self.reasoning_effort = effort;
                self.set_info(format!(
                    "session already uses effort {}",
                    effort_label(effort)
                ));
                Effects::redraw(Redraw::Immediate)
            }
            Some((session_id, _)) => {
                self.overlay = None;
                self.reasoning_effort = effort;
                self.send(
                    PendingIntent::SetEffort { session_id },
                    SessionCommand::SetSessionEffort { session_id, effort },
                )
            }
            None => {
                self.overlay = None;
                self.reasoning_effort = effort;
                self.set_info(format!(
                    "new sessions will use effort {}",
                    effort_label(effort)
                ));
                Effects::redraw(Redraw::Immediate)
            }
        }
    }

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

    pub(super) fn open_sessions(&mut self) -> Effects {
        self.open_session_picker(None)
    }

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
        self.cancel_prefetch();
        let all_agents = scope.is_some();
        let expanded = BTreeSet::new();
        let mut picker = Picker::with_items(self.session_rows(scope, &expanded, all_agents));
        let start = self
            .previous_focus
            .filter(|id| scope.is_none() && self.sessions.is_session(*id))
            .or_else(|| self.focused());
        if let Some(start) = start
            && let Some(index) = picker.items().iter().position(|row| row.id == start)
        {
            picker.select_item(index);
        }
        self.overlay = Some(Overlay::Sessions {
            picker,
            scope,
            confirm: None,
            expanded,
            all_agents,
        });
        let mut effects = Effects::redraw(Redraw::Immediate);
        if let Some(selected) = self.session_picker_selected() {
            effects.extend(self.prefetch_session(selected));
        }
        effects
    }

    fn session_rows(
        &self,
        scope: Option<SessionId>,
        expanded: &BTreeSet<SessionId>,
        all_agents: bool,
    ) -> Vec<SessionRow> {
        let order: Vec<(SessionId, usize, usize)> = match scope {
            Some(root) => self
                .sessions
                .session_of(root)
                .filter(|session| self.sessions.is_session(*session))
                .map(|session| {
                    let agents = self.sessions.agents_of(session);
                    let working = agents
                        .iter()
                        .filter(|agent| {
                            matches!(
                                self.sessions[agent].summary.status,
                                SessionStatus::Running | SessionStatus::Queued
                            )
                        })
                        .count();
                    (session, agents.len(), working)
                })
                .into_iter()
                .collect(),
            None => self
                .sessions
                .rail_order()
                .into_iter()
                .map(|row| (row.id, row.agents, row.working))
                .collect(),
        };
        let mut rows = Vec::with_capacity(order.len());
        for (session, agents, working) in order {
            let row = |id: SessionId, listed: bool| {
                let view = &self.sessions[&id];
                SessionRow {
                    id,
                    session,
                    title: view.summary.title.clone(),
                    agents: if id == session { agents } else { 0 },
                    working: if id == session { working } else { 0 },
                    updated_at_ms: view.summary.updated_at_ms,
                    answer: latest_answer(view),
                    listed,
                }
            };
            rows.push(row(session, true));
            if agents > 0 {
                let unfolded = all_agents || expanded.contains(&session);
                rows.extend(
                    self.sessions
                        .agents_of(session)
                        .into_iter()
                        .map(|agent| row(agent, unfolded)),
                );
            }
        }
        rows
    }

    pub(super) fn refresh_session_picker(&mut self) {
        let Some(Overlay::Sessions {
            scope,
            expanded,
            all_agents,
            picker,
            ..
        }) = &self.overlay
        else {
            return;
        };
        let (scope, expanded, all_agents) = (*scope, expanded.clone(), *all_agents);
        let before = picker.current().map(|row| (row.id, row.session));
        let rows = self.session_rows(scope, &expanded, all_agents);
        if let Some(Overlay::Sessions { picker, .. }) = &mut self.overlay {
            picker.replace_items(rows, |row| row.id);
            if let Some((id, session)) = before
                && picker.current().map(|row| row.id) != Some(id)
                && let Some(index) = picker.items().iter().position(|row| row.id == session)
            {
                picker.select_item(index);
            }
        }
    }

    fn toggle_session_agents(&mut self) -> Effects {
        let Some(Overlay::Sessions {
            picker, expanded, ..
        }) = &mut self.overlay
        else {
            return Effects::none();
        };
        let Some(session) = picker.current().map(|row| row.session) else {
            return Effects::none();
        };
        if !expanded.remove(&session) {
            expanded.insert(session);
        }
        self.refresh_session_picker();
        Effects::redraw(Redraw::Immediate)
    }

    fn toggle_all_session_agents(&mut self) -> Effects {
        let Some(Overlay::Sessions { all_agents, .. }) = &mut self.overlay else {
            return Effects::none();
        };
        *all_agents = !*all_agents;
        self.refresh_session_picker();
        Effects::redraw(Redraw::Immediate)
    }

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

    pub(super) fn undo_workspace(&mut self) -> Effects {
        let Some(workspace_id) = self.workspace_id else {
            self.set_warning("workspace is still connecting".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        self.set_info("restoring the workspace...".to_owned());
        self.send(
            PendingIntent::Undo,
            qq_protocol::SessionCommand::UndoWorkspace {
                workspace_id,
                snapshot: None,
            },
        )
    }

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

    pub(crate) fn history_picker(&self) -> Option<&Picker<HistoryRow>> {
        match &self.overlay {
            Some(Overlay::History(picker)) => Some(picker),
            _ => None,
        }
    }

    pub(super) fn open_commands(&mut self, help: bool) -> Effects {
        self.overlay = Some(Overlay::Commands {
            picker: Picker::with_items(command_rows()),
            help,
        });
        Effects::redraw(Redraw::Immediate)
    }

    pub(crate) fn command_picker(&self) -> Option<(&Picker<CommandRow>, bool)> {
        match &self.overlay {
            Some(Overlay::Commands { picker, help }) => Some((picker, *help)),
            _ => None,
        }
    }

    pub(crate) fn chord_label(&self, command: Command) -> Option<String> {
        crate::commands::chord_label(&self.settings, command)
    }
}

#[cfg(test)]
impl App {
    pub(crate) fn filtered_sessions(&self) -> Vec<SessionId> {
        match &self.overlay {
            Some(Overlay::Sessions { picker, .. }) => {
                picker.filtered().map(|(_, row)| row.id).collect()
            }
            _ => Vec::new(),
        }
    }

    pub(crate) fn filtered_models(&self) -> Vec<usize> {
        match &self.overlay {
            Some(Overlay::Models(picker)) => {
                picker.filtered().filter_map(|(_, row)| row.index).collect()
            }
            _ => Vec::new(),
        }
    }

    pub(crate) fn filtered_themes(&self) -> Vec<usize> {
        match &self.overlay {
            Some(Overlay::Themes { picker, .. }) => {
                picker.filtered().map(|(_, row)| row.index).collect()
            }
            _ => Vec::new(),
        }
    }

    pub(crate) fn open_session_picker_with(
        &mut self,
        query: &str,
        selected: Option<SessionId>,
        confirm: Option<SessionConfirm>,
    ) {
        let rows = self.session_rows(None, &BTreeSet::new(), false);
        self.overlay = Some(Overlay::sessions(rows, query, selected, confirm));
    }

    pub(crate) fn open_model_picker_for_test(&mut self) {
        self.overlay = Some(Overlay::models(self.model_rows()));
    }
}

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
        })
        .collect()
}

fn latest_answer(view: &SessionView) -> String {
    let Some(messages) = view.messages.as_ref() else {
        return String::new();
    };
    let Some(text) = messages
        .iter()
        .rev()
        .filter(|message| message.role == qq_protocol::MessageRole::Assistant)
        .map(super::message_source)
        .find(|text| !text.trim().is_empty())
    else {
        return String::new();
    };
    let mut end = text.len().min(crate::input::SESSION_ANSWER_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
