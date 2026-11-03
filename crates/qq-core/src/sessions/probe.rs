use qq_protocol::{RunId, SessionId, ToolCallId};

use super::SessionRuntimeError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Checkpoint {
    CancellationRead(RunId),
    OutcomeRead(RunId),
    OutcomeReadPolled(RunId),
    ChildCreated(SessionId),
    ReservedStart(RunId),
    ChildDelivery(SessionId),
    ChildReceipt(SessionId),
    #[cfg_attr(not(test), allow(dead_code))]
    ParentWait(SessionId),
    ExecutionStop(RunId),
    BufferedToolOutput(ToolCallId),
    DropSettlement(RunId),
    TurnCapture(RunId, u32),
    WorkspaceReserved(SessionId),
    SessionResourcesCleaned(SessionId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StoreFault {
    CancellationRead(RunId),
    ReservedSettlement(RunId),
    ReservedReload(RunId),
    ChildCancellation(RunId),
    ForkCreated(SessionId),
}

pub(super) static PROBE: RuntimeProbe = RuntimeProbe::new();
pub(super) static FAULTS: StoreFaults = StoreFaults::new();

#[cfg(test)]
pub(super) use enabled::{RuntimeProbe, StoreFaults};

#[cfg(not(test))]
pub(super) use disabled::{RuntimeProbe, StoreFaults};

#[cfg(not(test))]
mod disabled {
    use super::{Checkpoint, RunId, SessionRuntimeError, StoreFault};

    pub(in crate::sessions) struct RuntimeProbe;

    impl RuntimeProbe {
        pub(super) const fn new() -> Self {
            Self
        }

        #[inline]
        pub(in crate::sessions) async fn reached(&self, _at: Checkpoint) -> bool {
            false
        }

        #[inline]
        pub(in crate::sessions) async fn reached_with(&self, _at: Checkpoint, _run: RunId) {}

        #[inline]
        pub(in crate::sessions) fn passed(&self, _at: Checkpoint) {}

        #[inline]
        pub(in crate::sessions) async fn polled<F: Future>(
            &self,
            _at: Checkpoint,
            future: F,
        ) -> F::Output {
            future.await
        }
    }

    pub(in crate::sessions) struct StoreFaults;

    impl StoreFaults {
        pub(super) const fn new() -> Self {
            Self
        }

        #[inline]
        pub(in crate::sessions) fn take(&self, _fault: StoreFault) -> Option<SessionRuntimeError> {
            None
        }
    }
}

#[cfg(test)]
mod enabled {
    use std::sync::Mutex;

    use tokio::sync::oneshot;

    use super::{Checkpoint, RunId, SessionRuntimeError, StoreFault};

    type Entered = Box<dyn FnOnce(Option<RunId>) -> bool + Send>;

    struct Hold {
        at: Checkpoint,
        skip: usize,
        entered: Entered,
        release: Option<oneshot::Receiver<()>>,
    }

    pub(in crate::sessions) struct RuntimeProbe {
        holds: Mutex<Vec<Hold>>,
    }

    impl RuntimeProbe {
        pub(super) const fn new() -> Self {
            Self {
                holds: Mutex::new(Vec::new()),
            }
        }

        pub(in crate::sessions) fn hold(
            &self,
            at: Checkpoint,
        ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
            let (entered, entered_rx) = oneshot::channel();
            let release = self.register(at, 0, entered_signal(entered), true);
            (entered_rx, release.expect("a hold has a release"))
        }

        pub(in crate::sessions) fn hold_reporting_run(
            &self,
            at: Checkpoint,
            skip: usize,
        ) -> (oneshot::Receiver<RunId>, oneshot::Sender<()>) {
            let (entered, entered_rx) = oneshot::channel();
            let release = self.register(
                at,
                skip,
                Box::new(move |run: Option<RunId>| {
                    entered
                        .send(run.expect("checkpoint reports its run"))
                        .is_ok()
                }),
                true,
            );
            (entered_rx, release.expect("a hold has a release"))
        }

        pub(in crate::sessions) fn observe(&self, at: Checkpoint) -> oneshot::Receiver<()> {
            let (entered, entered_rx) = oneshot::channel();
            self.register(at, 0, entered_signal(entered), false);
            entered_rx
        }

        pub(in crate::sessions) async fn reached(&self, at: Checkpoint) -> bool {
            self.arrive(at, None).await
        }

        pub(in crate::sessions) async fn reached_with(&self, at: Checkpoint, run: RunId) {
            self.arrive(at, Some(run)).await;
        }

        pub(in crate::sessions) fn passed(&self, at: Checkpoint) {
            if let Some(hold) = self.take(at) {
                (hold.entered)(None);
            }
        }

        pub(in crate::sessions) async fn polled<F: Future>(
            &self,
            at: Checkpoint,
            future: F,
        ) -> F::Output {
            let mut future = std::pin::pin!(future);
            let mut first = true;
            std::future::poll_fn(|cx| {
                let result = future.as_mut().poll(cx);
                if std::mem::take(&mut first) {
                    self.passed(at);
                }
                result
            })
            .await
        }

        fn register(
            &self,
            at: Checkpoint,
            skip: usize,
            entered: Entered,
            holds: bool,
        ) -> Option<oneshot::Sender<()>> {
            let (release, release_rx) = if holds {
                let (release, release_rx) = oneshot::channel();
                (Some(release), Some(release_rx))
            } else {
                (None, None)
            };
            self.holds.lock().unwrap().push(Hold {
                at,
                skip,
                entered,
                release: release_rx,
            });
            release
        }

        fn take(&self, at: Checkpoint) -> Option<Hold> {
            let mut holds = self.holds.lock().unwrap();
            let index = holds.iter().position(|hold| hold.at == at)?;
            if holds[index].skip > 0 {
                holds[index].skip -= 1;
                return None;
            }
            Some(holds.remove(index))
        }

        async fn arrive(&self, at: Checkpoint, run: Option<RunId>) -> bool {
            let Some(hold) = self.take(at) else {
                return false;
            };
            if (hold.entered)(run)
                && let Some(release) = hold.release
            {
                let _ = release.await;
            }
            true
        }
    }

    fn entered_signal(entered: oneshot::Sender<()>) -> Entered {
        Box::new(move |_| {
            let _ = entered.send(());
            true
        })
    }

    pub(in crate::sessions) struct StoreFaults {
        failures: Mutex<Vec<(StoreFault, SessionRuntimeError)>>,
    }

    impl StoreFaults {
        pub(super) const fn new() -> Self {
            Self {
                failures: Mutex::new(Vec::new()),
            }
        }

        pub(in crate::sessions) fn inject(
            &self,
            fault: StoreFault,
            failures: impl IntoIterator<Item = SessionRuntimeError>,
        ) {
            self.failures
                .lock()
                .unwrap()
                .extend(failures.into_iter().map(|failure| (fault, failure)));
        }

        pub(in crate::sessions) fn take(&self, fault: StoreFault) -> Option<SessionRuntimeError> {
            let mut failures = self.failures.lock().unwrap();
            let index = failures.iter().position(|(target, _)| *target == fault)?;
            Some(failures.remove(index).1)
        }
    }
}
