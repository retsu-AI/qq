use std::borrow::Cow;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{App, message_source};
use crate::{
    effect::{Effects, Redraw},
    input::{Find, Overlay},
    viewport::Block,
};

pub(crate) fn find_matches(text: &str, lowered_query: &str) -> bool {
    !lowered_query.is_empty() && text.to_lowercase().contains(lowered_query)
}

impl App {
    pub(super) fn open_find(&mut self) -> Effects {
        if self.view().transcript().is_none() {
            self.set_info("find works in a transcript".to_owned());
            return Effects::redraw(Redraw::Immediate);
        }
        self.find_opened = None;
        self.overlay = Some(Overlay::Find(Find {
            origin: self.viewport().window().start,
            ..Find::default()
        }));
        Effects::redraw(Redraw::Immediate)
    }

    pub(super) fn close_find(&mut self) {
        if let Some(Overlay::Find(_)) = self.overlay {
            self.overlay = None;
            self.block_cursor = None;
        }
        self.find_opened = None;
        self.pane_mut().reveal = None;
    }

    pub(crate) fn find(&self) -> Option<&Find> {
        if let Some(Overlay::Find(find)) = &self.overlay {
            return Some(find);
        }
        None
    }

    fn find_mut(&mut self) -> Option<&mut Find> {
        if let Some(Overlay::Find(find)) = &mut self.overlay {
            return Some(find);
        }
        None
    }

    pub(super) fn handle_find_key(&mut self, key: KeyEvent) -> Effects {
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => {
                self.close_find();
                Effects::redraw(Redraw::Immediate)
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => self.step_find(false),
            KeyCode::Enter | KeyCode::Down => self.step_find(true),
            KeyCode::Up => self.step_find(false),
            KeyCode::PageUp | KeyCode::PageDown | KeyCode::End => self.handle_compose_key(key),
            KeyCode::Backspace => {
                let popped = self
                    .find_mut()
                    .is_some_and(|find| find.query.pop().is_some());
                if popped {
                    self.search_find();
                }
                Effects::changed_now(popped)
            }
            KeyCode::Char(character) if plain => {
                let mut encoded = [0; 4];
                self.push_find_text(character.encode_utf8(&mut encoded))
            }
            _ => Effects::none(),
        }
    }

    pub(super) fn push_find_text(&mut self, text: &str) -> Effects {
        let pushed = self.find_mut().is_some_and(|find| find.push_text(text));
        if pushed {
            self.search_find();
        }
        Effects::changed_now(pushed)
    }

    fn block_text(&self, block: Block) -> Option<Cow<'_, str>> {
        let session = self.sessions.get(&self.view().transcript()?)?;
        match block {
            Block::Message(id) => session
                .messages
                .as_ref()?
                .iter()
                .find(|message| message.id == id)
                .map(message_source),
            Block::Tool(id) => session
                .tool_calls
                .as_ref()?
                .iter()
                .find(|call| call.id == id)
                .map(|call| {
                    Cow::Owned(crate::view::tool_copy_text(
                        call,
                        session.live_tool_output.get(&id).map(String::as_str),
                    ))
                }),
            Block::Reasoning(run) => session
                .reasoning
                .get(&run)
                .map(|reasoning| Cow::Borrowed(reasoning.text.as_str())),
            Block::Code(..) => None,
        }
    }

    fn search_find(&mut self) {
        let Some((query, top)) = self
            .find()
            .map(|find| (find.query.to_lowercase(), find.origin))
        else {
            return;
        };
        let mut blocks: Vec<(Block, std::ops::Range<usize>)> = self
            .pane()
            .blocks
            .iter()
            .filter(|(block, _)| !matches!(block, Block::Code(..)))
            .cloned()
            .collect();
        blocks.sort_by_key(|(_, range)| range.start);
        blocks.dedup_by_key(|(block, _)| *block);
        let mut matches = Vec::new();
        let mut first_below = None;
        for (block, range) in blocks {
            if self
                .block_text(block)
                .is_some_and(|text| find_matches(&text, &query))
            {
                if first_below.is_none() && range.start >= top {
                    first_below = Some(matches.len());
                }
                matches.push(block);
            }
        }
        let current = (!matches.is_empty()).then(|| first_below.unwrap_or(matches.len() - 1));
        if let Some(find) = self.find_mut() {
            find.matches = matches;
            find.current = current;
        }
        self.show_find_match();
    }

    fn step_find(&mut self, forward: bool) -> Effects {
        let Some(find) = self.find_mut() else {
            return Effects::none();
        };
        let count = find.matches.len();
        let Some(current) = find.current.filter(|_| count > 0) else {
            return Effects::none();
        };
        find.current = Some(if forward {
            (current + 1) % count
        } else {
            (current + count - 1) % count
        });
        self.show_find_match();
        Effects::redraw(Redraw::Immediate)
    }

    fn scroll_tool_to_query(&mut self, id: qq_protocol::ToolCallId) {
        let Some(query) = self.find().map(|find| find.query.to_lowercase()) else {
            return;
        };
        let row = self
            .view()
            .transcript()
            .and_then(|session| self.sessions.get(&session))
            .and_then(|session| session.tool_calls.as_deref())
            .and_then(|calls| calls.iter().find(|call| call.id == id))
            .and_then(|call| crate::view::expanded_panel_match(call, &query));
        if let Some(row) = row {
            self.tool_scroll.insert(
                id,
                row.saturating_sub(crate::view::EXPANDED_WINDOW_ROWS / 2),
            );
        }
    }

    fn show_find_match(&mut self) {
        let target = self
            .find()
            .and_then(|find| find.matches.get(find.current?).copied());
        if self.find_opened.is_some() && self.find_opened != target {
            match self.find_opened.take() {
                Some(Block::Tool(call)) => {
                    self.expanded_tool_calls.remove(&call);
                    self.tool_scroll.remove(&call);
                }
                Some(Block::Reasoning(run)) => {
                    self.open_reasoning.remove(&run);
                }
                Some(Block::Message(_) | Block::Code(..)) | None => {}
            }
        }
        let Some(target) = target else {
            self.block_cursor = None;
            self.pane_mut().reveal = None;
            return;
        };
        let opened = match target {
            Block::Tool(call) => {
                let opened = self.expanded_tool_calls.insert(call);
                if opened || self.find_opened == Some(target) {
                    self.scroll_tool_to_query(call);
                }
                opened
            }
            Block::Reasoning(run) => self.open_reasoning.insert(run),
            Block::Message(_) | Block::Code(..) => false,
        };
        if opened {
            self.find_opened = Some(target);
        }
        self.block_cursor = Some(target);
        self.pane_mut().reveal = Some(target);
    }
}
