//! Side effects the application asks the event loop to perform. `App` never
//! touches the network, the terminal, or the clock; it returns these and the
//! loop applies them in order.

use crate::ClientRequest;
use qq_client::state::Attention;

/// One effect produced by an update.
#[derive(Debug, PartialEq)]
pub(crate) enum Effect {
    /// Something visible changed. `Immediate` is reserved for user input so
    /// typing echoes without waiting for the frame tick; everything else
    /// coalesces onto the next tick.
    Redraw(Redraw),
    /// Send a request to the server. Failures come back as a `ClientUpdate`.
    Send(ClientRequest),
    /// Suspend the terminal and open the external editor with this draft.
    Editor(String),
    /// Ring the terminal for an event that happened while it was unfocused.
    Attention(Attention),
    /// Turn terminal mouse reporting on or off.
    MouseCapture(bool),
    Quit,
}

/// When the next frame is drawn relative to pending state changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Redraw {
    /// Streamed or background changes; coalesce until the frame tick.
    Scheduled,
    /// User input; draw before waiting on anything else.
    Immediate,
}

/// The effects of one update, in application order. Most updates produce
/// none or one, so this stays a thin wrapper that only allocates when it
/// holds something.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Effects(Vec<Effect>);

impl Effects {
    #[must_use]
    pub(crate) const fn none() -> Self {
        Self(Vec::new())
    }

    #[must_use]
    pub(crate) fn redraw(redraw: Redraw) -> Self {
        Self(vec![Effect::Redraw(redraw)])
    }

    /// `Redraw::Scheduled` when `changed`, otherwise nothing.
    #[must_use]
    pub(crate) fn changed(changed: bool) -> Self {
        if changed {
            Self::redraw(Redraw::Scheduled)
        } else {
            Self::none()
        }
    }

    #[must_use]
    pub(crate) fn send(request: ClientRequest) -> Self {
        Self(vec![
            Effect::Redraw(Redraw::Scheduled),
            Effect::Send(request),
        ])
    }

    /// `Redraw::Immediate` when `changed`, otherwise nothing; for input paths.
    #[must_use]
    pub(crate) fn changed_now(changed: bool) -> Self {
        if changed {
            Self::redraw(Redraw::Immediate)
        } else {
            Self::none()
        }
    }

    /// A request produced by user input: redraw immediately and send.
    #[must_use]
    pub(crate) fn send_now(request: ClientRequest) -> Self {
        Self(vec![
            Effect::Redraw(Redraw::Immediate),
            Effect::Send(request),
        ])
    }

    pub(crate) fn push(&mut self, effect: Effect) {
        self.0.push(effect);
    }

    pub(crate) fn extend(&mut self, other: Self) {
        self.0.extend(other.0);
    }

    #[cfg(any(test, feature = "bench-support"))]
    #[must_use]
    pub(crate) fn redraws(&self) -> bool {
        self.0
            .iter()
            .any(|effect| matches!(effect, Effect::Redraw(_)))
    }

    /// Requests in this batch, for tests.
    #[cfg(test)]
    pub(crate) fn requests(&self) -> impl Iterator<Item = &ClientRequest> {
        self.0.iter().filter_map(|effect| match effect {
            Effect::Send(request) => Some(request),
            _ => None,
        })
    }

    #[cfg(test)]
    pub(crate) fn into_requests(self) -> Vec<ClientRequest> {
        self.0
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::Send(request) => Some(request),
                _ => None,
            })
            .collect()
    }

    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn iter(&self) -> std::slice::Iter<'_, Effect> {
        self.0.iter()
    }
}

impl IntoIterator for Effects {
    type Item = Effect;
    type IntoIter = std::vec::IntoIter<Effect>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl From<ClientRequest> for Effects {
    fn from(request: ClientRequest) -> Self {
        Self::send(request)
    }
}

#[cfg(test)]
impl Effects {
    /// `(redrew, requests)` for tests written against the pre-effect API.
    pub(crate) fn split(self) -> (bool, Vec<ClientRequest>) {
        let redrew = self.redraws();
        (redrew, self.into_requests())
    }
}

impl Effects {
    /// Whether this batch sends anything to the server.
    #[must_use]
    pub(crate) fn requests_anything(&self) -> bool {
        self.0
            .iter()
            .any(|effect| matches!(effect, Effect::Send(_)))
    }
}
