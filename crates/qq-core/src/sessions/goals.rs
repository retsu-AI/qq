//! Durable client goal mutations. Callers hold the store transaction that
//! publishes the corresponding event; no shadow copy is authoritative.
use super::*;
use qq_protocol::{
    GoalBudget, GoalCheck, GoalCheckAuthorization, GoalCheckScope, GoalControl, GoalCounters,
    GoalId, GoalIdentity, GoalNeedsUser, GoalPauseReason, GoalSnapshot, GoalStatus,
};

pub(super) struct Specification {
    pub(super) objective: String,
    pub(super) check: Option<GoalCheck>,
    pub(super) budget: GoalBudget,
}

/// The runtime receives only the claimed goal and its scoped writer. It
/// cannot create goals or borrow authority from a later session specification.
#[derive(Clone)]
pub(crate) struct GoalRunInput {
    store: Store,
    identity: RunIdentity,
    captured: Arc<GoalSnapshot>,
    initial_notice: Arc<str>,
    origin: Option<qq_protocol::RunOrigin>,
    displayed_revision: u32,
    display: Option<Arc<GoalSnapshot>>,
}

type GoalUpdateReceipt =
    oneshot::Receiver<Result<crate::tools::goal::UpdateGoalResult, SessionRuntimeError>>;

impl GoalRunInput {
    pub(super) fn new(store: Store, claimed: &ClaimedRun) -> Option<Self> {
        claimed.goal.as_ref().map(|goal| Self {
            store,
            identity: claimed.identity,
            captured: Arc::clone(goal),
            initial_notice: Arc::clone(
                claimed
                    .goal_initial_notice
                    .as_ref()
                    .expect("validated active goal claim has its initial notice"),
            ),
            origin: claimed.origin,
            displayed_revision: goal.revision,
            display: Some(Arc::clone(goal)),
        })
    }

    pub(crate) fn initial_notice(&self) -> &str {
        &self.initial_notice
    }

    pub(crate) async fn refresh_notice(&mut self) -> Result<Option<String>, SessionRuntimeError> {
        let session = self.identity.session_id;
        let captured_id = self.captured.goal_id;
        let current = self
            .store
            .call_after_group(store::Priority::AwaitControl, move |connection| {
                let current = load_live(connection, session)?;
                let Some(current) = current.filter(|goal| {
                    goal.goal_id == captured_id && goal.status == GoalStatus::Active
                }) else {
                    return Ok(None);
                };
                if !crate::tools::goal::fits_notice(&current) {
                    return Err(SessionRuntimeError::CONSTRAINT);
                }
                Ok(Some(Arc::new(current)))
            })
            .await?;
        self.display = current;
        let Some(current) = &self.display else {
            return Ok(None);
        };
        if current.revision == self.displayed_revision {
            return Ok(None);
        }
        self.displayed_revision = current.revision;
        Ok(Some(crate::tools::goal::render_notice(current, now_ms())))
    }

    pub(crate) fn post_summary_notice(&self) -> Option<String> {
        self.display
            .as_ref()
            .map(|goal| crate::tools::goal::render_notice(goal, now_ms()))
    }

    pub(crate) async fn admit_update(
        &self,
        arguments: crate::tools::goal::UpdateGoalArgs,
    ) -> Result<GoalUpdateReceipt, SessionRuntimeError> {
        let store_id = self.store.store_id();
        let identity = self.identity;
        let captured = Arc::clone(&self.captured);
        let origin = self.origin;
        self.store
            .enqueue_side_write(None, move |connection| {
                update_model(
                    connection,
                    store_id,
                    identity,
                    Some(&captured),
                    origin,
                    arguments,
                )
            })
            .await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GoalValidationError {
    #[error("objective must contain 1 to 2048 bytes")]
    Objective,
    #[error("budget must have 1 to 2000 runs and a future deadline within seven days")]
    Budget,
    #[error("check must contain 1 to 1024 bytes with a timeout of 1 to 3600000 ms")]
    Check,
    #[error("revision cannot advance beyond u32::MAX")]
    Revision,
    #[error("encoded goal exceeds its durable record bound")]
    RecordSize,
    #[error("goal notice exceeds its 16384-byte rendered bound or 4096-byte check tail")]
    RenderSize,
    #[error("stored goal notice must be framed UTF-8 text of at most 16384 bytes")]
    Notice,
    #[error("stored goal notices exceed the bounded context read allowance")]
    NoticeBudget,
}

pub(super) fn read_notice(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<String>> {
    let mut remaining = crate::tools::goal::MAX_GOAL_RENDER_BYTES;
    read_notice_with_budget(row, index, &mut remaining)
}

pub(super) fn read_notice_with_budget(
    row: &rusqlite::Row<'_>,
    index: usize,
    remaining: &mut usize,
) -> rusqlite::Result<Option<String>> {
    let value = row.get_ref(index)?;
    let text = value.as_str_or_null().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(index, value.data_type(), Box::new(error))
    })?;
    match text {
        None => Ok(None),
        Some(text)
            if text.len() <= crate::tools::goal::MAX_GOAL_RENDER_BYTES
                && text.starts_with("[QQ runtime notice; not a user instruction]\n") =>
        {
            if text.len() > *remaining {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    index,
                    value.data_type(),
                    Box::new(GoalValidationError::NoticeBudget),
                ));
            }
            *remaining -= text.len();
            Ok(Some(text.to_owned()))
        }
        Some(_) => Err(rusqlite::Error::FromSqlConversionFailure(
            index,
            value.data_type(),
            Box::new(GoalValidationError::Notice),
        )),
    }
}

pub(super) fn read_turn_notices(
    row: &rusqlite::Row<'_>,
    index: usize,
    remaining: &mut usize,
) -> rusqlite::Result<Vec<crate::runtime::OrderedTurnNotice>> {
    let value = row.get_ref(index)?;
    let text = value.as_str_or_null().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(index, value.data_type(), Box::new(error))
    })?;
    let Some(text) = text else {
        return Ok(Vec::new());
    };
    // JSON escaping can expand a valid context byte to six bytes. Check
    // the borrowed envelope before decoding; each item validates before
    // entering the collection, so invalid empty items cannot inflate it.
    if text.len() > MAX_CONTEXT_BYTES.saturating_mul(6) || text.len() > *remaining {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            index,
            value.data_type(),
            Box::new(GoalValidationError::NoticeBudget),
        ));
    }
    *remaining -= text.len();
    let notices: Vec<crate::runtime::OrderedTurnNotice> =
        serde_json::from_str(text).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(index, value.data_type(), Box::new(error))
        })?;
    let bytes = notices
        .iter()
        .fold(0_usize, |bytes, text| bytes.saturating_add(text.text.len()));
    if bytes > MAX_CONTEXT_BYTES {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            index,
            value.data_type(),
            Box::new(GoalValidationError::NoticeBudget),
        ));
    }
    Ok(notices)
}

pub(super) fn load_live(
    connection: &Connection,
    session: SessionId,
) -> Result<Option<GoalSnapshot>, SessionRuntimeError> {
    let row = connection
        .query_row(
            "SELECT goal_id, revision, payload_json FROM session_goals
         WHERE session_id = ?1 AND live = 1",
            [session.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((id, revision, payload)) = row else {
        return Ok(None);
    };
    if payload.len() > 64 * 1024 {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let goal: GoalSnapshot = serde_json::from_str(&payload)?;
    if goal.goal_id.to_string() != id || goal.revision != revision || goal.session_id != session {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    Ok(Some(goal))
}

pub(super) fn set(
    connection: &Connection,
    session: SessionId,
    mut specification: Specification,
    expected: Option<GoalIdentity>,
    replace: bool,
    now: u64,
) -> Result<GoalSnapshot, SessionRuntimeError> {
    session_workspace(connection, session)?;
    let current = load_live(connection, session)?;
    let identity = current.as_ref().map(|goal| GoalIdentity {
        goal_id: goal.goal_id,
        revision: goal.revision,
    });
    if expected != identity || (replace && identity.is_none()) {
        return Err(SessionRuntimeError::GoalConflict { current: identity });
    }
    if current
        .as_ref()
        .is_some_and(|goal| goal.status == GoalStatus::Achieved)
        && !replace
    {
        return Err(SessionRuntimeError::GoalImmutable);
    }
    if specification.objective.is_empty() || specification.objective.len() > 2048 {
        return Err(SessionRuntimeError::InvalidGoal(
            GoalValidationError::Objective,
        ));
    }
    if specification.budget.max_goal_runs == 0
        || specification.budget.max_goal_runs > 2000
        || specification.budget.deadline_ms <= now
        || specification.budget.deadline_ms.saturating_sub(now) > 7 * 86_400_000
    {
        return Err(SessionRuntimeError::InvalidGoal(
            GoalValidationError::Budget,
        ));
    }
    if specification.check.as_ref().is_some_and(|check| {
        check.command.is_empty()
            || check.command.len() > 1024
            || check.timeout_ms == 0
            || check.timeout_ms > 3_600_000
    }) {
        return Err(SessionRuntimeError::InvalidGoal(GoalValidationError::Check));
    }
    validate_run_limits(&specification.budget.per_run)?;
    if specification.budget.per_run.max_model_turns.is_none() {
        specification.budget.per_run.max_model_turns =
            Some(qq_protocol::DEFAULT_GOAL_RUN_MODEL_TURNS);
    }
    let check_changed = current
        .as_ref()
        .is_none_or(|goal| goal.check != specification.check);
    let mut goal = match current {
        Some(previous) if !replace => previous,
        previous => {
            if let Some(previous) = previous {
                connection.execute(
                    "UPDATE session_goals SET live = 0 WHERE goal_id = ?1",
                    [previous.goal_id.to_string()],
                )?;
            }
            let goal_id = GoalId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            GoalSnapshot {
                session_id: session,
                goal_id,
                revision: 0,
                objective: String::new(),
                check: None,
                check_authorization: None,
                budget: specification.budget.clone(),
                checklist: Vec::new(),
                notes: String::new(),
                status: GoalStatus::Active,
                counters: GoalCounters::default(),
                next_run_at_ms: now,
                wait: None,
                achievement_claim: None,
                audit_pending: false,
                audit_revision: None,
                pending_run_id: None,
                check_output: None,
            }
        }
    };
    goal.revision = goal
        .revision
        .checked_add(1)
        .ok_or(SessionRuntimeError::InvalidGoal(
            GoalValidationError::Revision,
        ))?;
    goal.objective = specification.objective;
    goal.budget = specification.budget;
    goal.check = specification.check;
    goal.wait = None;
    goal.achievement_claim = None;
    goal.audit_pending = false;
    goal.audit_revision = None;
    goal.check_output = None;
    goal.next_run_at_ms = now;
    if check_changed || replace {
        goal.check_authorization = goal.check.as_ref().map(|_| GoalCheckAuthorization::Pending);
        if goal.check.is_some() && goal.status == GoalStatus::Active {
            goal.status = needs_check_approval();
        } else if goal.check.is_none() && goal.status == needs_check_approval() {
            goal.status = GoalStatus::Paused {
                reason: GoalPauseReason::User,
            };
        }
    }
    save(connection, &goal)?;
    Ok(goal)
}

pub(super) fn control(
    connection: &Connection,
    session: SessionId,
    expected: GoalIdentity,
    action: GoalControl,
    now: u64,
    check_policy: Option<&Result<(), GoalCheckPolicyError>>,
) -> Result<(GoalSnapshot, bool), SessionRuntimeError> {
    session_workspace(connection, session)?;
    let mut goal = load_live(connection, session)?.ok_or(SessionRuntimeError::GoalNotFound)?;
    let identity = GoalIdentity {
        goal_id: goal.goal_id,
        revision: goal.revision,
    };
    if expected != identity {
        return Err(SessionRuntimeError::GoalConflict {
            current: Some(identity),
        });
    }
    let clear = action == GoalControl::Clear;
    if goal.status == GoalStatus::Achieved && !clear {
        return Err(SessionRuntimeError::GoalImmutable);
    }
    match action {
        GoalControl::Clear => {}
        GoalControl::Pause => {
            goal.status = GoalStatus::Paused {
                reason: GoalPauseReason::User,
            }
        }
        GoalControl::Resume => {
            ensure_activation(&goal, now)?;
            if goal.check.is_some()
                && !matches!(
                    goal.check_authorization,
                    Some(GoalCheckAuthorization::Once | GoalCheckAuthorization::ForGoal)
                )
            {
                return Err(SessionRuntimeError::GoalCheckApprovalRequired);
            }
            goal.status = GoalStatus::Active;
            goal.counters.no_progress_streak = 0;
            goal.counters.failure_streak = 0;
            goal.next_run_at_ms = now;
            goal.wait = None;
        }
        GoalControl::ApproveCheck { scope, command } => {
            let Some(check) = goal.check.as_ref() else {
                return Err(SessionRuntimeError::GoalCheckMissing);
            };
            if check.command != command {
                return Err(SessionRuntimeError::GoalCheckCommandMismatch);
            }
            let workspace_id = session_workspace(connection, session)?;
            let workspace: String = connection.query_row(
                "SELECT path FROM workspaces WHERE id = ?1",
                [workspace_id.to_string()],
                |row| row.get(0),
            )?;
            if crate::approval::classify_command(&command, Some(Path::new(&workspace))).decision
                == crate::approval::Decision::Forbidden
            {
                return Err(SessionRuntimeError::GoalCheckForbidden);
            }
            if let Some(Err(error)) = check_policy {
                return Err(SessionRuntimeError::GoalCheckPolicy(error.clone()));
            }
            if goal.status == needs_check_approval() {
                ensure_activation(&goal, now)?;
            }
            goal.check_authorization = Some(match scope {
                GoalCheckScope::Once => GoalCheckAuthorization::Once,
                GoalCheckScope::ForGoal => GoalCheckAuthorization::ForGoal,
            });
            if goal.status == needs_check_approval() {
                goal.status = GoalStatus::Active;
            }
        }
        GoalControl::DenyCheck => {
            if goal.check.is_none() {
                return Err(SessionRuntimeError::GoalCheckMissing);
            }
            goal.check_authorization = Some(GoalCheckAuthorization::Pending);
            if goal.status == GoalStatus::Active {
                goal.status = needs_check_approval();
            }
        }
    }
    goal.revision = goal
        .revision
        .checked_add(1)
        .ok_or(SessionRuntimeError::InvalidGoal(
            GoalValidationError::Revision,
        ))?;
    if goal.audit_pending {
        goal.audit_revision = Some(goal.revision);
    }
    save(connection, &goal)?;
    if clear {
        connection.execute(
            "UPDATE session_goals SET live = 0 WHERE goal_id = ?1",
            [goal.goal_id.to_string()],
        )?;
    }
    Ok((goal, clear))
}

fn needs_check_approval() -> GoalStatus {
    GoalStatus::Paused {
        reason: GoalPauseReason::NeedsUser {
            cause: GoalNeedsUser::CheckApproval,
        },
    }
}

fn ensure_activation(goal: &GoalSnapshot, now: u64) -> Result<(), SessionRuntimeError> {
    if goal.pending_run_id.is_some() {
        return Err(SessionRuntimeError::GoalCancellationPending);
    }
    if goal.budget.deadline_ms <= now || goal.counters.runs >= goal.budget.max_goal_runs {
        return Err(SessionRuntimeError::GoalBudgetSpent);
    }
    if let Some(limit) = goal.budget.max_total_tokens {
        let Some(usage) = goal.counters.usage else {
            return Err(SessionRuntimeError::GoalBudgetSpent);
        };
        let spent = usage
            .input_tokens
            .checked_add(usage.cache_read_input_tokens)
            .and_then(|value| value.checked_add(usage.cache_write_input_tokens))
            .and_then(|value| value.checked_add(usage.output_tokens));
        if spent.is_none_or(|spent| spent >= limit) {
            return Err(SessionRuntimeError::GoalBudgetSpent);
        }
    }
    if let Some(limit) = goal.budget.max_cost_usd_nanos
        && goal
            .counters
            .estimated_cost_usd_nanos
            .is_none_or(|spent| spent >= limit)
    {
        return Err(SessionRuntimeError::GoalBudgetSpent);
    }
    Ok(())
}

pub(super) fn save(
    connection: &Connection,
    goal: &GoalSnapshot,
) -> Result<(), SessionRuntimeError> {
    if !crate::tools::goal::fits_notice(goal) {
        return Err(SessionRuntimeError::InvalidGoal(
            GoalValidationError::RenderSize,
        ));
    }
    let payload = serde_json::to_string(goal)?;
    if payload.len() > 64 * 1024 {
        return Err(SessionRuntimeError::InvalidGoal(
            GoalValidationError::RecordSize,
        ));
    }
    connection.execute(
        "INSERT INTO session_goals(goal_id, session_id, revision, live, payload_json)
         VALUES (?1, ?2, ?3, 1, ?4) ON CONFLICT(goal_id) DO UPDATE
         SET revision = excluded.revision, payload_json = excluded.payload_json",
        params![
            goal.goal_id.to_string(),
            goal.session_id.to_string(),
            goal.revision,
            payload
        ],
    )?;
    Ok(())
}

/// Freeze the prompt owner's captured attribution inside its first paid admission.
/// SQL NULL is uncaptured; JSON null is a durable captured absence.
pub(super) fn freeze_goal_claim(
    transaction: &Connection,
    identity: RunIdentity,
    goal: Option<&GoalSnapshot>,
    initial_notice: Option<&str>,
) -> Result<(), SessionRuntimeError> {
    if identity.kind != RunKind::Prompt {
        return if goal.is_none() && initial_notice.is_none() {
            Ok(())
        } else {
            Err(SessionRuntimeError::CONSTRAINT)
        };
    }
    if goal.is_some() != initial_notice.is_some()
        || initial_notice.is_some_and(|text| {
            text.len() > crate::tools::goal::MAX_GOAL_RENDER_BYTES
                || !text.starts_with("[QQ runtime notice; not a user instruction]\n")
        })
    {
        return Err(SessionRuntimeError::InvalidGoal(
            GoalValidationError::Notice,
        ));
    }
    if goal.is_some_and(|goal| {
        goal.session_id != identity.session_id || goal.status != GoalStatus::Active
    }) {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let encoded = serde_json::to_string(&goal)?;
    if encoded.len() > 64 * 1024 {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let id = goal.map(|goal| goal.goal_id.to_string());
    let revision = goal.map(|goal| goal.revision);
    let stored: (Option<String>, Option<u32>, Option<String>, Option<String>) = transaction.query_row(
        "SELECT goal_id, goal_revision, goal_snapshot_json, goal_initial_notice FROM runs WHERE id = ?1 AND session_id = ?2",
        params![identity.run_id.to_string(), identity.session_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, read_notice(row, 3)?)),
    )?;
    match stored {
        (None, None, None, None) => {
            let changed = transaction.execute(
                "UPDATE runs SET goal_id = ?3, goal_revision = ?4, goal_snapshot_json = ?5, goal_initial_notice = ?6
                 WHERE id = ?1 AND session_id = ?2 AND goal_snapshot_json IS NULL",
                params![
                    identity.run_id.to_string(),
                    identity.session_id.to_string(),
                    id,
                    revision,
                    encoded,
                    initial_notice
                ],
            )?;
            if changed != 1 {
                return Err(SessionRuntimeError::CONSTRAINT);
            }
            Ok(())
        }
        (stored_id, stored_revision, Some(stored_payload), stored_notice)
            if stored_id == id
                && stored_revision == revision
                && stored_payload == encoded
                && stored_notice.as_deref() == initial_notice =>
        {
            Ok(())
        }
        _ => Err(SessionRuntimeError::CONSTRAINT),
    }
}

/// Apply only model-owned data against the current live goal. Client revisions
/// fence controls, while notes/checklist can merge without replacing client data.
pub(super) fn update_model(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    captured: Option<&GoalSnapshot>,
    origin: Option<qq_protocol::RunOrigin>,
    arguments: crate::tools::goal::UpdateGoalArgs,
) -> Result<crate::tools::goal::UpdateGoalResult, SessionRuntimeError> {
    use crate::tools::goal::{
        ChecklistEdit, GoalUpdateField as Field, GoalUpdateRejection as Reject,
        MAX_CHECKLIST_ITEMS, MAX_CHECKLIST_TEXT_BYTES, MAX_NOTES_BYTES, MAX_REASON_BYTES,
        MAX_WAIT_SECONDS, ModelStatus, UpdateGoalResult,
    };
    let mut result = UpdateGoalResult::default();
    let Some(captured) = captured else {
        result.reject(Field::Goal, Reject::NoClaim, None);
        return Ok(result);
    };
    if captured.session_id != identity.session_id
        || captured.status != GoalStatus::Active
        || identity.kind != RunKind::Prompt
    {
        result.reject(Field::Goal, Reject::InvalidClaim, None);
        return Ok(result);
    }
    let transaction = store::begin_unit(connection)?;
    let Some(mut current) = load_live(&transaction, identity.session_id)? else {
        result.reject(Field::Goal, Reject::Stopped, None);
        return Ok(result);
    };
    if current.goal_id != captured.goal_id {
        result.reject(Field::Goal, Reject::Replaced, None);
        return Ok(result);
    }
    if current.status != GoalStatus::Active {
        result.reject(Field::Goal, Reject::Stopped, None);
        return Ok(result);
    }
    let row=transaction.query_row(
        "SELECT status='running' AND cancel_requested=0 AND kind='prompt', goal_id,goal_revision,origin_json FROM runs WHERE id=?1 AND session_id=?2",
        params![identity.run_id.to_string(),identity.session_id.to_string()],
        |row|Ok((row.get::<_,bool>(0)?,row.get::<_,Option<String>>(1)?,row.get::<_,Option<u32>>(2)?,row.get::<_,Option<String>>(3)?)),
    ).optional()?;
    let Some((running, id, revision, stored_origin)) = row else {
        result.reject(Field::Goal, Reject::RunStopped, None);
        return Ok(result);
    };
    if !running {
        result.reject(Field::Goal, Reject::RunStopped, None);
        return Ok(result);
    }
    let stored_origin = stored_origin
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?;
    if id.as_deref() != Some(captured.goal_id.to_string().as_str())
        || revision != Some(captured.revision)
        || stored_origin != origin
    {
        result.reject(Field::Goal, Reject::InvalidClaim, None);
        return Ok(result);
    }
    let can_control = matches!(origin,Some(qq_protocol::RunOrigin::Goal {goal_id,revision}) if goal_id==captured.goal_id && revision==captured.revision);
    let stale = current.revision != captured.revision;
    let conflicting = arguments.status.is_some() && arguments.wait.is_some();
    let previous = current.clone();
    if arguments.checklist.len() > MAX_CHECKLIST_ITEMS {
        result.reject(Field::Checklist, Reject::TooManyItems, None);
    } else {
        for (index, edit) in arguments.checklist.into_iter().enumerate() {
            match edit {
                ChecklistEdit::Add { text } => {
                    if text.trim().is_empty() || text.len() > MAX_CHECKLIST_TEXT_BYTES {
                        result.reject(Field::Checklist, Reject::InvalidValue, Some(index));
                        continue;
                    }
                    if current.checklist.len() >= MAX_CHECKLIST_ITEMS {
                        result.reject(Field::Checklist, Reject::TooManyItems, Some(index));
                        continue;
                    }
                    let id = (1..=u8::MAX)
                        .find(|id| current.checklist.iter().all(|item| item.id != *id))
                        .ok_or(SessionRuntimeError::CONSTRAINT)?;
                    current.checklist.push(qq_protocol::GoalChecklistItem {
                        id,
                        text,
                        state: qq_protocol::GoalChecklistState::Pending,
                    });
                    result.added += 1;
                    result.added_ids.push(id);
                }
                ChecklistEdit::SetState { id, state } => {
                    if let Some(item) = current.checklist.iter_mut().find(|item| item.id == id) {
                        item.state = state;
                        result.changed += 1;
                    } else {
                        result.reject(Field::Checklist, Reject::UnknownItem, Some(index));
                    }
                }
                ChecklistEdit::Drop { id } => {
                    if let Some(position) = current.checklist.iter().position(|item| item.id == id)
                    {
                        current.checklist.remove(position);
                        result.dropped += 1;
                    } else {
                        result.reject(Field::Checklist, Reject::UnknownItem, Some(index));
                    }
                }
            }
        }
    }
    if let Some(notes) = arguments.notes {
        if notes.len() > MAX_NOTES_BYTES {
            result.reject(Field::Notes, Reject::InvalidValue, None);
        } else {
            current.notes = notes;
            result.notes_applied = true;
        }
    }
    if let Some(status) = arguments.status {
        let rejection = if !can_control {
            Some(Reject::ControlNotAllowed)
        } else if stale {
            Some(Reject::StaleRevision)
        } else if conflicting {
            Some(Reject::ConflictingControls)
        } else {
            None
        };
        if let Some(reason) = rejection {
            result.reject(Field::Status, reason, None);
        } else {
            match status {
                ModelStatus::Achieved { evidence } => {
                    if evidence.trim().is_empty() || evidence.len() > MAX_REASON_BYTES {
                        result.reject(Field::Status, Reject::InvalidValue, None);
                    } else {
                        current.achievement_claim = Some(qq_protocol::GoalAchievementClaim {
                            run_id: identity.run_id,
                            revision: captured.revision,
                            evidence,
                        });
                        current.audit_pending = true;
                        current.audit_revision = Some(current.revision);
                        current.wait = None;
                        result.status_applied = true;
                    }
                }
                ModelStatus::Blocked { reason } => {
                    if reason.trim().is_empty() || reason.len() > MAX_REASON_BYTES {
                        result.reject(Field::Status, Reject::InvalidValue, None);
                    } else {
                        current.status = GoalStatus::Blocked {
                            reason: qq_protocol::GoalBlockedReason::Model { reason },
                        };
                        current.wait = None;
                        current.achievement_claim = None;
                        current.audit_pending = false;
                        current.audit_revision = None;
                        result.status_applied = true;
                    }
                }
            }
        }
    }
    if let Some(wait) = arguments.wait {
        let rejection = if !can_control {
            Some(Reject::ControlNotAllowed)
        } else if stale {
            Some(Reject::StaleRevision)
        } else if conflicting {
            Some(Reject::ConflictingControls)
        } else if wait.seconds > MAX_WAIT_SECONDS
            || wait.reason.trim().is_empty()
            || wait.reason.len() > MAX_REASON_BYTES
        {
            Some(Reject::InvalidValue)
        } else {
            None
        };
        if let Some(reason) = rejection {
            result.reject(Field::Wait, reason, None);
        } else {
            let until_ms = now_ms()
                .saturating_add(u64::from(wait.seconds) * 1000)
                .min(current.budget.deadline_ms);
            current.wait = Some(qq_protocol::GoalWait {
                until_ms,
                reason: wait.reason,
            });
            current.next_run_at_ms = until_ms;
            current.achievement_claim = None;
            current.audit_pending = false;
            current.audit_revision = None;
            result.wait_applied = true;
        }
    }
    result.checklist_changed = current.checklist != previous.checklist;
    if current != previous {
        match save(&transaction, &current) {
            Ok(()) => {}
            Err(SessionRuntimeError::InvalidGoal(GoalValidationError::RecordSize)) => {
                let mut refused = UpdateGoalResult::default();
                refused.reject(Field::Goal, Reject::RecordTooLarge, None);
                return Ok(refused);
            }
            Err(SessionRuntimeError::InvalidGoal(GoalValidationError::RenderSize)) => {
                let mut refused = UpdateGoalResult::default();
                refused.reject(Field::Goal, Reject::RenderTooLarge, None);
                return Ok(refused);
            }
            Err(error) => return Err(error),
        }
        append_event(
            &transaction,
            EventContext::for_run(store_id, identity, now_ms()),
            SessionEvent::GoalUpdated {
                goal: Box::new(current),
            },
        )?;
    }
    transaction.commit()?;
    Ok(result)
}
