use std::{collections::VecDeque, future::Future, pin::Pin, sync::Mutex};

use crate::{ClientFailure, ClientPort, ClientRequest, ClientUpdate};

const PENDING_REQUEST_LIMIT: usize = 64;

type Connecting<P> = Pin<Box<dyn Future<Output = Result<P, ClientFailure>> + Send>>;

enum State<P> {
    Connecting(Connecting<P>),
    Ready(P),
    Failed {
        error: ClientFailure,
        reported: bool,
    },
}

pub struct LazyPort<P> {
    state: State<P>,
    pending: Mutex<VecDeque<ClientRequest>>,
}

impl<P: ClientPort> LazyPort<P> {
    pub fn new(connect: impl Future<Output = Result<P, ClientFailure>> + Send + 'static) -> Self {
        Self {
            state: State::Connecting(Box::pin(connect)),
            pending: Mutex::new(VecDeque::new()),
        }
    }

    async fn ready(&mut self) -> Result<&mut P, ClientFailure> {
        if let State::Connecting(connect) = &mut self.state {
            match connect.await {
                Ok(port) => {
                    let pending =
                        std::mem::take(&mut *self.pending.lock().expect("pending request lock"));
                    for request in pending {
                        if let Err(error) = port.try_send(request) {
                            self.state = State::Failed {
                                error: error.clone(),
                                reported: false,
                            };
                            return Err(error);
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
            State::Connecting(_) => unreachable!("connect future was just driven"),
        }
    }
}

impl<P: ClientPort> ClientPort for LazyPort<P> {
    fn try_send(&self, request: ClientRequest) -> Result<(), ClientFailure> {
        match &self.state {
            State::Ready(port) => port.try_send(request),
            State::Failed { error, .. } => Err(error.clone()),
            State::Connecting(_) => {
                let mut pending = self.pending.lock().expect("pending request lock");
                if pending.len() >= PENDING_REQUEST_LIMIT {
                    return Err(ClientFailure::new(
                        "still connecting; too many requests are waiting",
                    ));
                }
                pending.push_back(request);
                Ok(())
            }
        }
    }

    async fn recv(&mut self) -> Option<ClientUpdate> {
        match self.ready().await {
            Ok(port) => port.recv().await,
            Err(error) => {
                if let State::Failed { reported, .. } = &mut self.state
                    && !*reported
                {
                    *reported = true;
                    return Some(ClientUpdate::SnapshotFailed(error));
                }
                None
            }
        }
    }
}
