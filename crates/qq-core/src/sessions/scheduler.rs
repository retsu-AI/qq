use super::*;
use super::{
    execution::{RunResources, execute_run, internal_failure, persistence_failure},
    runtime::SessionRuntimeInner,
};

struct SchedulerStopGuard(watch::Sender<bool>);

impl Drop for SchedulerStopGuard {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

pub(super) async fn schedule_runs(
    inner: std::sync::Weak<SessionRuntimeInner>,
    mut receiver: mpsc::Receiver<()>,
    mut shutdown: watch::Receiver<bool>,
    stopped: watch::Sender<bool>,
) {
    let _stopped = SchedulerStopGuard(stopped);
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
        // Each depth is claimed from its own queue against its own permit
        // pool; see `child_permits` for why sharing a pool between a depth
        // and its parents would deadlock parents awaiting their children.
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
                let task_inner = Arc::clone(&inner);
                let panic_claimed = claimed.panic_settlement_claim();
                tokio::spawn(async move {
                    let resources = RunResources::default();
                    #[cfg(test)]
                    let resources = resources.for_test_run(claimed.identity.run_id);
                    let execution = AssertUnwindSafe(supervise_reserved_run(
                        Arc::clone(&task_inner),
                        claimed,
                        resources.clone(),
                    ))
                    .catch_unwind()
                    .await;
                    if execution.is_err() {
                        if resources.drain().await.is_err() {
                            task_inner.failed.send_replace(true);
                        } else {
                            settle_panicked_execution(&task_inner, &panic_claimed).await;
                        }
                    }
                    drop(permit);
                    task_inner
                        .settlements
                        .send_modify(|generation| *generation = generation.wrapping_add(1));
                    if !*task_inner.shutdown.borrow() {
                        let _ = task_inner.schedule.try_send(());
                    }
                });
            }
        }
    }
}

async fn supervise_reserved_run(
    inner: Arc<SessionRuntimeInner>,
    claimed: ClaimedRun,
    resources: RunResources,
) {
    let (cancel, cancel_receiver) = watch::channel(false);
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
    // A cancel recorded before the claim rides the claim. One recorded
    // between the claim and the registration above would have found no
    // watch to signal, so the flag is re-read once now that the watch
    // exists; every later cancel reaches it through `cancel`.
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
