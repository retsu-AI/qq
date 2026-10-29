use qq_protocol::{InputPart, MessageId};
use tokio::sync::{mpsc, watch};

pub const MAX_PENDING_STEERING: u16 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SteeringMessage {
    pub(crate) message_id: MessageId,
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

pub(crate) struct SteeringReceiver {
    pub(crate) messages: mpsc::Receiver<SteeringMessage>,
    pub(crate) interrupts: watch::Receiver<u64>,
    pub(crate) peeked: Option<SteeringMessage>,
}

#[derive(Clone)]
pub(crate) struct SteeringSender {
    pub(crate) messages: mpsc::Sender<SteeringMessage>,
    pub(crate) interrupts: watch::Sender<u64>,
}

impl SteeringSender {
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
            peeked: None,
        },
    )
}
