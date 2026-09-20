//! Active-run steering.
//!
//! A steering message is user input added to a run that is already executing.
//! The session layer records it durably, then hands it to the run loop through
//! a bounded channel; the loop injects it as a user message at the next safe
//! boundary (after a turn's tool results are appended, or instead of
//! completing when the model returned no tool calls). An interrupting steer
//! also bumps a generation the loop observes inside the provider stream and
//! tool execution, so the boundary arrives now: the in-flight stream or tool
//! futures are dropped, partial text stands, and unfinished calls settle as
//! interrupted before the injected message is sent.

use qq_protocol::{InputPart, MessageId};
use tokio::sync::{mpsc, watch};

/// Most steering messages that may wait for a boundary per run. Admission
/// refuses the next one rather than growing the queue.
pub const MAX_PENDING_STEERING: u16 = 4;

/// One durably recorded steering message awaiting injection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SteeringMessage {
    pub(crate) message_id: MessageId,
    /// The structured input as submitted. File parts are read when the
    /// message is applied at a turn boundary, so the model sees the bytes as
    /// they are then and the store keeps them like a prompt's attachments.
    pub(crate) input: Vec<InputPart>,
}

#[cfg(test)]
impl SteeringMessage {
    pub(crate) fn text(message_id: MessageId, text: impl Into<String>) -> Self {
        Self {
            message_id,
            input: vec![InputPart::Text { text: text.into() }],
        }
    }
}

/// The run loop's end of a steering channel.
pub(crate) struct SteeringReceiver {
    pub(crate) messages: mpsc::Receiver<SteeringMessage>,
    /// Bumped once per interrupting steer. The loop compares against the last
    /// generation it handled so a bump that lands between turns is not lost.
    pub(crate) interrupts: watch::Receiver<u64>,
}

/// The session layer's end.
#[derive(Clone)]
pub(crate) struct SteeringSender {
    pub(crate) messages: mpsc::Sender<SteeringMessage>,
    pub(crate) interrupts: watch::Sender<u64>,
}

impl SteeringSender {
    /// Requests that the in-flight provider stream or tool execution stop at
    /// once so queued steering is applied now.
    pub(crate) fn interrupt(&self) {
        self.interrupts.send_modify(|generation| *generation += 1);
    }
}

pub(crate) fn steering_channel() -> (SteeringSender, SteeringReceiver) {
    let (messages_tx, messages_rx) = mpsc::channel(usize::from(MAX_PENDING_STEERING));
    let (interrupts_tx, interrupts_rx) = watch::channel(0);
    (
        SteeringSender {
            messages: messages_tx,
            interrupts: interrupts_tx,
        },
        SteeringReceiver {
            messages: messages_rx,
            interrupts: interrupts_rx,
        },
    )
}
