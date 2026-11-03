use super::*;

use super::{
    execution::{RunResources, execute_run, internal_failure, persistence_failure},
    runtime::SessionRuntimeInner,
};
use tracing::Instrument as _;

use std::task::{Context, Poll};
use tokio::sync::{AcquireError, OwnedSemaphorePermit};

type AdmissionFuture =
    Pin<Box<dyn Future<Output = Result<OwnedSemaphorePermit, AcquireError>> + Send>>;

enum AdmissionState {
    Active { _permit: OwnedSemaphorePermit },
    Waiting,
    Resuming(AdmissionFuture),
}

pub(super) struct RunAdmission {
    pool: Arc<Semaphore>,
    state: Mutex<AdmissionState>,
    schedule: mpsc::Sender<()>,
}

impl RunAdmission {
    fn new(pool: Arc<Semaphore>, permit: OwnedSemaphorePermit, schedule: mpsc::Sender<()>) -> Self {
        Self {
            pool,
            state: Mutex::new(AdmissionState::Active { _permit: permit }),
            schedule,
        }
    }

    pub(super) fn suspend(self: &Arc<Self>) -> AdmissionHold {
        *self.state.lock().expect("run admission lock") = AdmissionState::Waiting;
        let _ = self.schedule.try_send(());
        AdmissionHold(Arc::clone(self))
    }

    pub(super) fn resume(&self) {
        let mut state = self.state.lock().expect("run admission lock");
        if matches!(*state, AdmissionState::Waiting) {
            let mut future: AdmissionFuture = Box::pin(Arc::clone(&self.pool).acquire_owned());
            let mut context = Context::from_waker(futures_util::task::noop_waker_ref());
            *state = match future.as_mut().poll(&mut context) {
                Poll::Ready(Ok(permit)) => AdmissionState::Active { _permit: permit },
                Poll::Ready(Err(_)) => unreachable!("admission pools are never closed"),
                Poll::Pending => AdmissionState::Resuming(future),
            };
        }
    }

    #[cfg(test)]
    pub(super) fn is_resuming(&self) -> bool {
        matches!(
            *self.state.lock().expect("run admission lock"),
            AdmissionState::Resuming(_)
        )
    }

    fn poll_ready(&self, context: &mut Context<'_>) -> Poll<()> {
        let mut state = self.state.lock().expect("run admission lock");
        match &mut *state {
            AdmissionState::Resuming(future) => match future.as_mut().poll(context) {
                Poll::Ready(Ok(permit)) => {
                    *state = AdmissionState::Active { _permit: permit };
                    Poll::Ready(())
                }
                Poll::Ready(Err(_)) => unreachable!("admission pools are never closed"),
                Poll::Pending => Poll::Pending,
            },
            AdmissionState::Active { .. } => Poll::Ready(()),
            AdmissionState::Waiting => Poll::Ready(()),
        }
    }

    pub(super) async fn reacquire(&self) {
        self.resume();
        std::future::poll_fn(|context| self.poll_ready(context)).await;
    }
}

pub(super) struct AdmissionHold(Arc<RunAdmission>);

impl Drop for AdmissionHold {
    fn drop(&mut self) {
        self.0.resume();
    }
}
struct SchedulerStopGuard(watch::Sender<bool>);

impl Drop for SchedulerStopGuard {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

const EXPIRY_INTERVAL: Duration = Duration::from_secs(60 * 60);

async fn expire_idle_sessions(inner: &SessionRuntimeInner, retention: Duration) {
    let now = now_ms();
    let cutoff = now.saturating_sub(u64::try_from(retention.as_millis()).unwrap_or(u64::MAX));
    let mut after = None;
    loop {
        match inner.store.expire_idle_sessions(cutoff, after, now).await {
            Ok(expired) => {
                if let Err(error) = fork::cleanup(inner, false).await {
                    tracing::warn!(%error, "could not reclaim expired session resources");
                }
                for cursor in expired.cursors {
                    inner.notify(cursor);
                }
                match expired.resume_after {
                    Some(next) => after = Some(next),
                    None => return,
                }
            }
            Err(error) => {
                tracing::warn!(%error, "could not delete sessions past retention_days");
                return;
            }
        }
    }
}

pub(super) async fn schedule_runs(
    inner: std::sync::Weak<SessionRuntimeInner>,
    mut receiver: mpsc::Receiver<()>,
    mut shutdown: watch::Receiver<bool>,
    stopped: watch::Sender<bool>,
) {
    let _stopped = SchedulerStopGuard(stopped);
    let mut next_goal_wake: Option<u64> = None;
    let mut next_expiry = Some(tokio::time::Instant::now());
    'scheduler: loop {
        let scheduled = tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break 'scheduler;
                }
                continue;
            }
            scheduled = receiver.recv() => scheduled,
            () = async {
                match next_goal_wake {
                    Some(at)=>tokio::time::sleep(Duration::from_millis(at.saturating_sub(now_ms()))).await,
                    None=>std::future::pending::<()>().await,
                }
            } => Some(()),
            () = async {
                match next_expiry {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending::<()>().await,
                }
            } => Some(()),
        };
        if scheduled.is_none() {
            break;
        }
        let Some(inner) = inner.upgrade() else {
            break;
        };
        if *shutdown.borrow() || *inner.failed.borrow() {
            break;
        }
        if let Some(retention) = inner.retention
            && next_expiry.is_some_and(|at| at <= tokio::time::Instant::now())
        {
            next_expiry = Some(tokio::time::Instant::now() + EXPIRY_INTERVAL);
            expire_idle_sessions(&inner, retention).await;
        } else if inner.retention.is_none() {
            next_expiry = None;
        }
        next_goal_wake = if inner.store.has_goals() {
            let _lifecycle = inner.lifecycle.read().await;
            if inner.draining.load(Ordering::Acquire) {
                None
            } else {
                match inner.store.drive_goals(now_ms()).await {
                    Ok(pass) => {
                        for cursor in pass.cursors {
                            inner.notify(cursor);
                        }
                        pass.next_wake
                    }
                    Err(_) => {
                        inner.failed.send_replace(true);
                        break;
                    }
                }
            }
        } else {
            None
        };
        for depth in 0..=MAX_CHILD_DEPTH {
            let pool = if depth == 0 {
                &inner.permits
            } else {
                &inner.child_permits[usize::from(depth) - 1]
            };
            loop {
                if *shutdown.borrow() || *inner.failed.borrow() {
                    break 'scheduler;
                }
                let permit = match Arc::clone(pool).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => break,
                };
                let claimed = match inner.store.reserve_next_run_at_depth(depth).await {
                    Ok(Some(claimed)) => claimed,
                    Ok(None) => break,
                    Err(_) => {
                        inner.failed.send_replace(true);
                        break 'scheduler;
                    }
                };
                let admission = Arc::new(RunAdmission::new(
                    Arc::clone(pool),
                    permit,
                    inner.schedule.clone(),
                ));
                inner
                    .admissions
                    .lock()
                    .expect("run admissions lock")
                    .insert(claimed.identity.run_id, Arc::clone(&admission));
                let task_inner = Arc::clone(&inner);
                let panic_claimed = claimed.panic_settlement_claim();

                let run_span = tracing::info_span!(target: "qq::diagnostic", "run", run_ordinal = crate::runtime::next_diagnostic_run());
                tokio::spawn(
                    async move {
                        let resources = RunResources::for_run(claimed.identity.run_id);
                        let run_id = claimed.identity.run_id;
                        let deadline = claimed
                            .limits
                            .max_duration_ms
                            .map(|ms| tokio::time::Instant::now() + Duration::from_millis(ms));
                        let (cancel, mut cancelled) = watch::channel(false);
                        let mut failed = task_inner.failed.subscribe();
                        let mut shutdown = task_inner.shutdown.subscribe();
                        let control = async move {
                            tokio::select! {
                                _ = cancelled.wait_for(|cancelled| *cancelled) => {},
                                _ = failed.wait_for(|failed| *failed) => {},
                                _ = shutdown.wait_for(|shutdown| *shutdown) => {},
                            }
                        };
                        tokio::pin!(control);
                        let mut stopped = false;
                        let supervised = supervise_reserved_run(
                            Arc::clone(&task_inner),
                            claimed,
                            resources.clone(),
                            cancel,
                        );
                        tokio::pin!(supervised);
                        let execution = AssertUnwindSafe(std::future::poll_fn(|context| {
                            if !stopped && control.as_mut().poll(context).is_ready() {
                                stopped = true;
                            }
                            let cancelled = stopped;
                            let expired =
                                deadline.is_some_and(|deadline| {
                                    tokio::time::Instant::now() >= deadline
                                }) || task_inner.goal_deadlines.lock().ok().is_some_and(
                                    |deadlines| {
                                        deadlines
                                            .get(&run_id)
                                            .is_some_and(|deadline| deadline.expired())
                                    },
                                );
                            if !cancelled
                                && !expired
                                && !*task_inner.failed.borrow()
                                && !*task_inner.shutdown.borrow()
                                && admission.poll_ready(context).is_pending()
                            {
                                probe::PROBE.passed(probe::Checkpoint::AdmissionWaiting(run_id));
                                return Poll::Pending;
                            }
                            supervised.as_mut().poll(context)
                        }))
                        .catch_unwind()
                        .await;
                        if execution.is_err() {
                            match resources.drain().await {
                                Ok(mut teardown) => {
                                    if execution::persist_finished_results(
                                        &task_inner,
                                        &panic_claimed,
                                        &mut teardown,
                                    )
                                    .await
                                    .is_err()
                                    {
                                        task_inner.failed.send_replace(true);
                                    } else {
                                        settle_panicked_execution(&task_inner, &panic_claimed)
                                            .await;
                                    }
                                }
                                Err(_) => {
                                    task_inner.failed.send_replace(true);
                                }
                            }
                        }
                        task_inner
                            .admissions
                            .lock()
                            .expect("run admissions lock")
                            .remove(&run_id);
                        drop(admission);
                        task_inner
                            .settlements
                            .send_modify(|generation| *generation = generation.wrapping_add(1));
                        if !*task_inner.shutdown.borrow() {
                            let _ = task_inner.schedule.try_send(());
                        }
                    }
                    .instrument(run_span),
                );
            }
        }
    }
}

async fn supervise_reserved_run(
    inner: Arc<SessionRuntimeInner>,
    claimed: ClaimedRun,
    resources: RunResources,
    cancel: watch::Sender<bool>,
) {
    let cancel_receiver = cancel.subscribe();
    let registered = match inner.cancellations.lock() {
        Ok(mut cancellations) => {
            cancellations.insert(claimed.identity.run_id, cancel.clone());
            true
        }
        Err(_) => false,
    };
    if !registered {
        settle_unstartable_reservation(
            &inner,
            &claimed,
            internal_failure("run cancellation registry is unavailable"),
            true,
        )
        .await;
        return;
    }
    if claimed.cancel_requested {
        cancel.send_replace(true);
    } else {
        match inner
            .store
            .cancellation_requested(claimed.identity.run_id)
            .await
        {
            Ok(true) => {
                cancel.send_replace(true);
            }
            Ok(false) => {}
            Err(error) => {
                settle_unstartable_reservation(
                    &inner,
                    &claimed,
                    persistence_failure("failed to read reserved-run cancellation state", &error),
                    false,
                )
                .await;
                return;
            }
        }
    }
    if *inner.failed.borrow() {
        settle_unstartable_reservation(
            &inner,
            &claimed,
            internal_failure("session runtime failed before run preparation"),
            true,
        )
        .await;
        return;
    }
    execute_run(inner, claimed, cancel_receiver, resources).await;
}

async fn settle_unstartable_reservation(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    outcome: RunOutcome,
    fail_runtime: bool,
) {
    if fail_runtime {
        inner.failed.send_replace(true);
    }
    match inner.store.finish_reserved_run(claimed, outcome).await {
        Ok(events) => {
            for event in events {
                inner.notify(event.cursor);
            }
        }
        Err(_) => {
            inner.failed.send_replace(true);
        }
    }
    if let Ok(mut cancellations) = inner.cancellations.lock() {
        cancellations.remove(&claimed.identity.run_id);
    }
    if let Ok(mut steering) = inner.steering.lock() {
        steering.remove(&claimed.identity.run_id);
    }
    inner.clear_run_approvals(claimed.identity.run_id);
    inner
        .settlements
        .send_modify(|generation| *generation = generation.wrapping_add(1));
}

async fn settle_panicked_execution(inner: &SessionRuntimeInner, claimed: &ClaimedRun) {
    if claimed.identity.kind == RunKind::Prompt {
        checkpoints::settle(inner, claimed, true).await;
    }
    match inner
        .store
        .settle_panicked_execution(
            claimed,
            internal_failure("agent run task panicked; committed work was preserved"),
        )
        .await
    {
        Ok(settlement) => {
            for event in settlement.events {
                inner.notify(event.cursor);
            }
            if let Ok(mut cancellations) = inner.cancellations.lock() {
                for run_id in &settlement.run_ids {
                    cancellations.remove(run_id);
                }
            }
            for run_id in settlement.run_ids {
                inner.clear_run_approvals(run_id);
            }
        }
        Err(_) => {
            inner.failed.send_replace(true);
        }
    }
}
