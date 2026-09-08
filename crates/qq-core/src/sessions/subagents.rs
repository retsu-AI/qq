use super::runtime::SessionRuntimeInner;
use super::*;
use crate::runtime::{ChildCleanupError, ChildDrainFuture};
use qq_protocol::ChildAuthority;

type ChildReceipt = Result<SpawnAgentSpend, ChildCleanupError>;

#[cfg(test)]
struct ChildDeliveryHook {
    session_id: SessionId,
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

#[cfg(test)]
static CHILD_DELIVERY_HOOKS: Mutex<Vec<ChildDeliveryHook>> = Mutex::new(Vec::new());

#[cfg(test)]
pub(super) fn hold_child_delivery(
    session_id: SessionId,
) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (entered, entered_rx) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    CHILD_DELIVERY_HOOKS
        .lock()
        .unwrap()
        .push(ChildDeliveryHook {
            session_id,
            entered,
            release: release_rx,
        });
    (entered_rx, release)
}

struct ChildTask {
    cancel: watch::Sender<bool>,
    completed: watch::Receiver<Option<ChildReceipt>>,
}

/// Owners and unconsumed receipts stay here even if a tool or a cleanup await
/// is interrupted. Only a charged receipt or a fully drained batch is removed.
#[derive(Default)]
pub(super) struct ChildTasks {
    tasks: Mutex<HashMap<ToolCallId, ChildTask>>,
}

impl ChildTasks {
    fn acknowledge(&self, call_id: ToolCallId) {
        self.tasks
            .lock()
            .expect("child owner registry poisoned")
            .remove(&call_id);
    }

    pub(super) async fn drain(&self) -> Result<Vec<SpawnAgentSpend>, ChildCleanupError> {
        let pending = {
            let tasks = self.tasks.lock().map_err(|_| ChildCleanupError)?;
            tasks
                .values()
                .map(|task| {
                    task.cancel.send_replace(true);
                    task.completed.clone()
                })
                .collect::<Vec<_>>()
        };
        for mut completed in pending {
            while completed.borrow().is_none() {
                if completed.changed().await.is_err() {
                    return Err(ChildCleanupError);
                }
            }
        }
        // Nothing is removed before the last await: a cancelled drain can be
        // resumed by the session owner without losing any child's accounting.
        let mut tasks = self.tasks.lock().map_err(|_| ChildCleanupError)?;
        let mut spend = Vec::with_capacity(tasks.len());
        for task in tasks.values() {
            match *task.completed.borrow() {
                Some(Ok(receipt)) => spend.push(receipt),
                Some(Err(error)) => return Err(error),
                None => return Err(ChildCleanupError),
            }
        }
        tasks.clear();
        Ok(spend)
    }
}

struct CancelChildWaiter(watch::Sender<bool>);

impl Drop for CancelChildWaiter {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

async fn child_cancelled(cancel: &mut watch::Receiver<bool>) {
    while !*cancel.borrow() {
        if cancel.changed().await.is_err() {
            return;
        }
    }
}

/// Executes `spawn_agent` calls for one parent run: creates an authority-limited child
/// session and queued task atomically (while preserving the ordinary durable
/// event stream), then resolves with the child run's final assistant text.
pub(super) struct SessionSubagentSpawner {
    inner: Arc<SessionRuntimeInner>,
    parent: ClaimedRun,
    /// Bounds this run's children in flight; excess spawn calls in one turn
    /// wait here rather than erroring.
    slots: Arc<Semaphore>,
    /// One write child at a time per parent run: two writers must never
    /// share the checkout. Held in addition to a `slots` permit.
    write_slot: Arc<Semaphore>,
    /// Whether the plan's roster permits write children at all.
    write_children: bool,
    /// Children this run has spawned so far, capped at `max_children`.
    spawned: Arc<AtomicUsize>,
    /// The effective total-children bound: the caller's `max_children` when
    /// imposed (admission already capped it), else the runtime ceiling.
    max_children: usize,
    tasks: Arc<ChildTasks>,
}

impl SessionSubagentSpawner {
    pub(super) fn new(
        inner: Arc<SessionRuntimeInner>,
        parent: ClaimedRun,
        tasks: Arc<ChildTasks>,
    ) -> Self {
        let concurrent = parent
            .limits
            .max_concurrent_children
            .map_or(usize::from(MAX_CONCURRENT_CHILDREN_PER_RUN), usize::from)
            .min(usize::from(MAX_CONCURRENT_CHILDREN_PER_RUN));
        let max_children = parent
            .limits
            .max_children
            .map_or(usize::from(MAX_SPAWNED_CHILDREN_PER_RUN), usize::from)
            .min(usize::from(MAX_SPAWNED_CHILDREN_PER_RUN));
        Self {
            inner,
            parent,
            slots: Arc::new(Semaphore::new(concurrent)),
            write_slot: Arc::new(Semaphore::new(1)),
            write_children: false,
            spawned: Arc::new(AtomicUsize::new(0)),
            max_children,
            tasks,
        }
    }

    /// Permits `authority: write` spawns. Set from the compiled plan's roster.
    pub(super) fn with_write_children(mut self, write_children: bool) -> Self {
        self.write_children = write_children;
        self
    }
}

impl SubagentSpawner for SessionSubagentSpawner {
    fn spawn(&self, request: SpawnRequest) -> SpawnAgentFuture {
        let inner = Arc::clone(&self.inner);
        let parent = self.parent.clone();
        let slots = Arc::clone(&self.slots);
        let budget = SpawnBudget {
            slots,
            write_slot: Arc::clone(&self.write_slot),
            write_children: self.write_children,
            spawned: Arc::clone(&self.spawned),
            max_children: self.max_children,
            tasks: Arc::clone(&self.tasks),
        };
        Box::pin(async move { spawn_child_run(inner, parent, budget, request).await })
    }

    fn acknowledge(&self, call_id: ToolCallId) {
        self.tasks.acknowledge(call_id);
    }

    fn drain(&self) -> ChildDrainFuture {
        let tasks = Arc::clone(&self.tasks);
        Box::pin(async move { tasks.drain().await })
    }
}

/// The per-run child bookkeeping one spawn call draws from.
pub(super) struct SpawnBudget {
    pub(super) slots: Arc<Semaphore>,
    pub(super) write_slot: Arc<Semaphore>,
    pub(super) write_children: bool,
    pub(super) spawned: Arc<AtomicUsize>,
    pub(super) max_children: usize,
    pub(super) tasks: Arc<ChildTasks>,
}

/// A child that never ran spent nothing. Failed children report their real
/// spend through `spawn_error_with_spend` so the parent budget still sees it.
fn spawn_error(content: impl Into<String>) -> SpawnAgentOutcome {
    spawn_error_with_spend(content, SpawnAgentSpend::NONE)
}

fn spawn_error_with_spend(content: impl Into<String>, spend: SpawnAgentSpend) -> SpawnAgentOutcome {
    SpawnAgentOutcome {
        content: content.into(),
        is_error: true,
        spend,
        session_id: None,
    }
}

/// One owned `spawn_agent` execution. Ordinary admission errors and settled
/// child failures become tool results. Uncertain ownership or teardown fails
/// the session runtime closed.
pub(super) async fn spawn_child_run(
    inner: Arc<SessionRuntimeInner>,
    parent: ClaimedRun,
    budget: SpawnBudget,
    request: SpawnRequest,
) -> SpawnAgentOutcome {
    let (cancel, cancellation) = watch::channel(false);
    let (completed, completion) = watch::channel(None);
    let (reply, response) = oneshot::channel();
    {
        let mut tasks = match budget.tasks.tasks.lock() {
            Ok(tasks) => tasks,
            Err(_) => {
                inner.failed.send_replace(true);
                return spawn_error_with_spend(
                    "child owner registry is unavailable",
                    SpawnAgentSpend::UNKNOWN,
                );
            }
        };
        if tasks.len() > usize::from(MAX_SPAWNED_CHILDREN_PER_RUN)
            || tasks.contains_key(&request.call_id)
        {
            return spawn_error("the child owner capacity was exhausted");
        }
        tasks.insert(
            request.call_id,
            ChildTask {
                cancel: cancel.clone(),
                completed: completion,
            },
        );
    }
    let deadline = request.budget.deadline;
    let deadline_cancel = cancel.clone();
    let _waiter = CancelChildWaiter(cancel);
    #[cfg(test)]
    let parent_session = parent.session_id;
    let owner_inner = Arc::clone(&inner);
    tokio::spawn(async move {
        let outcome = AssertUnwindSafe(async {
            let mut execution = std::pin::pin!(run_owned_child(
                Arc::clone(&owner_inner),
                parent,
                budget,
                request,
                cancellation,
            ));
            let expires = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                biased;
                () = expires => {
                    deadline_cancel.send_replace(true);
                    // Cancellation retains the H23 owner through accepted
                    // creation, started loader work, and execution teardown.
                    let mut outcome = execution.await;
                    outcome.content = "the sub-agent could not finish: its duration budget is spent".to_owned();
                    outcome.is_error = true;
                    outcome
                }
                outcome = &mut execution => outcome,
            }
        })
        .catch_unwind()
        .await;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(_) => {
                owner_inner.failed.send_replace(true);
                spawn_error_with_spend("child owner task panicked", SpawnAgentSpend::UNKNOWN)
            }
        };
        let receipt = if *owner_inner.failed.borrow() {
            Err(ChildCleanupError)
        } else {
            Ok(outcome.spend)
        };
        completed.send_replace(Some(receipt));
        let _ = reply.send(outcome);
    });
    match response.await {
        Ok(outcome) => {
            #[cfg(test)]
            {
                let hook = {
                    let mut hooks = CHILD_DELIVERY_HOOKS.lock().unwrap();
                    hooks
                        .iter()
                        .position(|hook| hook.session_id == parent_session)
                        .map(|index| hooks.remove(index))
                };
                if let Some(hook) = hook {
                    let _ = hook.entered.send(());
                    let _ = hook.release.await;
                }
            }
            outcome
        }
        Err(_) => {
            inner.failed.send_replace(true);
            spawn_error_with_spend("child owner task disappeared", SpawnAgentSpend::UNKNOWN)
        }
    }
}

async fn run_owned_child(
    inner: Arc<SessionRuntimeInner>,
    parent: ClaimedRun,
    budget: SpawnBudget,
    request: SpawnRequest,
    mut cancellation: watch::Receiver<bool>,
) -> SpawnAgentOutcome {
    let SpawnBudget {
        slots,
        write_slot,
        write_children,
        spawned,
        max_children,
        tasks: _,
    } = budget;
    let SpawnRequest {
        call_id,
        task,
        model,
        authority,
        budget: child_budget,
        purpose,
    } = request;
    // Authority attenuates strictly: a read child is ReadOnly; a write child
    // is Supervised and needs both the roster's permission and a reviewer to
    // adjudicate its actions. Only depth one may write (children have no
    // spawner at all, so this is structural). A ReadOnly parent cannot grant
    // write: its own policy already denied the mutating spawn call.
    let child_mode = match authority {
        ChildAuthority::Read => ApprovalMode::ReadOnly,
        ChildAuthority::Write => {
            if !write_children {
                return spawn_error(
                    "write sub-agents are not enabled: set delegation.write_children = true in \
                     the configuration, or spawn with authority read",
                );
            }
            if inner.approval_reviewer.is_none() {
                return spawn_error(
                    "write sub-agents require a configured reviewer_model to adjudicate their \
                     actions; spawn with authority read instead",
                );
            }
            ApprovalMode::Supervised
        }
    };
    // The budget counts attempts, so a failing spawn also consumes it. A
    // refused spawn is a tool error the model can act on, never a terminal
    // outcome: ending the parent because it asked for one child too many
    // would discard the work it already did.
    if spawned.fetch_add(1, Ordering::AcqRel) >= max_children {
        return spawn_error(format!(
            "this run already spawned {max_children} sub-agents; \
             continue with what they returned"
        ));
    }
    // Preparation and execution share the same child bounds. A started loader
    // may own blocking work, so cancellation retains its permit until it exits.
    let acquire = async {
        // Wait for a child slot before creating anything: spawn calls queued
        // behind the concurrency cap must not pile up sessions they cannot run.
        let Ok(_slot) = Arc::clone(&slots).acquire_owned().await else {
            return Err(spawn_error("the sub-agent scheduler is unavailable"));
        };
        // Writers serialize behind the read slot too: a second write spawn in the
        // same turn waits here until the first child settles.
        let _write_slot = if child_mode == ApprovalMode::Supervised {
            match Arc::clone(&write_slot).acquire_owned().await {
                Ok(permit) => Some(permit),
                Err(_) => return Err(spawn_error("the sub-agent scheduler is unavailable")),
            }
        } else {
            None
        };
        Ok((_slot, _write_slot))
    };
    let (_slot, _write_slot) = tokio::select! {
        biased;
        () = child_cancelled(&mut cancellation) => return spawn_error("the sub-agent admission was cancelled"),
        acquired = acquire => match acquired {
            Ok(acquired) => acquired,
            Err(outcome) => return outcome,
        },
    };
    let prepare_cancel = cancellation.clone();
    let prepared = async {
        if *prepare_cancel.borrow() {
            return Err(spawn_error("the sub-agent admission was cancelled"));
        }
        let mut selection = parent.model.clone();
        if let Some(model) = model.and_then(|model| {
            let model = model.trim().to_owned();
            (!model.is_empty()).then_some(model)
        }) {
            selection.model = Some(model);
        } else {
            selection = match inner
                .loader
                .resolve_worker_model(parent.workspace.clone(), selection)
                .await
            {
                Ok(selection) => selection,
                Err(error) => {
                    return Err(spawn_error(format!(
                        "the sub-agent model could not be resolved: {}",
                        truncate_utf8(error.message, MAX_FAILURE_MESSAGE_BYTES)
                    )));
                }
            };
        }
        if *prepare_cancel.borrow() {
            return Err(spawn_error("the sub-agent admission was cancelled"));
        }
        // The spawn-time choke point: every resolved route — explicit argument,
        // configured worker, or parent fallback — must be authenticated and in
        // the served model list right now, before any durable child state exists.
        if let Err(error) = inner
            .loader
            .validate_spawn_model(parent.workspace.clone(), selection.clone())
            .await
        {
            return Err(spawn_error(format!(
                "the sub-agent model was rejected: {}",
                truncate_utf8(error.message, MAX_FAILURE_MESSAGE_BYTES)
            )));
        }
        if *prepare_cancel.borrow() {
            return Err(spawn_error("the sub-agent admission was cancelled"));
        }
        // Validate and construct the selected runtime before creating durable
        // child state. A bad explicit/configured route must leave no orphan
        // session (and this runtime will be reused from the loader cache when the
        // queued child is claimed).
        if let Err(error) = inner
            .loader
            .load(RuntimeLoadRequest {
                workspace: parent.workspace.clone(),
                model: selection.clone(),
                profile: parent.profile.clone(),
            })
            .await
        {
            return Err(spawn_error(format!(
                "the sub-agent model could not be loaded: {}",
                truncate_utf8(error.message, MAX_FAILURE_MESSAGE_BYTES)
            )));
        }
        Ok(selection)
    };
    let mut prepared = std::pin::pin!(prepared);
    let selection = tokio::select! {
        biased;
        () = child_cancelled(&mut cancellation) => {
            let _ = prepared.await;
            return spawn_error("the sub-agent admission was cancelled");
        },
        prepared = &mut prepared => match prepared {
            Ok(selection) => selection,
            Err(outcome) => return outcome,
        },
    };
    // Shutdown owns the write side of this gate. Hold the read side across
    // the durable child transaction so shutdown observes either no child run
    // or the fully queued run it must settle; child admission can never cross
    // the shutdown scan.
    let lifecycle = tokio::select! {
        biased;
        () = child_cancelled(&mut cancellation) => return spawn_error("the sub-agent admission was cancelled"),
        lifecycle = inner.lifecycle.read() => lifecycle,
    };
    if *inner.shutdown.borrow() || *inner.failed.borrow() {
        return spawn_error("the session runtime is shutting down");
    }
    let child_limits = match child_budget.limits_at(tokio::time::Instant::now()) {
        Ok(limits) => limits,
        Err(kind) => {
            return spawn_error(format!(
                "the sub-agent cannot start: its {} budget is spent",
                kind.as_str()
            ));
        }
    };
    let created = match inner
        .store
        .create_child_run(
            &parent,
            call_id,
            ChildAdmission {
                model: selection,
                task,
                limits: child_limits,
                approval_mode: child_mode,
                purpose,
            },
        )
        .await
    {
        Ok(created) => created,
        Err(error) => {
            return spawn_error(format!(
                "the sub-agent session and task could not be created: {error}"
            ));
        }
    };
    let CreatedChildRun {
        session_id: child_session_id,
        run_id,
        committed_through,
    } = created;
    let mut guard = CancelChildOnDrop {
        inner: Arc::clone(&inner),
        run_id: Some(run_id),
    };
    inner.notify(committed_through);
    // The run cannot start until scheduling below, so subscribing after the
    // atomic commit and before that signal cannot miss its completion.
    let Ok(mut wakeup) = inner.subscribe(parent.workspace_id, committed_through.sequence) else {
        return spawn_error("the sub-agent could not be awaited");
    };
    drop(lifecycle);
    let _ = inner.schedule.try_send(());
    let mut cancelled = false;
    let (outcome, spend) = loop {
        if !cancelled && *cancellation.borrow() {
            inner.cancel(run_id);
            match inner.store.cancel_child_run(run_id).await {
                Ok(applied) => {
                    inner.notify(applied.receipt.committed_through);
                    for run in applied.cascade_cancels {
                        inner.cancel(run);
                    }
                    let _ = inner.schedule.try_send(());
                }
                Err(_) => {
                    inner.failed.send_replace(true);
                    guard.disarm();
                    return spawn_error_with_spend(
                        "child cancellation could not be persisted",
                        SpawnAgentSpend::UNKNOWN,
                    );
                }
            }
            cancelled = true;
        }
        match inner.store.run_outcome(run_id).await {
            Ok(Some(settled)) => break settled,
            Ok(None) => {}
            Err(error) => {
                // Outcome admission waits for capacity. A remaining error
                // means ownership can no longer be established durably;
                // the parent must not resume ordinary tool execution.
                inner.failed.send_replace(true);
                inner.cancel(run_id);
                guard.disarm();
                return spawn_error_with_spend(
                    format!("the sub-agent outcome could not be read: {error}"),
                    SpawnAgentSpend::UNKNOWN,
                );
            }
        }
        tokio::select! {
            biased;
            () = child_cancelled(&mut cancellation), if !cancelled => {},
            changed = wakeup.changed() => if changed.is_err() {
                inner.failed.send_replace(true);
                inner.cancel(run_id);
                guard.disarm();
                return spawn_error_with_spend("child completion became unavailable", SpawnAgentSpend::UNKNOWN);
            },
        }
    };
    guard.disarm();
    // Every settled child charges its real spend to the parent, whatever
    // its outcome; the parent's budgets must see failed work too. Every
    // settled child also names its session so callers can point at it.
    let mut settled = match outcome {
        RunOutcome::Completed => match inner.store.run_final_text(run_id).await {
            Ok(text) if text.trim().is_empty() => {
                spawn_error_with_spend("the sub-agent completed without producing any text", spend)
            }
            Ok(text) => SpawnAgentOutcome {
                content: text,
                is_error: false,
                spend,
                session_id: None,
            },
            Err(error) => spawn_error_with_spend(
                format!("the sub-agent answer could not be read: {error}"),
                spend,
            ),
        },
        RunOutcome::Cancelled => spawn_error_with_spend("the sub-agent run was cancelled", spend),
        RunOutcome::Interrupted => {
            spawn_error_with_spend("the sub-agent run was interrupted", spend)
        }
        RunOutcome::BudgetExhausted { exhaustion } => spawn_error_with_spend(
            format!(
                "the sub-agent run exhausted its budget: {}",
                exhaustion.message
            ),
            spend,
        ),
        RunOutcome::Failed { failure } => spawn_error_with_spend(
            format!("the sub-agent run failed: {}", failure.message),
            spend,
        ),
    };
    settled.session_id = Some(child_session_id);
    settled
}

/// Cancels a still-running child run when the spawn future awaiting it is
/// dropped before the child finished.
struct CancelChildOnDrop {
    inner: Arc<SessionRuntimeInner>,
    run_id: Option<RunId>,
}

impl CancelChildOnDrop {
    fn disarm(&mut self) {
        self.run_id = None;
    }
}

impl Drop for CancelChildOnDrop {
    fn drop(&mut self) {
        if let Some(run_id) = self.run_id.take() {
            // The owned task only abandons an admitted child on a panic.
            // Stop the runtime rather than permit further parent mutation.
            self.inner.failed.send_replace(true);
            self.inner.cancel(run_id);
        }
    }
}

/// `search_history` over the run's own session. Read-only and bounded; an
/// overloaded store retries briefly rather than surfacing a transient error
/// to the model as history that does not exist.
pub(super) struct SessionHistorySearcher {
    inner: Arc<SessionRuntimeInner>,
    session_id: SessionId,
    run_id: RunId,
}

impl SessionHistorySearcher {
    pub(super) fn new(
        inner: Arc<SessionRuntimeInner>,
        session_id: SessionId,
        run_id: RunId,
    ) -> Self {
        Self {
            inner,
            session_id,
            run_id,
        }
    }
}

impl HistorySearcher for SessionHistorySearcher {
    fn search(&self, query: String, limit: usize) -> HistorySearchFuture {
        let inner = Arc::clone(&self.inner);
        let session_id = self.session_id;
        let run_id = self.run_id;
        Box::pin(async move {
            for _ in 0..HISTORY_SEARCH_RETRIES {
                match inner
                    .store
                    .search_history(session_id, run_id, query.clone(), limit)
                    .await
                {
                    Ok(matches) => return Ok(matches),
                    Err(SessionRuntimeError::Overloaded) => {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    Err(error) => return Err(format!("history search failed: {error}")),
                }
            }
            Err("history search failed: the session store stayed overloaded".to_owned())
        })
    }
}

const HISTORY_SEARCH_RETRIES: usize = 64;

/// Audits a root run's candidate answer by spawning a read-only child marked
/// `purpose: audit` at the configured roster role. The child inherits every
/// bound of an ordinary child (remaining budget, depth, accounting,
/// cancellation) and its final text is parsed as the verdict. Any failure is
/// `Unavailable`: the audit never fails the audited run.
pub(super) struct SessionAuditHook {
    inner: Arc<SessionRuntimeInner>,
    parent: ClaimedRun,
    delegation: qq_protocol::DelegationRoster,
    /// Audits are one at a time per run and never count against the
    /// parent's ordinary child slots.
    slots: Arc<Semaphore>,
    tasks: Arc<ChildTasks>,
    pending: Arc<Mutex<Option<ToolCallId>>>,
}

impl SessionAuditHook {
    pub(super) fn new(
        inner: Arc<SessionRuntimeInner>,
        parent: ClaimedRun,
        delegation: qq_protocol::DelegationRoster,
        tasks: Arc<ChildTasks>,
    ) -> Self {
        Self {
            inner,
            parent,
            delegation,
            slots: Arc::new(Semaphore::new(1)),
            tasks,
            pending: Arc::new(Mutex::new(None)),
        }
    }
}

/// The fixed brief an audit child receives. The auditor verifies claims with
/// its own read-only tools rather than trusting the run's account.
const AUDIT_BRIEF_HEADER: &str = "You are auditing another agent's final answer to a user \
request in this workspace. You have read-only tools. Verify the answer's factual claims \
against the actual workspace state: open the files it says it changed, run the read-only \
checks it says it ran, and confirm the request was addressed. Do not redo the task. Reply \
with exactly one JSON object on one line and nothing else: \
{\"verdict\":\"pass\"} when the claims hold, or \
{\"verdict\":\"revise\",\"findings\":[\"...\"]} listing each concrete, verifiable problem \
(at most 8, one sentence each). Escalate nothing; if you cannot verify, say so as a finding.";

impl crate::runtime::AuditHook for SessionAuditHook {
    fn acknowledge(&self) {
        if let Some(call_id) = self.pending.lock().expect("audit owner poisoned").take() {
            self.tasks.acknowledge(call_id);
        }
    }

    fn drain(&self) -> ChildDrainFuture {
        let tasks = Arc::clone(&self.tasks);
        Box::pin(async move { tasks.drain().await })
    }

    fn audit(
        &self,
        request: crate::runtime::AuditRequest,
        budget: crate::runtime::ChildBudget,
    ) -> crate::runtime::AuditFuture {
        let inner = Arc::clone(&self.inner);
        let parent = self.parent.clone();
        let slots = Arc::clone(&self.slots);
        let tasks = Arc::clone(&self.tasks);
        let pending = Arc::clone(&self.pending);
        let model = self
            .delegation
            .route_for_role(request.role)
            .map(str::to_owned);
        Box::pin(async move {
            let mut brief = String::with_capacity(4 * 1024);
            brief.push_str(AUDIT_BRIEF_HEADER);
            brief.push_str("\n\nUser request:\n");
            brief.push_str(&request.prompt);
            brief.push_str("\n\nThe agent's final answer");
            if request.revision > 0 {
                brief.push_str(&format!(" (revision {})", request.revision));
            }
            brief.push_str(":\n");
            brief.push_str(&request.answer);
            if !request.actions.is_empty() {
                brief.push_str("\n\nTool calls the agent made, in order:\n");
                for action in &request.actions {
                    brief.push_str("- ");
                    brief.push_str(&action.tool);
                    if let Some(target) = &action.target {
                        brief.push(' ');
                        brief.push_str(target);
                    }
                    if action.is_error {
                        brief.push_str(" (error)");
                    }
                    brief.push('\n');
                }
            }
            // The audit child is admitted with the parent's remaining budget
            // like any child; the parent's meter already accounts for this
            // spend when the verdict returns.
            let call_id = match ToolCallId::generate() {
                Ok(call_id) => call_id,
                Err(_) => {
                    inner.failed.send_replace(true);
                    return crate::runtime::AuditVerdict::unavailable();
                }
            };
            *pending.lock().expect("audit owner poisoned") = Some(call_id);
            let outcome = spawn_child_run(
                inner,
                parent,
                SpawnBudget {
                    slots,
                    write_slot: Arc::new(Semaphore::new(1)),
                    write_children: false,
                    spawned: Arc::new(AtomicUsize::new(0)),
                    max_children: 1,
                    tasks,
                },
                SpawnRequest {
                    call_id,
                    task: brief,
                    model,
                    authority: ChildAuthority::Read,
                    budget,
                    purpose: SessionPurpose::Audit,
                },
            )
            .await;
            let mut verdict = crate::runtime::AuditVerdict {
                outcome: AuditOutcome::Unavailable,
                findings: Vec::new(),
                usage: outcome.spend.usage,
                cost_usd_nanos: outcome.spend.cost_usd_nanos,
                audit_session: outcome.session_id,
            };
            if outcome.is_error {
                return verdict;
            }
            #[derive(serde::Deserialize)]
            struct Reply {
                verdict: String,
                #[serde(default)]
                findings: Vec<String>,
            }
            let Ok(reply) = serde_json::from_str::<Reply>(outcome.content.trim()) else {
                return verdict;
            };
            match reply.verdict.as_str() {
                "pass" => verdict.outcome = AuditOutcome::Pass,
                "revise" => {
                    verdict.outcome = AuditOutcome::Revised;
                    verdict.findings = reply
                        .findings
                        .into_iter()
                        .map(|finding| {
                            truncate_utf8(finding, crate::runtime::MAX_AUDIT_FINDING_BYTES)
                        })
                        .filter(|finding| !finding.trim().is_empty())
                        .take(crate::runtime::MAX_AUDIT_FINDINGS)
                        .collect();
                    if verdict.findings.is_empty() {
                        // A revise with nothing to fix is not actionable.
                        verdict.outcome = AuditOutcome::Pass;
                    }
                }
                _ => {}
            }
            verdict
        })
    }
}
