use crate::ClientRequest;
use qq_client::state::Attention;

#[derive(Debug, PartialEq)]
pub(crate) enum Effect {
    Redraw(Redraw),
    Send(ClientRequest),
    Editor(String),
    ResolveMentions {
        id: u64,
        submit: PendingSubmit,
    },
    CompleteMention {
        id: u64,
        query: String,
        recent: Vec<String>,
    },
    PrefetchBody {
        id: u64,
        workspace_id: qq_protocol::WorkspaceId,
        session_id: qq_protocol::SessionId,
    },
    Attention(Attention),
    MouseCapture(bool),
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingSubmit {
    pub(crate) text: String,
    pub(crate) target: SubmitTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubmitTarget {
    Prompt {
        session_id: qq_protocol::SessionId,
    },
    Steer {
        session_id: qq_protocol::SessionId,
        run_id: qq_protocol::RunId,
        interrupt: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Redraw {
    Scheduled,
    Immediate,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Effects(Vec<Effect>);

impl Effects {
    pub(crate) fn resolve_mentions(id: u64, submit: PendingSubmit) -> Self {
        Self(vec![Effect::ResolveMentions { id, submit }])
    }

    #[must_use]
    pub(crate) const fn none() -> Self {
        Self(Vec::new())
    }

    #[must_use]
    pub(crate) fn redraw(redraw: Redraw) -> Self {
        Self(vec![Effect::Redraw(redraw)])
    }

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

    #[must_use]
    pub(crate) fn changed_now(changed: bool) -> Self {
        if changed {
            Self::redraw(Redraw::Immediate)
        } else {
            Self::none()
        }
    }

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
    pub(crate) fn split(self) -> (bool, Vec<ClientRequest>) {
        let redrew = self.redraws();
        (redrew, self.into_requests())
    }
}

impl Effects {
    #[must_use]
    pub(crate) fn requests_anything(&self) -> bool {
        self.0
            .iter()
            .any(|effect| matches!(effect, Effect::Send(_)))
    }
}
