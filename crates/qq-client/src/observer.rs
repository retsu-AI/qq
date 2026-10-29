use std::{future::Future, pin::Pin, time::Duration};

use futures_util::StreamExt;
use qq_protocol::{EventCursor, SessionEventEnvelope, WorkspaceId};

use crate::{ClientError, SessionClient};

pub const OBSERVER_MIN_BACKOFF: Duration = Duration::from_millis(50);
pub const OBSERVER_MAX_BACKOFF: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObserverStep {
    Continue,
    Stop,
}

pub type SinkFuture<'a> = Pin<Box<dyn Future<Output = ObserverStep> + Send + 'a>>;

pub trait EventSink: Send {
    fn deliver(&mut self, event: &SessionEventEnvelope) -> SinkFuture<'_>;

    fn disconnected(&mut self, _error: &ClientError, _resume_from: &EventCursor) -> ObserverStep {
        ObserverStep::Continue
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObserverExit {
    Stopped,
    CursorRejected,
    EventTooLarge,
}

pub async fn run<S: EventSink>(
    client: &SessionClient,
    workspace_id: WorkspaceId,
    mut cursor: EventCursor,
    sink: &mut S,
) -> (ObserverExit, EventCursor) {
    let mut backoff = OBSERVER_MIN_BACKOFF;
    loop {
        let stream = match client.events(workspace_id, cursor).await {
            Ok(stream) => stream,
            Err(
                ClientError::InvalidCursor
                | ClientError::ServerMessage { status: 400, .. }
                | ClientError::ServerResponse { status: 400 },
            ) => return (ObserverExit::CursorRejected, cursor),
            Err(error) => {
                if sink.disconnected(&error, &cursor) == ObserverStep::Stop {
                    return (ObserverExit::Stopped, cursor);
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(OBSERVER_MAX_BACKOFF);
                continue;
            }
        };
        let mut stream = stream;
        let mut failure = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(event) => {
                    backoff = OBSERVER_MIN_BACKOFF;
                    let next = event.cursor;
                    if sink.deliver(&event).await == ObserverStep::Stop {
                        return (ObserverExit::Stopped, cursor);
                    }
                    cursor = next;
                }
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        match failure {
            None => {}
            Some(ClientError::InvalidCursor) => return (ObserverExit::CursorRejected, cursor),
            Some(ClientError::EventTooLarge) => return (ObserverExit::EventTooLarge, cursor),
            Some(error) if sink.disconnected(&error, &cursor) == ObserverStep::Stop => {
                return (ObserverExit::Stopped, cursor);
            }
            Some(_) => {}
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(OBSERVER_MAX_BACKOFF);
    }
}
