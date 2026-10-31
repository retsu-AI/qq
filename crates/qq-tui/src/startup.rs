use std::{collections::VecDeque, future::Future, pin::Pin, sync::Mutex};

use crate::{ClientFailure, ClientPort, ClientRequest, ClientUpdate, TuiOptions};

const PENDING_REQUEST_LIMIT: usize = 64;

type Starting<P> =
    Pin<Box<dyn Future<Output = Result<(P, TuiOptions), ClientFailure>> + Send + 'static>>;

enum State<P> {
    Starting(Starting<P>),
    Ready(P),
    Failed {
        error: ClientFailure,
        reported: bool,
    },
}

pub struct StartupPort<P> {
    state: State<P>,
    pending: Mutex<VecDeque<ClientRequest>>,
    failures: VecDeque<ClientFailure>,
    update: Option<TuiOptions>,
}

impl<P: ClientPort> StartupPort<P> {
    #[must_use]
    pub fn new(
        startup: impl Future<Output = Result<(P, TuiOptions), ClientFailure>> + Send + 'static,
    ) -> Self {
        Self {
            state: State::Starting(Box::pin(startup)),
            pending: Mutex::new(VecDeque::new()),
            failures: VecDeque::new(),
            update: None,
        }
    }

    async fn ready(&mut self) -> Result<&mut P, ClientFailure> {
        if let State::Starting(startup) = &mut self.state {
            match startup.await {
                Ok((port, options)) => {
                    self.update = Some(options);
                    let pending =
                        std::mem::take(&mut *self.pending.lock().expect("pending request lock"));
                    for request in pending {
                        if let Err(error) = port.try_send(request) {
                            self.failures.push_back(error);
                        }
                    }
                    self.state = State::Ready(port);
                }
                Err(error) => {
                    self.state = State::Failed {
                        error,
                        reported: false,
                    };
                }
            }
        }
        match &mut self.state {
            State::Ready(port) => Ok(port),
            State::Failed { error, .. } => Err(error.clone()),
            State::Starting(_) => unreachable!("startup future was just driven"),
        }
    }
}

impl<P: ClientPort> ClientPort for StartupPort<P> {
    fn try_send(&self, request: ClientRequest) -> Result<(), ClientFailure> {
        match &self.state {
            State::Ready(port) => port.try_send(request),
            State::Failed { error, .. } => Err(error.clone()),
            State::Starting(_) => {
                let mut pending = self.pending.lock().expect("pending request lock");
                if pending.len() >= PENDING_REQUEST_LIMIT {
                    return Err(ClientFailure::new(
                        "still starting; too many requests are waiting",
                    ));
                }
                pending.push_back(request);
                Ok(())
            }
        }
    }

    async fn recv(&mut self) -> Option<ClientUpdate> {
        if let Err(error) = self.ready().await {
            if let State::Failed { reported, .. } = &mut self.state
                && !*reported
            {
                *reported = true;
                return Some(ClientUpdate::SnapshotFailed(error));
            }
            return None;
        }
        if let Some(options) = self.update.take() {
            return Some(ClientUpdate::Startup(qq_client::StartupUpdate::new(
                options,
            )));
        }
        if let Some(error) = self.failures.pop_front() {
            return Some(ClientUpdate::SnapshotFailed(error));
        }
        match &mut self.state {
            State::Ready(port) => port.recv().await,
            State::Starting(_) | State::Failed { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    use qq_protocol::ModelSelection;
    use tokio::sync::mpsc;
    use tokio::sync::oneshot;

    use super::*;

    struct FakePort {
        sent: Arc<Mutex<Vec<ClientRequest>>>,
        updates: mpsc::Receiver<ClientUpdate>,
        fail_next: Arc<AtomicBool>,
    }

    impl ClientPort for FakePort {
        fn try_send(&self, request: ClientRequest) -> Result<(), ClientFailure> {
            if self.fail_next.swap(false, Ordering::SeqCst) {
                return Err(ClientFailure::new("fake send failed"));
            }
            self.sent.lock().expect("sent lock").push(request);
            Ok(())
        }

        async fn recv(&mut self) -> Option<ClientUpdate> {
            self.updates.recv().await
        }
    }

    #[tokio::test]
    async fn publishes_options_and_flushes_held_requests_before_updates() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let (updates, update_rx) = mpsc::channel(1);
        let (ready_tx, ready_rx) = oneshot::channel();
        let port = FakePort {
            sent: Arc::clone(&sent),
            updates: update_rx,
            fail_next: Arc::new(AtomicBool::new(false)),
        };
        let mut startup = StartupPort::new(async move {
            let configured = ready_rx.await.expect("startup released");
            Ok((port, configured))
        });
        let request = ClientRequest::Capabilities;
        startup.try_send(request.clone()).unwrap();
        let receive = tokio::spawn(async move {
            let startup_update = startup.recv().await;
            let client_update = startup.recv().await;
            (startup_update, client_update)
        });
        let configured = TuiOptions {
            model: ModelSelection {
                model: Some("openai/configured".to_owned()),
                ..ModelSelection::default()
            },
            ..TuiOptions::default()
        };
        ready_tx.send(configured.clone()).unwrap();
        updates
            .send(ClientUpdate::Connection(crate::ConnectionState::Live))
            .await
            .unwrap();
        let (startup_update, client_update) = receive.await.unwrap();
        assert_eq!(*sent.lock().expect("sent lock"), vec![request]);
        let Some(ClientUpdate::Startup(startup_update)) = startup_update else {
            panic!("expected startup options update")
        };
        assert_eq!(
            startup_update
                .downcast_ref::<TuiOptions>()
                .expect("tui startup options")
                .model,
            configured.model
        );
        assert_eq!(
            client_update,
            Some(ClientUpdate::Connection(crate::ConnectionState::Live))
        );
    }

    #[tokio::test]
    async fn a_held_request_failure_does_not_drop_later_requests_or_the_ready_port() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let fail_next = Arc::new(AtomicBool::new(true));
        let (updates, update_rx) = mpsc::channel(1);
        let port = FakePort {
            sent: Arc::clone(&sent),
            updates: update_rx,
            fail_next,
        };
        let mut startup = StartupPort::new(async move { Ok((port, TuiOptions::default())) });
        startup.try_send(ClientRequest::Capabilities).unwrap();
        let later = ClientRequest::Models(ModelSelection::default());
        startup.try_send(later.clone()).unwrap();

        assert!(matches!(
            startup.recv().await,
            Some(ClientUpdate::Startup(_))
        ));
        assert_eq!(
            startup.recv().await,
            Some(ClientUpdate::SnapshotFailed(ClientFailure::new(
                "fake send failed"
            )))
        );
        assert_eq!(*sent.lock().expect("sent lock"), vec![later]);

        updates
            .send(ClientUpdate::Connection(crate::ConnectionState::Live))
            .await
            .unwrap();
        assert_eq!(
            startup.recv().await,
            Some(ClientUpdate::Connection(crate::ConnectionState::Live))
        );
    }
    #[tokio::test]
    async fn startup_failure_is_reported_once() {
        let mut startup =
            StartupPort::<FakePort>::new(async { Err(ClientFailure::new("configuration failed")) });
        assert_eq!(
            startup.recv().await,
            Some(ClientUpdate::SnapshotFailed(ClientFailure::new(
                "configuration failed"
            )))
        );
    }
}
