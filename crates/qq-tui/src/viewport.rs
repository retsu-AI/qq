use std::{collections::HashMap, ops::Range};

use qq_protocol::{MessageId, RunId, SessionId, ToolCallId};

pub(crate) const MAX_PANES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum View {
    Transcript(Option<SessionId>),
    Attention,
    SideQuestions(SessionId),
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
            Self::SideQuestions(session) => Some(session),
            Self::Attention | Self::Changes => None,
        }
    }

    pub(crate) const fn transcript(self) -> Option<SessionId> {
        match self {
            Self::Transcript(session) => session,
            Self::SideQuestions(_) | Self::Attention | Self::Changes => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Block {
    Message(MessageId),
    Code(MessageId, usize),
    Tool(ToolCallId),
    Reasoning(RunId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hit {
    Tool(ToolCallId),
    Receipt(RunId, u32),
    Session(SessionId),
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptPane {
    pub view: View,
    pub viewport: Viewport,
    pub live_message_ranges: HashMap<MessageId, Range<usize>>,
    pub blocks: Vec<(Block, Range<usize>)>,
    pub hits: Vec<(usize, Hit)>,
    pub reveal: Option<(Block, String)>,
    pub relayout: bool,
    pub output_arrived: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Viewport {
    context: Option<View>,
    body_rows: usize,
    height: usize,
    offset: usize,
    unseen: usize,
    width: usize,
    count_growth: bool,
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
                offset: 0,
                unseen: 0,
                width: self.width,
                count_growth: false,
            };
            return;
        }
        if self.offset > 0 && self.height > 0 && !preserve_tail_anchor {
            let top = self
                .body_rows
                .saturating_sub(self.offset)
                .saturating_sub(self.height);
            if self.count_growth {
                self.unseen = self
                    .unseen
                    .saturating_add(body_rows.saturating_sub(self.body_rows));
            }
            self.offset = body_rows.saturating_sub(top.saturating_add(height));
        }
        self.body_rows = body_rows;
        self.height = height;
        self.offset = self.offset.min(body_rows.saturating_sub(height));
        self.unseen = self.unseen.min(self.offset);
    }

    pub(crate) const fn offset(&self) -> usize {
        self.offset
    }

    pub(crate) const fn unseen(&self) -> usize {
        self.unseen
    }

    pub(crate) fn note_layout(&mut self, width: usize, live: bool) {
        self.count_growth = live && self.width == width;
        self.width = width;
    }

    pub(crate) const fn height(&self) -> usize {
        self.height
    }

    pub(crate) fn window(&self) -> Range<usize> {
        let end = self.body_rows.saturating_sub(self.offset);
        end.saturating_sub(self.height)..end
    }

    pub(crate) fn reveal(&mut self, rows: &Range<usize>) -> bool {
        let before = self.offset;
        let window = self.window();
        let top = if rows.start < window.start || rows.len() > self.height {
            rows.start
        } else if rows.end > window.end {
            rows.end.saturating_sub(self.height)
        } else {
            window.start
        };
        self.offset = self
            .body_rows
            .saturating_sub(top.saturating_add(self.height))
            .min(self.body_rows.saturating_sub(self.height));
        self.unseen = self.unseen.min(self.offset);
        self.offset != before
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
        self.unseen = self.unseen.min(self.offset);
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
    fn rows_that_arrive_below_a_scrolled_view_count_as_unseen_until_scrolled_to() {
        let mut viewport = Viewport::default();
        let context = View::Transcript(Some(session(1)));
        viewport.note_layout(80, true);
        viewport.update(context, 100, 10, false);
        viewport.note_layout(80, true);
        viewport.update(context, 104, 10, false);
        assert_eq!(viewport.unseen(), 0, "following the tail sees every row");
        assert!(viewport.scroll(20));
        viewport.update(context, 110, 10, false);
        viewport.update(context, 113, 10, false);
        assert_eq!(viewport.window(), 74..84, "the view stays put");
        assert_eq!(viewport.unseen(), 9);
        viewport.update(context, 113, 10, true);
        assert_eq!(viewport.unseen(), 9, "a re-layout adds nothing new");
        viewport.note_layout(60, true);
        viewport.update(context, 140, 10, false);
        assert_eq!(
            viewport.unseen(),
            9,
            "rewrapping at a new width is not output"
        );
        viewport.note_layout(60, false);
        viewport.update(context, 150, 10, false);
        assert_eq!(
            viewport.unseen(),
            9,
            "rows from an idle session are not output"
        );
        viewport.note_layout(60, true);
        viewport.update(context, 140, 10, false);
        assert_eq!(viewport.window(), 74..84, "the top row never moved");
        assert!(viewport.scroll(-52));
        assert_eq!(viewport.unseen(), 4, "scrolling down reveals some");
        assert!(viewport.scroll(isize::MIN));
        assert_eq!(viewport.unseen(), 0);
        assert!(viewport.scroll(5));
        viewport.update(View::Transcript(Some(session(2))), 50, 10, false);
        assert_eq!(viewport.unseen(), 0);
    }

    #[test]
    fn revealing_a_block_scrolls_just_enough_to_show_its_top() {
        let mut viewport = Viewport::default();
        let context = View::Transcript(Some(session(1)));
        viewport.update(context, 100, 10, false);
        assert_eq!(viewport.window(), 90..100);
        assert!(viewport.reveal(&(40..44)));
        assert_eq!(viewport.window(), 40..50);
        assert!(!viewport.reveal(&(42..45)));
        assert!(viewport.reveal(&(95..99)));
        assert_eq!(viewport.window(), 89..99);
        assert!(viewport.reveal(&(10..40)));
        assert_eq!(viewport.window(), 10..20);
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
}
