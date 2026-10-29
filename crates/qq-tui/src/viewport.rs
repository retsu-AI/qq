use std::{collections::HashMap, ops::Range};

use qq_protocol::{MessageId, SessionId};

pub(crate) const MAX_PANES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum View {
    Transcript(Option<SessionId>),
    Attention,
    SideQuestions(SessionId),
    Goal(SessionId),
    Changes,
}

impl Default for View {
    fn default() -> Self {
        Self::Transcript(None)
    }
}

impl View {
    pub(crate) const fn session(self) -> Option<SessionId> {
        match self {
            Self::Transcript(session) => session,
            Self::SideQuestions(session) | Self::Goal(session) => Some(session),
            Self::Attention | Self::Changes => None,
        }
    }

    pub(crate) const fn transcript(self) -> Option<SessionId> {
        match self {
            Self::Transcript(session) => session,
            Self::SideQuestions(_) | Self::Goal(_) | Self::Attention | Self::Changes => None,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptPane {
    pub view: View,
    pub viewport: Viewport,
    pub live_message_ranges: HashMap<MessageId, Range<usize>>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Viewport {
    context: Option<View>,
    body_rows: usize,
    height: usize,
    offset: usize,
}

impl Viewport {
    pub(crate) fn update(
        &mut self,
        context: View,
        body_rows: usize,
        height: usize,
        preserve_tail_anchor: bool,
    ) {
        if self.context != Some(context) {
            *self = Self {
                context: Some(context),
                body_rows,
                height,
                offset: if matches!(context, View::Goal(_)) {
                    body_rows.saturating_sub(height)
                } else {
                    0
                },
            };
            return;
        }
        if (self.offset > 0 || matches!(context, View::Goal(_)))
            && self.height > 0
            && !preserve_tail_anchor
        {
            let top = self
                .body_rows
                .saturating_sub(self.offset)
                .saturating_sub(self.height);
            self.offset = body_rows.saturating_sub(top.saturating_add(height));
        }
        self.body_rows = body_rows;
        self.height = height;
        self.offset = self.offset.min(body_rows.saturating_sub(height));
    }

    pub(crate) const fn offset(&self) -> usize {
        self.offset
    }

    pub(crate) const fn height(&self) -> usize {
        self.height
    }

    pub(crate) fn intersects_or_follows(&self, rows: &Range<usize>) -> bool {
        self.height > 0 && self.body_rows.saturating_sub(self.offset) > rows.start
    }

    pub(crate) fn scroll(&mut self, rows: isize) -> bool {
        let before = self.offset;
        let maximum = self.body_rows.saturating_sub(self.height);
        self.offset = match rows.cmp(&0) {
            std::cmp::Ordering::Greater => before.saturating_add(rows.unsigned_abs()).min(maximum),
            std::cmp::Ordering::Less => before.saturating_sub(rows.unsigned_abs()),
            std::cmp::Ordering::Equal => before,
        };
        self.offset != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(byte: u8) -> SessionId {
        SessionId::from_bytes([byte; 16])
    }

    #[test]
    fn viewport_follows_the_tail_until_scrolled_and_keeps_its_top_row() {
        let mut viewport = Viewport::default();
        let context = View::Transcript(Some(session(1)));
        viewport.update(context, 100, 10, false);
        assert_eq!(viewport.offset(), 0);
        assert!(viewport.scroll(5));
        assert_eq!(viewport.offset(), 5);
        viewport.update(context, 110, 10, false);
        assert_eq!(viewport.offset(), 15);
        viewport.update(context, 120, 10, true);
        assert_eq!(viewport.offset(), 15);
        assert!(viewport.scroll(-100));
        assert_eq!(viewport.offset(), 0);
        assert!(viewport.scroll(isize::MAX));
        assert_eq!(viewport.offset(), 110);
        assert!(!viewport.scroll(0));
        viewport.update(View::Transcript(Some(session(2))), 50, 10, false);
        assert_eq!(viewport.offset(), 0);
        viewport.update(View::Attention, 50, 10, false);
        assert_eq!(viewport.offset(), 0);
    }

    #[test]
    fn side_view_follows_its_source_session_without_showing_its_transcript() {
        let view = View::SideQuestions(session(4));
        assert_eq!(view.session(), Some(session(4)));
        assert_eq!(view.transcript(), None);
        assert_eq!(
            View::Transcript(Some(session(4))).transcript(),
            Some(session(4))
        );
    }

    #[test]
    fn goal_viewport_opens_at_the_top_and_keeps_status_visible_when_diagnostics_grow() {
        let mut viewport = Viewport::default();
        let view = View::Goal(SessionId::from_bytes([0x61; 16]));
        viewport.update(view, 10, 20, false);
        assert_eq!(viewport.offset(), 0);
        viewport.update(view, 70, 20, false);
        assert_eq!(viewport.offset(), 50);
        viewport.scroll(-10);
        viewport.update(view, 100, 20, false);
        assert_eq!(viewport.offset(), 70);
    }
}
