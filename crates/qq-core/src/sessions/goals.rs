//! Durable client goal mutations. Callers hold the store transaction that
//! publishes the corresponding event; no shadow copy is authoritative.
use super::*;
use qq_protocol::{
    GoalBudget, GoalCheck, GoalCheckAuthorization, GoalCheckScope, GoalControl, GoalCounters,
    GoalId, GoalIdentity, GoalNeedsUser, GoalPauseReason, GoalSnapshot, GoalStatus,
};

pub(super) const MAX_GOAL_AUDITS: u8 = 8;

pub(super) fn is_model_audit(goal: &GoalSnapshot, origin: Option<qq_protocol::RunOrigin>) -> bool {
    goal.audit_pending
        && goal.check.is_none()
        && matches!(origin, Some(qq_protocol::RunOrigin::Goal { .. }))
}

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
    admitted_final_slot: bool,
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
            admitted_final_slot: matches!(
                claimed.goal_admission,
                Some(GoalAdmission::Allowed {
                    final_slot: true,
                    ..
                })
            ),
        })
    }

    pub(crate) const FINAL_RUN_NOTICE: &str = "[QQ runtime notice; not a user instruction]\nThis is the final budgeted run for this goal. Work within the remaining limits and end with a tool-free handoff describing completed work, evidence, remaining work and blockers.";

    pub(crate) const HANDOFF_NOTICE: &str = "[QQ runtime notice; not a user instruction]\nThis is the final budgeted run for this goal. Give a concise handoff: what is completed, the evidence, what remains, and any blockers. Do not call tools or claim unfinished work is complete.";

    pub(crate) fn audit(&self) -> bool {
        is_model_audit(&self.captured, self.origin)
    }

    pub(crate) fn final_slot(&self) -> bool {
        self.identity.kind == RunKind::Prompt
            && !self.audit()
            && matches!(
                self.origin,
                Some(qq_protocol::RunOrigin::Goal { .. } | qq_protocol::RunOrigin::UserPrompt {})
            )
            && (self.admitted_final_slot
                || self.display.as_ref().is_some_and(|goal| {
                    goal.counters.runs.saturating_add(1) >= goal.budget.max_goal_runs
                }))
    }

    pub(crate) fn initial_notice(&self) -> &str {
        &self.initial_notice
    }

    pub(crate) fn final_slot_turn_cap(&self) -> Option<u32> {
        self.final_slot().then(|| {
            self.display
                .as_deref()
                .unwrap_or(self.captured.as_ref())
                .budget
                .per_run
                .max_model_turns
                .unwrap_or(qq_protocol::DEFAULT_GOAL_RUN_MODEL_TURNS)
        })
    }

    async fn refresh_display(&mut self) -> Result<(), SessionRuntimeError> {
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
        Ok(())
    }

    fn take_notice(&mut self) -> Option<String> {
        let Some(current) = &self.display else {
            return None;
        };
        if current.revision == self.displayed_revision {
            return None;
        }
        self.displayed_revision = current.revision;
        Some(crate::tools::goal::render_notice(current, now_ms()))
    }

    pub(crate) async fn refresh_notice(&mut self) -> Result<Option<String>, SessionRuntimeError> {
        self.refresh_display().await?;
        Ok(self.take_notice())
    }

    pub(crate) async fn refresh_handoff_notice(
        &mut self,
    ) -> Result<Option<String>, SessionRuntimeError> {
        self.refresh_display().await?;
        // A non-final candidate may still enter output repair or audit. Do not
        // acknowledge its new revision until a subsequent request carries it.
        Ok(if self.final_slot() {
            self.take_notice()
        } else {
            None
        })
    }

    pub(crate) fn post_summary_notice(&self, handoff: bool) -> Option<String> {
        self.display.as_ref().map(|goal| {
            let mut notice = crate::tools::goal::render_notice(goal, now_ms());
            if handoff {
                notice.push_str("\n\n");
                notice.push_str(Self::HANDOFF_NOTICE);
            } else if self.final_slot() {
                notice.push_str("\n\n");
                notice.push_str(Self::FINAL_RUN_NOTICE);
            }
            notice
        })
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
    policy: GoalPolicy,
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
    let policy = policy.validate()?;
    policy.validate_specification(
        &specification.budget,
        specification.check.as_ref(),
        Some(now),
    )?;
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
    goal_policy::save(connection, goal.goal_id, policy, Some(now))?;
    Ok(goal)
}

pub(super) fn control(
    connection: &Connection,
    session: SessionId,
    expected: GoalIdentity,
    action: GoalControl,
    now: u64,
    check_policy: Option<&Result<(), GoalCheckPolicyError>>,
    policy: Option<&Result<GoalPolicy, GoalPolicyError>>,
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
    if matches!(
        action,
        GoalControl::Resume | GoalControl::ApproveCheck { .. }
    ) {
        let policy = match policy {
            Some(result) => result.clone()?.validate()?,
            None => GoalPolicy::default(),
        };
        let (_, at) = goal_policy::load(connection, goal.goal_id)?;
        policy.validate_specification(&goal.budget, goal.check.as_ref(), at)?;
        goal_policy::save(connection, goal.goal_id, policy, at)?;
    }
    match action {
        GoalControl::Clear => {}
        GoalControl::Pause => {
            goal.status = GoalStatus::Paused {
                reason: GoalPauseReason::User,
            }
        }
        GoalControl::Resume => {
            if paid_obligations(connection, session, None)?
                .iter()
                .any(|run| run.cancelled)
            {
                return Err(SessionRuntimeError::GoalCancellationPending);
            }
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

/// Freeze active attribution at reservation; captured absence at first paid admission.
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

/// Immutable paid-run ancestry. Session ownership alone cannot sponsor later prompts.
pub(super) fn paid_goal_owners(
    connection: &Connection,
    mut run_id: RunId,
) -> Result<Vec<(RunId, SessionId, Option<qq_protocol::GoalId>)>, SessionRuntimeError> {
    let mut owners = Vec::new();
    let mut expected: Option<(SessionId, WorkspaceId, u16)> = None;
    for _ in 0..=MAX_CHILD_DEPTH {
        let (session,workspace,depth,parent_session,session_owner,kind,goal,anchor,origin)=
            connection.query_row("SELECT r.session_id,s.workspace_id,s.depth,s.parent_id,s.owner_run_id,r.kind,r.goal_id,r.goal_paid_owner_run_id,r.origin_json FROM runs r JOIN sessions s ON s.id=r.session_id WHERE r.id=?1",[run_id.to_string()],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,u16>(2)?,row.get::<_,Option<String>>(3)?,row.get::<_,Option<String>>(4)?,row.get::<_,String>(5)?,row.get::<_,Option<String>>(6)?,row.get::<_,Option<String>>(7)?,row.get::<_,Option<String>>(8)?)))?;
        let session: SessionId = parse_id(&session)?;
        let workspace: WorkspaceId = parse_id(&workspace)?;
        if kind != "prompt"
            || depth > MAX_CHILD_DEPTH
            || expected.is_some_and(|expected| expected != (session, workspace, depth))
            || owners.iter().any(|(previous, _, _)| *previous == run_id)
        {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        owners.push((run_id, session, goal.as_deref().map(parse_id).transpose()?));
        let Some(anchor) = anchor else {
            return Ok(owners);
        };
        let anchor_id: RunId = parse_id(&anchor)?;
        let origin: qq_protocol::RunOrigin =
            serde_json::from_str(origin.as_deref().ok_or(SessionRuntimeError::CONSTRAINT)?)?;
        if !matches!(origin,qq_protocol::RunOrigin::Delegated{owner_run_id,..} if owner_run_id==anchor_id)
            || session_owner.as_deref() != Some(anchor.as_str())
            || depth == 0
        {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        let parent: SessionId = parse_id(
            parent_session
                .as_deref()
                .ok_or(SessionRuntimeError::CONSTRAINT)?,
        )?;
        expected = Some((parent, workspace, depth - 1));
        run_id = anchor_id;
    }
    Err(SessionRuntimeError::CONSTRAINT)
}

/// Called only behind terminal settlement's durable outcome guard. Archived
/// goals receive their own spend; only live rows produce client updates.
pub(super) fn finalize_spend(
    connection: &Connection,
    session_id: SessionId,
    run_id: RunId,
) -> Result<Vec<GoalSnapshot>, SessionRuntimeError> {
    let (base,own_goal,anchored,usage_json,cost,completed,uncertain,admitted)=connection.query_row(
        "SELECT CASE WHEN r.kind='prompt' THEN r.id ELSE owner.id END,
            CASE WHEN r.kind='prompt' THEN r.goal_id ELSE owner.goal_id END,
            r.goal_paid_owner_run_id IS NOT NULL OR owner.goal_paid_owner_run_id IS NOT NULL,
            r.usage_json,r.estimated_cost_usd_nanos,
            EXISTS(SELECT 1 FROM model_turns WHERE run_id=r.id AND work_disposition='committed'),
            r.goal_request_pending OR EXISTS(SELECT 1 FROM model_turns WHERE run_id=r.id AND work_disposition='uncertain'),r.goal_helper_admitted
         FROM runs r LEFT JOIN runs owner ON r.kind='compaction' AND r.auto_compaction=1
           AND owner.id=r.auto_compaction_for_run_id AND owner.session_id=r.session_id AND owner.kind='prompt'
         WHERE r.id=?1 AND r.session_id=?2",
        params![run_id.to_string(),session_id.to_string()],
        |row|Ok((row.get::<_,Option<String>>(0)?,row.get::<_,Option<String>>(1)?,row.get::<_,bool>(2)?,row.get::<_,Option<String>>(3)?,row.get::<_,Option<u64>>(4)?,row.get::<_,bool>(5)?,row.get::<_,bool>(6)?,row.get::<_,bool>(7)?)),
    )?;
    if own_goal.is_none() && !anchored {
        return Ok(Vec::new());
    }
    let Some(base) = base else {
        return Err(SessionRuntimeError::CONSTRAINT);
    };
    let base: RunId = parse_id(&base)?;
    let terminal = if own_goal.is_some() && base == run_id {
        let (revision, outcome, finished, model_audit, final_output): (u32, String, u64,bool,Option<String>) = connection.query_row(
            "SELECT goal_revision,outcome_json,finished_at_ms,
                COALESCE(json_extract(goal_snapshot_json,'$.audit_pending')=1 AND json_type(goal_snapshot_json,'$.check')='null'
                    AND json_extract(origin_json,'$.type')='goal',0),final_output_json FROM runs WHERE id=?1",
            [run_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?,row.get(3)?,row.get(4)?)),
        )?;
        Some((
            revision,
            serde_json::from_str::<RunOutcome>(&outcome)?,
            finished,
            model_audit,
            final_output,
        ))
    } else {
        None
    };
    let owners = paid_goal_owners(connection, base)?;
    let usage: Option<qq_protocol::TokenUsage> = usage_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?;
    let has_spend = completed
        || uncertain
        || admitted
        || cost.is_some_and(|cost| cost > 0)
        || usage.is_some_and(|usage| {
            usage.input_tokens > 0
                || usage.output_tokens > 0
                || usage.cache_read_input_tokens > 0
                || usage.cache_write_input_tokens > 0
        });
    let mut updates = Vec::new();
    let mut charged = Vec::new();
    for (owner, goal_session, id) in owners {
        let Some(id) = id else { continue };
        let (payload, live): (String, bool) = connection.query_row(
            "SELECT payload_json,live FROM session_goals WHERE goal_id=?1 AND session_id=?2",
            params![id.to_string(), goal_session.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let mut goal: GoalSnapshot = serde_json::from_str(&payload)?;
        if goal.goal_id != id || goal.session_id != goal_session {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        if charged.contains(&id) {
            continue;
        };
        charged.push(id);
        let direct = owner == run_id;
        let mut changed = false;
        if direct && completed {
            goal.counters.runs = match goal.counters.runs.checked_add(1) {
                Some(runs) => runs,
                None => return Err(SessionRuntimeError::CONSTRAINT),
            };
            changed = true;
        }
        if has_spend {
            goal.counters.usage = match (goal.counters.usage, usage) {
                (Some(total), Some(usage)) => execution::add_usage(total, usage),
                _ => None,
            };
            goal.counters.estimated_cost_usd_nanos =
                match (goal.counters.estimated_cost_usd_nanos, cost) {
                    (Some(total), Some(cost)) => total.checked_add(cost),
                    _ => None,
                };
            changed = true;
        }
        if uncertain {
            goal.counters.usage = None;
            goal.counters.estimated_cost_usd_nanos = None;
            changed = true;
        }
        if direct && goal.pending_run_id == Some(run_id) {
            goal.pending_run_id = None;
            changed = true;
        }
        if live
            && direct
            && goal.status == GoalStatus::Active
            && let Some((revision, outcome, now, model_audit, final_output)) = &terminal
            && goal.revision == *revision
        {
            changed |=
                apply_terminal_outcome(connection, &mut goal, run_id, outcome, completed, *now)?;
            if *model_audit
                && matches!(
                    outcome,
                    RunOutcome::Completed
                        | RunOutcome::BudgetExhausted { .. }
                        | RunOutcome::Failed { .. }
                )
                && goal.status == GoalStatus::Active
                && goal.audit_pending
                && goal.audit_revision == Some(*revision)
            {
                let verdict = final_output
                    .as_deref()
                    .map(serde_json::from_str::<FinalOutput>)
                    .transpose()?;
                let confirmed = matches!(verdict,Some(FinalOutput::Valid {ref value,..})
                    if value.get("achieved").and_then(serde_json::Value::as_bool)==Some(true)
                        && value.get("evidence").and_then(serde_json::Value::as_str).is_some_and(|evidence|!evidence.trim().is_empty()))
                    && matches!(outcome, RunOutcome::Completed);
                goal.audit_pending = false;
                goal.audit_revision = None;
                if confirmed {
                    goal.status = GoalStatus::Achieved;
                } else {
                    goal.achievement_claim = None;
                }
                changed = true;
            }
        }
        if changed {
            save(connection, &goal)?;
            if live {
                updates.push(goal);
            }
        }
    }
    Ok(updates)
}

/// Outcomes never borrow authority from a newer live goal revision.
fn apply_terminal_outcome(
    connection: &Connection,
    goal: &mut GoalSnapshot,
    run: RunId,
    outcome: &RunOutcome,
    committed: bool,
    now: u64,
) -> Result<bool, SessionRuntimeError> {
    let previous = (
        goal.counters.failure_streak,
        goal.counters.no_progress_streak,
        goal.next_run_at_ms,
        goal.wait.as_ref().map(|wait| wait.until_ms),
    );
    let unproductive_failure = matches!(outcome,RunOutcome::Failed{failure} if !matches!(failure.kind,
        RunFailureKind::Authentication|RunFailureKind::ProviderAuthentication|RunFailureKind::Configuration|RunFailureKind::ProviderConfiguration))
        || (!committed && matches!(outcome, RunOutcome::BudgetExhausted { .. }));
    if !matches!(outcome, RunOutcome::Cancelled | RunOutcome::Interrupted) {
        if !unproductive_failure {
            goal.counters.failure_streak = 0;
        }
        if !matches!(outcome,RunOutcome::Paused{pause} if matches!(pause.as_ref(),qq_protocol::RunPause::NoProgress{..}))
        {
            goal.counters.no_progress_streak = 0;
        }
    }
    match outcome {
        RunOutcome::Completed => {
            goal.counters.failure_streak = 0;
            goal.counters.no_progress_streak = 0;
            if goal.wait.as_ref().is_some_and(|wait| wait.until_ms <= now) {
                goal.wait = None;
            }
            goal.next_run_at_ms = goal
                .wait
                .as_ref()
                .map_or(now, |wait| wait.until_ms)
                .min(goal.budget.deadline_ms);
        }
        RunOutcome::Failed { failure } => {
            let cause = match failure.kind {
                RunFailureKind::Authentication | RunFailureKind::ProviderAuthentication => {
                    Some(GoalNeedsUser::Authentication {
                        message: super::codec::truncate_utf8(failure.message.clone(), 1024),
                    })
                }
                RunFailureKind::Configuration | RunFailureKind::ProviderConfiguration => {
                    Some(GoalNeedsUser::Configuration {
                        message: super::codec::truncate_utf8(failure.message.clone(), 1024),
                    })
                }
                _ => None,
            };
            if let Some(cause) = cause {
                goal.status = GoalStatus::Paused {
                    reason: GoalPauseReason::NeedsUser { cause },
                };
            } else {
                goal.counters.failure_streak = goal.counters.failure_streak.saturating_add(1);
                if goal.counters.failure_streak >= 3 {
                    goal.status = GoalStatus::Blocked {
                        reason: qq_protocol::GoalBlockedReason::RepeatedFailure,
                    };
                } else {
                    goal.next_run_at_ms = now.min(goal.budget.deadline_ms);
                }
            }
        }
        RunOutcome::Paused { pause } => match pause.as_ref() {
            qq_protocol::RunPause::NoProgress { .. } => {
                goal.counters.no_progress_streak =
                    goal.counters.no_progress_streak.saturating_add(1);
                if goal.counters.no_progress_streak >= 2 {
                    goal.status = GoalStatus::Blocked {
                        reason: qq_protocol::GoalBlockedReason::NoProgress,
                    };
                } else {
                    goal.next_run_at_ms = now.min(goal.budget.deadline_ms);
                }
            }
            qq_protocol::RunPause::ProviderRetry { .. } => {
                // Reuse durable terminal evidence, independently of the other-failure
                // streak. Neutral stops without work do not consume the five-row
                // lookback; six retry pauses reach the 30-minute ceiling.
                let mut previous=connection.prepare("SELECT outcome_json,EXISTS(SELECT 1 FROM model_turns WHERE run_id=runs.id AND work_disposition='committed') FROM runs
                    WHERE session_id=?1 AND goal_id=?2 AND goal_revision=?3 AND kind='prompt'
                      AND id!=?4 AND outcome_json IS NOT NULL
                      AND ((json_extract(outcome_json,'$.type') IS NOT 'cancelled'
                            AND json_extract(outcome_json,'$.type') IS NOT 'interrupted')
                           OR EXISTS(SELECT 1 FROM model_turns WHERE run_id=runs.id AND work_disposition='committed'))
                    ORDER BY finished_at_ms DESC,rowid DESC LIMIT 5")?;
                let mut rows = previous.query(params![
                    goal.session_id.to_string(),
                    goal.goal_id.to_string(),
                    goal.revision,
                    run.to_string()
                ])?;
                let mut streak = 0;
                while !committed && let Some(row) = rows.next()? {
                    if row.get::<_, bool>(1)? {
                        break;
                    }
                    let payload: String = row.get(0)?;
                    if !matches!(serde_json::from_str::<RunOutcome>(&payload)?,RunOutcome::Paused{pause}
                        if matches!(pause.as_ref(),qq_protocol::RunPause::ProviderRetry{..}))
                    {
                        break;
                    }
                    streak += 1;
                }
                let policy = goal_policy::load(connection, goal.goal_id)?.0;
                let delay = (60_000_u64 << streak).min(policy.max_backoff_ms);
                goal.next_run_at_ms = now.saturating_add(delay).min(goal.budget.deadline_ms);
            }
        },
        RunOutcome::BudgetExhausted { .. } => {
            if let Some(reason) = goal_exhaustion(goal, now) {
                goal.status = GoalStatus::BudgetExhausted { reason };
            } else {
                if !committed {
                    goal.counters.failure_streak = goal.counters.failure_streak.saturating_add(1);
                    if goal.counters.failure_streak >= 3 {
                        goal.status = GoalStatus::Blocked {
                            reason: qq_protocol::GoalBlockedReason::RepeatedFailure,
                        };
                    }
                }
                goal.next_run_at_ms = now.min(goal.budget.deadline_ms);
            }
        }
        RunOutcome::Cancelled | RunOutcome::Interrupted => {}
    }
    if goal.status == GoalStatus::Active {
        if let Some(wait) = &goal.wait {
            goal.next_run_at_ms = goal
                .next_run_at_ms
                .max(wait.until_ms)
                .min(goal.budget.deadline_ms);
        }
        if let Some(reason) = goal_exhaustion(goal, now) {
            goal.status = GoalStatus::BudgetExhausted { reason };
        }
    }
    Ok(goal.status != GoalStatus::Active
        || previous
            != (
                goal.counters.failure_streak,
                goal.counters.no_progress_streak,
                goal.next_run_at_ms,
                goal.wait.as_ref().map(|wait| wait.until_ms),
            ))
}

/// Captured tool authority is separate from current paid-work admission.
#[derive(Clone, Copy)]
pub(super) enum GoalAdmission {
    Allowed {
        final_slot: bool,
        deadline_ms: u64,
        anchor_ms: u64,
        execution_started: Option<tokio::time::Instant>,
    },
    Exhausted(qq_protocol::GoalExhaustion),
    Cancelled,
}

pub(super) fn admit_run(
    connection: &Connection,
    captured: &GoalSnapshot,
    origin: Option<qq_protocol::RunOrigin>,
    limits: &mut qq_protocol::RunLimits,
    now: u64,
    anchor_ms: u64,
) -> Result<GoalAdmission, SessionRuntimeError> {
    let (payload, live): (String, bool) = connection.query_row(
        "SELECT payload_json,live FROM session_goals WHERE goal_id=?1 AND session_id=?2",
        params![
            captured.goal_id.to_string(),
            captured.session_id.to_string()
        ],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let current: GoalSnapshot = serde_json::from_str(&payload)?;
    if !live || current.status != GoalStatus::Active {
        return Ok(GoalAdmission::Cancelled);
    }
    if let Some(reason) = goal_exhaustion(&current, now) {
        return Ok(GoalAdmission::Exhausted(reason));
    }
    let model_audit = is_model_audit(captured, origin);
    if model_audit
        && (current.revision != captured.revision
            || !current.audit_pending
            || current.audit_revision != Some(current.revision)
            || current.counters.audits >= MAX_GOAL_AUDITS)
    {
        return Ok(GoalAdmission::Cancelled);
    }
    let original_duration = limits.max_duration_ms;
    let final_slot = !model_audit
        && matches!(
            origin,
            Some(qq_protocol::RunOrigin::Goal { .. } | qq_protocol::RunOrigin::UserPrompt {})
        )
        && current.counters.runs.saturating_add(1) >= current.budget.max_goal_runs;
    // Queued automatic limits are a cache; prompts retain their own limits
    // except for the final slot's finite turn ceiling.
    if matches!(origin, Some(qq_protocol::RunOrigin::Goal { .. })) {
        *limits = current.budget.per_run;
    }
    if model_audit {
        limits.max_model_turns = Some(1);
    }

    // Prompt limits are caller-owned, but managed ceilings apply to every
    // execution claim and again after policy refresh at start.
    let (policy, _) = goal_policy::load(connection, current.goal_id)?;
    macro_rules! clamp {
        ($($field:ident),+ $(,)?) => { $(
            if let Some(cap) = policy.per_run.$field {
                limits.$field = Some(limits.$field.map_or(cap, |requested| requested.min(cap)));
            }
        )+ };
    }
    clamp!(max_duration_ms, max_model_turns, max_tool_calls, max_total_tokens,
        max_cost_usd_nanos, max_input_tokens, max_output_tokens, max_tool_output_bytes,
        max_children, max_concurrent_children);
    if final_slot {
        let cap = current
            .budget
            .per_run
            .max_model_turns
            .unwrap_or(qq_protocol::DEFAULT_GOAL_RUN_MODEL_TURNS);
        limits.max_model_turns = Some(
            limits
                .max_model_turns
                .map_or(cap, |existing| existing.min(cap)),
        );
    }
    let remaining = current.budget.deadline_ms.saturating_sub(anchor_ms);
    limits.max_duration_ms = Some(
        limits
            .max_duration_ms
            .map_or(remaining, |cap| cap.min(remaining)),
    );
    if let Some(original) = original_duration {
        limits.max_duration_ms = limits
            .max_duration_ms
            .map(|duration| duration.min(original));
    }
    if let Some(cap) = current.budget.max_total_tokens {
        let spent = current
            .counters
            .usage
            .ok_or(SessionRuntimeError::CONSTRAINT)?;
        let spent = spent
            .input_tokens
            .checked_add(spent.cache_read_input_tokens)
            .and_then(|n| n.checked_add(spent.cache_write_input_tokens))
            .and_then(|n| n.checked_add(spent.output_tokens))
            .ok_or(SessionRuntimeError::CONSTRAINT)?;
        let remaining = cap - spent;
        limits.max_total_tokens = Some(
            limits
                .max_total_tokens
                .map_or(remaining, |cap| cap.min(remaining)),
        );
    }
    if let Some(cap) = current.budget.max_cost_usd_nanos {
        let remaining = cap
            - current
                .counters
                .estimated_cost_usd_nanos
                .ok_or(SessionRuntimeError::CONSTRAINT)?;
        limits.max_cost_usd_nanos = Some(
            limits
                .max_cost_usd_nanos
                .map_or(remaining, |cap| cap.min(remaining)),
        );
    }
    Ok(GoalAdmission::Allowed {
        final_slot,
        deadline_ms: current.budget.deadline_ms,
        anchor_ms,
        execution_started: None,
    })
}

/// Sponsored descendants retain accounting ownership without goal tool authority.
/// Their inherited relative duration must also respect the owner's absolute deadline.
pub(super) fn admit_sponsored_run(
    connection: &Connection,
    run_id: RunId,
    own_admission: Option<GoalAdmission>,
    limits: &mut qq_protocol::RunLimits,
    now: u64,
    anchor_ms: u64,
) -> Result<GoalAdmission, SessionRuntimeError> {
    let (mut deadline_ms, final_slot) = match own_admission {
        Some(GoalAdmission::Allowed {
            deadline_ms,
            final_slot,
            ..
        }) => (deadline_ms, final_slot),
        Some(refusal) => return Ok(refusal),
        None => (u64::MAX, false),
    };
    let mut sponsored = false;
    for (_, session, goal) in paid_goal_owners(connection, run_id)?.into_iter().skip(1) {
        let Some(goal) = goal else { continue };
        let payload: String = connection.query_row(
            "SELECT payload_json FROM session_goals WHERE goal_id=?1 AND session_id=?2",
            params![goal.to_string(), session.to_string()],
            |row| row.get(0),
        )?;
        let owner: GoalSnapshot = serde_json::from_str(&payload)?;
        deadline_ms = deadline_ms.min(owner.budget.deadline_ms);
        sponsored = true;
    }
    if !sponsored {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    if now >= deadline_ms {
        return Ok(GoalAdmission::Exhausted(
            qq_protocol::GoalExhaustion::Deadline,
        ));
    }
    let remaining = deadline_ms.saturating_sub(anchor_ms);
    limits.max_duration_ms = Some(
        limits
            .max_duration_ms
            .map_or(remaining, |cap| cap.min(remaining)),
    );
    Ok(GoalAdmission::Allowed {
        final_slot,
        deadline_ms,
        anchor_ms,
        execution_started: None,
    })
}

pub(super) fn goal_exhaustion(
    goal: &GoalSnapshot,
    now: u64,
) -> Option<qq_protocol::GoalExhaustion> {
    if now >= goal.budget.deadline_ms {
        return Some(qq_protocol::GoalExhaustion::Deadline);
    }
    if goal.counters.runs >= goal.budget.max_goal_runs {
        return Some(qq_protocol::GoalExhaustion::Runs);
    }
    if let Some(limit) = goal.budget.max_total_tokens {
        let spent = goal.counters.usage.and_then(|usage| {
            usage
                .input_tokens
                .checked_add(usage.cache_read_input_tokens)
                .and_then(|value| value.checked_add(usage.cache_write_input_tokens))
                .and_then(|value| value.checked_add(usage.output_tokens))
        });
        match spent {
            None => return Some(qq_protocol::GoalExhaustion::TokensUnknown),
            Some(spent) if spent >= limit => return Some(qq_protocol::GoalExhaustion::Tokens),
            _ => {}
        }
    }
    if let Some(limit) = goal.budget.max_cost_usd_nanos {
        match goal.counters.estimated_cost_usd_nanos {
            None => return Some(qq_protocol::GoalExhaustion::CostUnknown),
            Some(spent) if spent >= limit => return Some(qq_protocol::GoalExhaustion::Cost),
            _ => {}
        }
    }
    None
}

pub(super) const PAID_WORK_CTE: &str = "WITH RECURSIVE paid(id,depth) AS (
    SELECT id,0 FROM runs WHERE session_id=?1 AND goal_id IS NOT NULL AND (?2 IS NULL OR goal_id=?2)
    UNION ALL SELECT child.id,paid.depth+1 FROM paid JOIN runs child ON child.goal_paid_owner_run_id=paid.id
      WHERE paid.depth<?3
), obligations(id) AS (
    SELECT id FROM paid UNION SELECT helper.id FROM paid JOIN runs owner ON owner.id=paid.id
    JOIN runs helper ON helper.auto_compaction_for_run_id=owner.id AND helper.session_id=owner.session_id
      AND helper.kind='compaction' AND helper.auto_compaction=1
)";

pub(super) struct PaidObligation {
    pub(super) run: RunId,
    pub(super) session: SessionId,
    pub(super) workspace: WorkspaceId,
    pub(super) queued: bool,
    pub(super) preparing: bool,
    pub(super) cancelled: bool,
    pub(super) autonomous: bool,
}

/// Roots include settled historical goal runs. Only their exact paid children
/// and same-session automatic helpers inherit cancellation ownership.
pub(super) fn paid_obligations(
    connection: &Connection,
    session: SessionId,
    goal: Option<GoalId>,
) -> Result<Vec<PaidObligation>, SessionRuntimeError> {
    let mut statement=connection.prepare(&format!("{PAID_WORK_CTE}
        SELECT r.id,r.session_id,s.workspace_id,r.status='queued',COALESCE(s.preparing_run_id=r.id,0),r.cancel_requested,
               COALESCE(json_extract(r.origin_json,'$.type') IN ('goal','goal_check'),0)
        FROM obligations JOIN runs r ON r.id=obligations.id JOIN sessions s ON s.id=r.session_id
        WHERE r.outcome_json IS NULL AND r.status IN ('queued','running')"))?;
    let mut rows = statement.query(params![
        session.to_string(),
        goal.map(|goal| goal.to_string()),
        MAX_CHILD_DEPTH
    ])?;
    let mut obligations = Vec::new();
    while let Some(row) = rows.next()? {
        obligations.push(PaidObligation {
            run: parse_id(&row.get::<_, String>(0)?)?,
            session: parse_id(&row.get::<_, String>(1)?)?,
            workspace: parse_id(&row.get::<_, String>(2)?)?,
            queued: row.get(3)?,
            preparing: row.get(4)?,
            cancelled: row.get(5)?,
            autonomous: row.get(6)?,
        });
    }
    Ok(obligations)
}

pub(super) struct GoalSchedule {
    pub(super) has_goals: bool,
    pub(super) next_wake: Option<u64>,
    pub(super) cursors: Vec<EventCursor>,
}

/// A single scheduler pass commits at most 32 fresh goal runs. The pending
/// guard and idle check share the transaction with their durable queue rows.
pub(super) fn queue_due_goals(
    connection: &mut Connection,
    store_id: StoreId,
    now: u64,
) -> Result<GoalSchedule, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let mut cursors = Vec::new();
    let drainage_cte = PAID_WORK_CTE
        .replace("?1", "s.id")
        .replace("?2", "NULL")
        .replace("?3", &MAX_CHILD_DEPTH.to_string());
    let eligible=format!("g.live=1 AND json_extract(g.payload_json,'$.status.type')='active'
        AND json_extract(g.payload_json,'$.pending_run_id') IS NULL
        AND s.active_run_id IS NULL AND s.preparing_run_id IS NULL AND s.queued_prompts=0
        AND NOT EXISTS(SELECT 1 FROM runs r WHERE r.session_id=s.id AND r.status IN ('queued','running'))
        AND NOT EXISTS({drainage_cte} SELECT 1 FROM obligations JOIN runs r ON r.id=obligations.id
            WHERE r.outcome_json IS NULL AND r.status IN ('queued','running'))");
    let mut candidates=transaction.prepare(&format!("SELECT g.payload_json,s.workspace_id FROM session_goals g
        JOIN sessions s ON s.id=g.session_id WHERE {eligible}
        AND MIN(json_extract(g.payload_json,'$.next_run_at_ms'),json_extract(g.payload_json,'$.budget.deadline_ms'))<=?1
        ORDER BY json_extract(g.payload_json,'$.next_run_at_ms'),g.goal_id LIMIT 32"))?;
    let rows = candidates.query_map([now], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (payload, workspace) = row?;
        let mut goal: GoalSnapshot = serde_json::from_str(&payload)?;
        let workspace: WorkspaceId = parse_id(&workspace)?;
        let input_tokens = goal.counters.usage.and_then(|usage| {
            usage
                .input_tokens
                .checked_add(usage.cache_read_input_tokens)
                .and_then(|value| value.checked_add(usage.cache_write_input_tokens))
                .and_then(|value| value.checked_add(usage.output_tokens))
        });
        let exhaustion = goal_exhaustion(&goal, now);
        if let Some(reason) = exhaustion {
            goal.status = GoalStatus::BudgetExhausted { reason };
            save(&transaction, &goal)?;
            cursors.push(
                append_event(
                    &transaction,
                    EventContext::for_session(store_id, workspace, goal.session_id, None, now),
                    SessionEvent::GoalUpdated {
                        goal: Box::new(goal),
                    },
                )?
                .cursor,
            );
            continue;
        }
        if goal.audit_pending && goal.counters.audits >= MAX_GOAL_AUDITS {
            goal.status = GoalStatus::Blocked {
                reason: qq_protocol::GoalBlockedReason::AuditExhausted,
            };
            save(&transaction, &goal)?;
            cursors.push(
                append_event(
                    &transaction,
                    EventContext::for_session(store_id, workspace, goal.session_id, None, now),
                    SessionEvent::GoalUpdated {
                        goal: Box::new(goal),
                    },
                )?
                .cursor,
            );
            continue;
        }
        if goal.audit_pending && goal.check.is_some() {
            cursors.extend(super::goal_check::queue(
                &transaction,
                store_id,
                workspace,
                &mut goal,
                now,
            )?);
            continue;
        }
        let run = RunId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
        let command = CommandId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
        let message = MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
        let assistant = MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
        let mut limits = goal.budget.per_run;
        let remaining = goal.budget.deadline_ms - now;
        limits.max_duration_ms = Some(
            limits
                .max_duration_ms
                .map_or(remaining, |limit| limit.min(remaining)),
        );
        if let Some(limit) = goal.budget.max_total_tokens {
            let remaining = limit - input_tokens.ok_or(SessionRuntimeError::CONSTRAINT)?;
            limits.max_total_tokens = Some(
                limits
                    .max_total_tokens
                    .map_or(remaining, |limit| limit.min(remaining)),
            );
        }
        if let Some(limit) = goal.budget.max_cost_usd_nanos {
            let remaining = limit
                - goal
                    .counters
                    .estimated_cost_usd_nanos
                    .ok_or(SessionRuntimeError::CONSTRAINT)?;
            limits.max_cost_usd_nanos = Some(
                limits
                    .max_cost_usd_nanos
                    .map_or(remaining, |limit| limit.min(remaining)),
            );
        }
        goal.pending_run_id = Some(run);
        let origin = qq_protocol::RunOrigin::Goal {
            goal_id: goal.goal_id,
            revision: goal.revision,
        };
        let notice = crate::tools::goal::render_notice(&goal, now);
        let model_audit = is_model_audit(&goal, Some(origin));
        let (prompt, contract) = if model_audit {
            limits.max_model_turns = Some(1);
            let contract = qq_protocol::OutputContract {
                schema: serde_json::json!({
                    "type":"object","properties":{"achieved":{"type":"boolean"},"evidence":{"type":"string","minLength":1,"maxLength":512}},
                    "required":["achieved","evidence"],"additionalProperties":false,
                }),
                repair_turns: 0,
            };
            crate::output::CompiledOutputSchema::compile(&contract)
                .map_err(|_| SessionRuntimeError::CONSTRAINT)?;
            (
                format!(
                    "Independently audit the current goal's achievement claim against the persisted work. Do not assume the claim is correct. Use one tool-free turn. Return JSON with achieved (boolean) and evidence (a specific nonempty reason, at most 512 characters). Confirm only if the evidence supports completion.\nClaim under review: {}",
                    serde_json::to_string(&goal.achievement_claim)?
                ),
                Some(serde_json::to_string(&contract)?),
            )
        } else {
            ("Work toward the current goal. Keep its checklist and notes current with update_goal.".to_owned(),None)
        };
        transaction.execute("INSERT INTO runs(id,session_id,command_id,user_message_id,assistant_message_id,status,created_at_ms,limits_json,origin_json,
            goal_id,goal_revision,goal_snapshot_json,goal_initial_notice,output_contract_json) VALUES(?1,?2,?3,?4,?5,'queued',?6,?7,?8,?9,?10,?11,?12,?13)",
            params![run.to_string(),goal.session_id.to_string(),command.to_string(),message.to_string(),assistant.to_string(),now,
                serde_json::to_string(&limits)?,serde_json::to_string(&origin)?,goal.goal_id.to_string(),goal.revision,serde_json::to_string(&goal)?,notice,contract])?;
        transaction.execute("INSERT INTO messages(id,session_id,run_id,ordinal,role,state,output,created_at_ms)
            VALUES(?1,?2,?3,(SELECT COALESCE(MAX(ordinal),0)+1 FROM messages WHERE session_id=?2),'user','queued',?4,?5)",
            params![message.to_string(),goal.session_id.to_string(),run.to_string(),prompt,now])?;
        transaction.execute(
            "UPDATE sessions SET status='queued',queued_prompts=1,updated_at_ms=?2 WHERE id=?1",
            params![goal.session_id.to_string(), now],
        )?;
        save(&transaction, &goal)?;
        cursors.push(
            append_event(
                &transaction,
                EventContext::for_session(store_id, workspace, goal.session_id, None, now),
                SessionEvent::GoalUpdated {
                    goal: Box::new(goal.clone()),
                },
            )?
            .cursor,
        );
        cursors.push(
            append_event(
                &transaction,
                EventContext::for_run_ids(store_id, workspace, goal.session_id, run, None, now),
                SessionEvent::PromptQueued {
                    session: Box::new(load_session_summary(&transaction, goal.session_id)?),
                    message: load_message(&transaction, message)?,
                    run: Box::new(load_run(&transaction, run)?),
                    queue_position: 1,
                },
            )?
            .cursor,
        );
    }
    drop(candidates);
    let next:Option<u64>=transaction.query_row(&format!("SELECT MIN(MIN(json_extract(g.payload_json,'$.next_run_at_ms'),json_extract(g.payload_json,'$.budget.deadline_ms')))
        FROM session_goals g JOIN sessions s ON s.id=g.session_id WHERE {eligible}"),[],|row|row.get(0))?;
    let has_goals: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_goals WHERE live=1)",
        [],
        |row| row.get(0),
    )?;
    transaction.commit()?;
    Ok(GoalSchedule {
        has_goals,
        next_wake: next,
        cursors,
    })
}

pub(super) fn start_model_audit(
    connection: &Connection,
    identity: RunIdentity,
    captured: Option<&GoalSnapshot>,
    origin: Option<qq_protocol::RunOrigin>,
) -> Result<Option<GoalSnapshot>, SessionRuntimeError> {
    let Some(captured) = captured.filter(|goal| is_model_audit(goal, origin)) else {
        return Ok(None);
    };
    let Some(mut goal) = load_live(connection, identity.session_id)? else {
        return Err(SessionRuntimeError::CONSTRAINT);
    };
    if goal.goal_id != captured.goal_id
        || goal.revision != captured.revision
        || goal.status != GoalStatus::Active
        || !goal.audit_pending
        || goal.audit_revision != Some(goal.revision)
        || goal.pending_run_id != Some(identity.run_id)
        || goal.counters.audits >= MAX_GOAL_AUDITS
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    goal.counters.audits += 1;
    save(connection, &goal)?;
    Ok(Some(goal))
}
