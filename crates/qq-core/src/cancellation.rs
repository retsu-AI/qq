//! The run's cancellation token. One per run, cloned into every tool call,
//! host call, and blocking helper the run starts. Cancelling sets a flag and
//! wakes every waiter, so a call awaiting [`RunCancellation::cancelled`]
//! returns at once instead of noticing on its next poll tick; blocking work
//! that cannot await checks [`RunCancellation::is_cancelled`] between steps.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio::sync::Notify;

#[derive(Clone, Default)]
pub struct RunCancellation(Arc<Inner>);

#[derive(Default)]
struct Inner {
    cancelled: AtomicBool,
    wake: Notify,
}

impl std::fmt::Debug for RunCancellation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RunCancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl RunCancellation {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A token that is already cancelled, for tests of the cancelled path.
    #[must_use]
    pub fn already_cancelled() -> Self {
        let token = Self::new();
        token.cancel();
        token
    }

    /// Cancels the run. Idempotent; every current and future waiter wakes.
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        self.0.wake.notify_waiters();
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    /// Whether two handles are the same token (clones of one run's token).
    #[cfg(test)]
    pub(crate) fn same_token(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// A handle that does not keep the token alive; `upgrade` fails once the
    /// run has dropped every clone. For test hooks that must not extend a
    /// run's lifetime.
    #[cfg(test)]
    pub(crate) fn downgrade(&self) -> WeakRunCancellation {
        WeakRunCancellation(Arc::downgrade(&self.0))
    }

    /// Resolves once the run is cancelled. Registers with the notifier before
    /// reading the flag, so a `cancel` between the check and the await is
    /// not lost; a `cancel` before the first call resolves immediately.
    pub async fn cancelled(&self) {
        loop {
            let wake = self.0.wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            wake.await;
        }
    }
}

#[cfg(test)]
pub(crate) struct WeakRunCancellation(std::sync::Weak<Inner>);

#[cfg(test)]
impl WeakRunCancellation {
    pub(crate) fn upgrade(&self) -> Option<RunCancellation> {
        self.0.upgrade().map(RunCancellation)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn a_waiter_wakes_on_cancel_without_polling() {
        let token = RunCancellation::new();
        assert!(!token.is_cancelled());
        let waiter = tokio::spawn({
            let token = token.clone();
            async move { token.cancelled().await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        token.cancel();
        tokio::time::timeout(Duration::from_millis(100), waiter)
            .await
            .expect("the waiter wakes promptly")
            .unwrap();
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn cancel_before_the_wait_resolves_immediately_and_repeats() {
        let token = RunCancellation::already_cancelled();
        tokio::time::timeout(Duration::from_millis(10), token.cancelled())
            .await
            .unwrap();
        // A second wait on a cancelled token also resolves: the flag, not a
        // one-shot permit, is the truth.
        tokio::time::timeout(Duration::from_millis(10), token.cancelled())
            .await
            .unwrap();
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn a_cancel_between_check_and_await_is_not_lost() {
        // Deterministic: cancel from a task that runs only once the waiter
        // has registered; `enable()` before the flag read is what makes
        // this safe, and a lost wake here would hang the test.
        let token = RunCancellation::new();
        let cancel = tokio::spawn({
            let token = token.clone();
            async move {
                tokio::task::yield_now().await;
                token.cancel();
            }
        });
        tokio::time::timeout(Duration::from_secs(1), token.cancelled())
            .await
            .unwrap();
        cancel.await.unwrap();
    }
}
