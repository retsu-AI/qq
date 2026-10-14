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
    use std::{future::Future, task::Poll};

    use super::*;

    #[tokio::test]
    async fn cancellation_wakes_every_registered_waiter_and_future_waits() {
        let token = RunCancellation::new();
        assert!(!token.is_cancelled());

        assert_eq!(format!("{token:?}"), "RunCancellation { cancelled: false }");
        let clones = (0..16).map(|_| token.clone()).collect::<Vec<_>>();
        let mut waits = clones
            .iter()
            .map(|token| Box::pin(token.cancelled()))
            .collect::<Vec<_>>();
        let wakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let waker = futures_util::task::waker(std::sync::Arc::new(CountWake(wakes.clone())));
        let mut cx = std::task::Context::from_waker(&waker);
        for wait in &mut waits {
            assert_eq!(wait.as_mut().poll(&mut cx), Poll::Pending);
        }
        token.cancel();
        assert_eq!(wakes.load(std::sync::atomic::Ordering::Relaxed), 16);
        for wait in &mut waits {
            assert_eq!(wait.as_mut().poll(&mut cx), Poll::Ready(()));
        }
        assert!(token.is_cancelled());
        assert_eq!(format!("{token:?}"), "RunCancellation { cancelled: true }");
        token.cancel();
        assert_eq!(
            Box::pin(token.cancelled()).as_mut().poll(&mut cx),
            Poll::Ready(())
        );
    }

    struct CountWake(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    impl futures_util::task::ArcWake for CountWake {
        fn wake_by_ref(arc_self: &std::sync::Arc<Self>) {
            arc_self
                .0
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[tokio::test]
    async fn cancel_before_the_wait_resolves_immediately_and_repeats() {
        let token = RunCancellation::already_cancelled();
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        for _ in 0..2 {
            assert_eq!(
                Box::pin(token.cancelled()).as_mut().poll(&mut cx),
                Poll::Ready(())
            );
        }
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn dropped_waiters_do_not_consume_the_live_waiters_wakeup() {
        let token = RunCancellation::new();
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let mut dropped = Box::pin(token.cancelled());
        let mut live = Box::pin(token.cancelled());
        assert_eq!(dropped.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(live.as_mut().poll(&mut cx), Poll::Pending);
        drop(dropped);
        token.cancel();
        assert_eq!(live.as_mut().poll(&mut cx), Poll::Ready(()));
    }
}
