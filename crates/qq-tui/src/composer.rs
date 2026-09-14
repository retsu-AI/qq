//! Cursor-aware prompt editing primitives.
//!
//! A bounded `String` is preferable to a rope for the composer's small (64 KiB)
//! documents. It has excellent cache locality and keeps the common append/edit
//! path allocation-free while capacity remains. Storage is hidden here so it
//! can still be replaced later without changing key handling.
//!
//! Large pastes are kept out of the visible text: the composer shows a short
//! `[Pasted N lines]` placeholder and substitutes the real content on submit.
//! Every edit that changes text records an undo entry; kills feed a one-slot
//! kill ring for yank.

use std::collections::VecDeque;

/// Pastes at or above this many lines, or this many bytes, collapse to a
/// placeholder so the composer stays readable.
pub(crate) const PASTE_PLACEHOLDER_LINES: usize = 3;
pub(crate) const PASTE_PLACEHOLDER_BYTES: usize = 512;
/// Undo depth. Each entry is a full text snapshot bounded by the composer
/// limit, so depth times limit bounds memory.
const UNDO_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Paste {
    /// The exact placeholder as it appears in `text`.
    placeholder: String,
    content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    text: String,
    cursor: Option<usize>,
}

#[derive(Debug, Default)]
pub(crate) struct Composer {
    pub(crate) text: String,
    /// UTF-8 byte offset. `None` means the end, which also lets tests and state
    /// restoration replace `text` directly without leaving a stale cursor.
    cursor: Option<usize>,
    /// Character column retained while moving through lines of unequal length.
    preferred_column: Option<usize>,
    pastes: Vec<Paste>,
    undo: VecDeque<Snapshot>,
    /// Most recent kill, yanked by Ctrl-Y.
    kill: String,
}

impl Composer {
    pub(crate) fn cursor(&self) -> usize {
        self.cursor.unwrap_or(self.text.len()).min(self.text.len())
    }

    pub(crate) fn clear(&mut self) {
        self.text.clear();
        self.cursor = None;
        self.preferred_column = None;
        self.pastes.clear();
        self.undo.clear();
    }

    pub(crate) fn replace(&mut self, text: String) {
        self.text = text;
        self.cursor = None;
        self.preferred_column = None;
        self.pastes.clear();
        self.undo.clear();
    }

    /// The text to submit: the visible text with every surviving placeholder
    /// replaced by its pasted content. Placeholders the user deleted expand
    /// to nothing, which is what deleting them means.
    pub(crate) fn expanded(&self) -> String {
        if self.pastes.is_empty() {
            return self.text.clone();
        }
        let mut expanded = self.text.clone();
        for paste in &self.pastes {
            if let Some(index) = expanded.find(&paste.placeholder) {
                expanded.replace_range(index..index + paste.placeholder.len(), &paste.content);
            }
        }
        expanded
    }

    fn snapshot(&mut self) {
        let snapshot = Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
        };
        if self.undo.back() == Some(&snapshot) {
            return;
        }
        self.undo.push_back(snapshot);
        while self.undo.len() > UNDO_DEPTH {
            self.undo.pop_front();
        }
    }

    /// Restore the previous text. Returns false when there is nothing to undo.
    pub(crate) fn undo(&mut self) -> bool {
        let Some(snapshot) = self.undo.pop_back() else {
            return false;
        };
        self.text = snapshot.text;
        self.cursor = snapshot.cursor;
        self.preferred_column = None;
        true
    }

    pub(crate) fn insert(&mut self, character: char) {
        // Consecutive single-character inserts coalesce into one undo step by
        // snapshotting only when the previous edit was not an insert at the
        // same position; simpler: snapshot on word boundaries.
        if character.is_whitespace() || self.undo.is_empty() {
            self.snapshot();
        }
        let cursor = self.cursor();
        self.text.insert(cursor, character);
        self.cursor = Some(cursor + character.len_utf8());
        self.preferred_column = None;
    }

    /// Insert text at the cursor as one undo step. Returns bytes inserted.
    pub(crate) fn insert_str(&mut self, text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }
        self.snapshot();
        let cursor = self.cursor();
        self.text.insert_str(cursor, text);
        self.cursor = Some(cursor + text.len());
        self.preferred_column = None;
        text.len()
    }

    /// The `@` token the cursor sits in or just after: its byte span (the `@`
    /// included) and the text after the `@`. `None` when the cursor is not
    /// on such a token, or the `@` is mid-word (an email) or escaped (`@@`).
    pub(crate) fn mention_token(&self) -> Option<(std::ops::Range<usize>, &str)> {
        let cursor = self.cursor();
        let before = &self.text[..cursor];
        let start = before.rfind('@')?;
        let token = &before[start + 1..];
        if token.contains(char::is_whitespace) || token.contains([')', ']', ',', ';']) {
            return None;
        }
        let preceded_ok = start == 0
            || before[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace() || matches!(c, '(' | '['));
        if !preceded_ok || before[..start].ends_with('@') {
            return None;
        }
        // The token ends at the cursor or the next boundary after it.
        let after = &self.text[cursor..];
        let end = cursor
            + after
                .find(|c: char| c.is_whitespace() || matches!(c, ')' | ']' | ',' | ';'))
                .unwrap_or(after.len());
        Some((start..end, token))
    }

    /// Replace the byte span with `text` and leave the cursor after it, as
    /// one undo step.
    pub(crate) fn replace_span(&mut self, span: std::ops::Range<usize>, text: &str) {
        if span.start > span.end || span.end > self.text.len() {
            return;
        }
        self.snapshot();
        self.text.replace_range(span.clone(), text);
        self.cursor = Some(span.start + text.len());
        self.preferred_column = None;
    }

    /// Insert pasted content. Small pastes go in literally; larger ones are
    /// stored and represented by a placeholder. Returns whether text changed.
    pub(crate) fn paste(&mut self, content: &str) -> bool {
        if content.is_empty() {
            return false;
        }
        let lines = content.lines().count();
        if lines < PASTE_PLACEHOLDER_LINES && content.len() < PASTE_PLACEHOLDER_BYTES {
            return self.insert_str(content) > 0;
        }
        let ordinal = self.pastes.len() + 1;
        let placeholder = format!("[Pasted #{ordinal} {lines} lines]");
        self.insert_str(&placeholder);
        self.pastes.push(Paste {
            placeholder,
            content: content.to_owned(),
        });
        true
    }

    pub(crate) fn backspace(&mut self) -> bool {
        let cursor = self.cursor();
        // Deleting into a placeholder removes the whole placeholder so a
        // half-edited token never expands to a half-paste.
        if let Some(range) = self.placeholder_ending_at(cursor) {
            self.snapshot();
            self.text.drain(range.clone());
            self.cursor = Some(range.start);
            self.preferred_column = None;
            return true;
        }
        let Some((previous, _)) = self.text[..cursor].char_indices().next_back() else {
            return false;
        };
        self.snapshot();
        self.text.drain(previous..cursor);
        self.cursor = Some(previous);
        self.preferred_column = None;
        true
    }

    pub(crate) fn delete(&mut self) -> bool {
        let cursor = self.cursor();
        if let Some(range) = self.placeholder_starting_at(cursor) {
            self.snapshot();
            self.text.drain(range);
            self.cursor = Some(cursor);
            self.preferred_column = None;
            return true;
        }
        let Some(character) = self.text[cursor..].chars().next() else {
            return false;
        };
        self.snapshot();
        self.text.drain(cursor..cursor + character.len_utf8());
        self.cursor = Some(cursor);
        self.preferred_column = None;
        true
    }

    fn placeholder_ending_at(&self, cursor: usize) -> Option<std::ops::Range<usize>> {
        self.pastes.iter().find_map(|paste| {
            let start = cursor.checked_sub(paste.placeholder.len())?;
            (self.text.is_char_boundary(start) && self.text[start..cursor] == paste.placeholder)
                .then_some(start..cursor)
        })
    }

    fn placeholder_starting_at(&self, cursor: usize) -> Option<std::ops::Range<usize>> {
        self.pastes.iter().find_map(|paste| {
            self.text[cursor..]
                .starts_with(&paste.placeholder)
                .then_some(cursor..cursor + paste.placeholder.len())
        })
    }

    pub(crate) fn move_left(&mut self) -> bool {
        let cursor = self.cursor();
        if let Some(range) = self.placeholder_ending_at(cursor) {
            self.cursor = Some(range.start);
            self.preferred_column = None;
            return true;
        }
        let Some((previous, _)) = self.text[..cursor].char_indices().next_back() else {
            return false;
        };
        self.cursor = Some(previous);
        self.preferred_column = None;
        true
    }

    pub(crate) fn move_right(&mut self) -> bool {
        let cursor = self.cursor();
        if let Some(range) = self.placeholder_starting_at(cursor) {
            self.cursor = Some(range.end);
            self.preferred_column = None;
            return true;
        }
        let Some(character) = self.text[cursor..].chars().next() else {
            return false;
        };
        self.cursor = Some(cursor + character.len_utf8());
        self.preferred_column = None;
        true
    }

    /// Byte offset of the start of the word before the cursor: skip
    /// whitespace backwards, then the word itself.
    fn word_start_before(&self, cursor: usize) -> usize {
        let text = &self.text[..cursor];
        let trimmed = text.trim_end_matches(|c: char| c.is_whitespace() && c != '\n');
        if trimmed.len() < text.len() && trimmed.ends_with('\n') {
            return trimmed.len();
        }
        let trimmed = text.trim_end_matches(char::is_whitespace);
        trimmed
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map_or(0, |(index, c)| index + c.len_utf8())
    }

    /// Byte offset just past the word after the cursor.
    fn word_end_after(&self, cursor: usize) -> usize {
        let rest = &self.text[cursor..];
        let skipped = rest.len() - rest.trim_start_matches(char::is_whitespace).len();
        let word = &rest[skipped..];
        let end = word.find(char::is_whitespace).unwrap_or(word.len());
        cursor + skipped + end
    }

    pub(crate) fn move_word_left(&mut self) -> bool {
        let cursor = self.cursor();
        if cursor == 0 {
            return false;
        }
        self.cursor = Some(self.word_start_before(cursor));
        self.preferred_column = None;
        true
    }

    pub(crate) fn move_word_right(&mut self) -> bool {
        let cursor = self.cursor();
        if cursor >= self.text.len() {
            return false;
        }
        self.cursor = Some(self.word_end_after(cursor));
        self.preferred_column = None;
        true
    }

    pub(crate) fn move_line_start(&mut self) -> bool {
        let cursor = self.cursor();
        let start = self.text[..cursor].rfind('\n').map_or(0, |i| i + 1);
        self.preferred_column = None;
        if start == cursor {
            return false;
        }
        self.cursor = Some(start);
        true
    }

    pub(crate) fn move_line_end(&mut self) -> bool {
        let cursor = self.cursor();
        let end = self.text[cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| cursor + i);
        self.preferred_column = None;
        if end == cursor {
            return false;
        }
        self.cursor = Some(end);
        true
    }

    fn kill_range(&mut self, range: std::ops::Range<usize>) -> bool {
        if range.is_empty() {
            return false;
        }
        self.snapshot();
        self.kill = self.text[range.clone()].to_owned();
        self.text.drain(range.clone());
        self.cursor = Some(range.start);
        self.preferred_column = None;
        true
    }

    /// Ctrl-W / Alt-Backspace: delete the word before the cursor into the
    /// kill slot.
    pub(crate) fn kill_word_back(&mut self) -> bool {
        let cursor = self.cursor();
        let start = self.word_start_before(cursor);
        self.kill_range(start..cursor)
    }

    /// Ctrl-K: delete to the end of the logical line (or the newline itself
    /// when already at the end).
    pub(crate) fn kill_to_line_end(&mut self) -> bool {
        let cursor = self.cursor();
        let end = self.text[cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| cursor + i);
        if end == cursor && end < self.text.len() {
            return self.kill_range(cursor..cursor + 1);
        }
        self.kill_range(cursor..end)
    }

    /// Ctrl-U: delete to the start of the logical line.
    pub(crate) fn kill_to_line_start(&mut self) -> bool {
        let cursor = self.cursor();
        let start = self.text[..cursor].rfind('\n').map_or(0, |i| i + 1);
        self.kill_range(start..cursor)
    }

    /// Ctrl-Y: reinsert the last kill.
    pub(crate) fn yank(&mut self) -> bool {
        if self.kill.is_empty() {
            return false;
        }
        let kill = self.kill.clone();
        self.insert_str(&kill) > 0
    }

    /// Moves between logical newline-delimited lines. Returning false at a
    /// boundary lets the caller use that arrow for prompt history.
    pub(crate) fn move_up(&mut self) -> bool {
        self.move_vertical(false)
    }

    pub(crate) fn move_down(&mut self) -> bool {
        self.move_vertical(true)
    }

    fn move_vertical(&mut self, down: bool) -> bool {
        let cursor = self.cursor();
        let line_start = self.text[..cursor].rfind('\n').map_or(0, |index| index + 1);
        let column = self.text[line_start..cursor].chars().count();
        let preferred = self.preferred_column.unwrap_or(column);

        let (target_start, target_end) = if down {
            let Some(newline) = self.text[cursor..].find('\n').map(|index| cursor + index) else {
                return false;
            };
            let start = newline + 1;
            let end = self.text[start..]
                .find('\n')
                .map_or(self.text.len(), |index| start + index);
            (start, end)
        } else {
            if line_start == 0 {
                return false;
            }
            let end = line_start - 1;
            let start = self.text[..end].rfind('\n').map_or(0, |index| index + 1);
            (start, end)
        };

        let offset = self.text[target_start..target_end]
            .char_indices()
            .nth(preferred)
            .map_or(target_end - target_start, |(offset, _)| offset);
        self.cursor = Some(target_start + offset);
        self.preferred_column = Some(preferred);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_at_unicode_character_boundaries() {
        let mut composer = Composer::default();
        composer.replace("aé".to_owned());
        assert!(composer.move_left());
        composer.insert('!');
        assert_eq!(composer.text, "a!é");
        assert!(composer.backspace());
        assert_eq!(composer.text, "aé");
        assert!(composer.delete());
        assert_eq!(composer.text, "a");
    }

    #[test]
    fn moves_vertically_and_retains_the_preferred_column() {
        let mut composer = Composer::default();
        composer.replace("abcd\nx\nwxyz".to_owned());
        assert!(composer.move_up());
        assert_eq!(composer.cursor(), 6);
        assert!(composer.move_up());
        assert_eq!(composer.cursor(), 4);
        assert!(!composer.move_up());
        assert!(composer.move_down());
        assert_eq!(composer.cursor(), 6);
        assert!(composer.move_down());
        assert_eq!(composer.cursor(), 11);
    }

    #[test]
    fn word_motions_and_kills_feed_the_yank_slot() {
        let mut composer = Composer::default();
        composer.replace("alpha beta  gamma".to_owned());
        assert!(composer.move_word_left());
        assert_eq!(composer.cursor(), 12);
        assert!(composer.move_word_left());
        assert_eq!(composer.cursor(), 6);
        assert!(composer.move_word_right());
        assert_eq!(composer.cursor(), 10);
        assert!(composer.move_line_end());
        assert!(composer.kill_word_back());
        assert_eq!(composer.text, "alpha beta  ");
        assert!(composer.yank());
        assert_eq!(composer.text, "alpha beta  gamma");
        assert!(composer.move_line_start());
        assert!(composer.kill_to_line_end());
        assert_eq!(composer.text, "");
        assert!(composer.yank());
        assert_eq!(composer.text, "alpha beta  gamma");
        assert!(composer.kill_to_line_start());
        assert_eq!(composer.text, "");
    }

    #[test]
    fn kill_to_line_end_at_a_line_end_joins_lines() {
        let mut composer = Composer::default();
        composer.replace("one\ntwo".to_owned());
        composer.move_up();
        composer.move_line_end();
        assert!(composer.kill_to_line_end());
        assert_eq!(composer.text, "onetwo");
    }

    #[test]
    fn undo_restores_prior_text_and_cursor() {
        let mut composer = Composer::default();
        for c in "ab cd".chars() {
            composer.insert(c);
        }
        assert!(composer.kill_word_back());
        assert_eq!(composer.text, "ab ");
        assert!(composer.undo());
        assert_eq!(composer.text, "ab cd");
        assert_eq!(composer.cursor(), 5);
        assert!(composer.undo());
        assert!(composer.text.len() < 5);
        while composer.undo() {}
        assert_eq!(composer.text, "");
    }

    #[test]
    fn large_pastes_collapse_to_placeholders_that_expand_on_submit() {
        let mut composer = Composer::default();
        composer.insert_str("see ");
        assert!(composer.paste("x = 1\ny = 2\nz = 3\n"));
        assert_eq!(composer.text, "see [Pasted #1 3 lines]");
        composer.insert_str(" please");
        assert_eq!(composer.expanded(), "see x = 1\ny = 2\nz = 3\n please");

        // Small pastes go in literally.
        assert!(composer.paste(" ok"));
        assert!(composer.expanded().ends_with(" please ok"));

        // Cursor motion treats the placeholder as one token, and backspace
        // removes it whole so nothing partial can expand.
        composer.replace(String::new());
        composer.paste("a\nb\nc");
        assert_eq!(composer.cursor(), composer.text.len());
        assert!(composer.move_left());
        assert_eq!(composer.cursor(), 0);
        assert!(composer.move_right());
        assert_eq!(composer.cursor(), composer.text.len());
        assert!(composer.backspace());
        assert_eq!(composer.text, "");
        assert_eq!(composer.expanded(), "");
    }
}
