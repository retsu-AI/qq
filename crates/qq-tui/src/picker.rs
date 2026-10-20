//! Shared state and behaviour for every filterable selection list: the model,
//! theme, session, and command pickers, and the slash autocomplete cursor.
//!
//! A `Picker<T>` owns its items, the query, and the cursor into the *filtered*
//! view of those items. Every picker answers the same keys the same way
//! (`Up`/`Down` move, `Backspace` and typing edit the query); the caller
//! decides what `Enter` means and how a row is drawn.

use crate::app::terminal_safe_character;

/// Maximum bytes accepted into a picker search query.
pub(crate) const MAX_QUERY_BYTES: usize = 256;

/// Something a picker can list and filter.
pub(crate) trait PickerItem {
    /// Text the query is matched against, case-insensitively, as a
    /// subsequence. Return every spelling a user might type.
    fn search_text<'a>(&'a self, out: &mut Vec<&'a str>);

    /// Whether the item is listed while the query is empty. Items that are
    /// not still match a search: the session picker keeps agents out of the
    /// default list but finds them by name.
    fn listed(&self) -> bool {
        true
    }
}

impl PickerItem for usize {
    fn search_text<'a>(&'a self, _: &mut Vec<&'a str>) {}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Picker<T = ()> {
    items: Vec<T>,
    pub query: String,
    /// Cursor into the filtered list.
    selected: usize,
    /// Indices into `items` matching `query`, in item order.
    filtered: Vec<usize>,
}

impl<T> Default for Picker<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            query: String::new(),
            selected: 0,
            filtered: Vec::new(),
        }
    }
}

/// A picker with no items: the slash autocomplete cursor, whose list is
/// computed from the composer text by the caller.
impl Picker<()> {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Cursor into a filtered list of `len` entries. Clamps to the last entry
    /// so a shrinking list never leaves the cursor dangling.
    #[must_use]
    pub(crate) fn selected(&self, len: usize) -> usize {
        self.selected.min(len.saturating_sub(1))
    }

    pub(crate) fn select(&mut self, index: usize) {
        self.selected = index;
    }

    pub(crate) fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub(crate) fn move_down(&mut self, len: usize) {
        self.selected = (self.selected + 1).min(len.saturating_sub(1));
    }
}

impl<T: PickerItem> Picker<T> {
    #[must_use]
    pub(crate) fn with_items(items: Vec<T>) -> Self {
        let mut picker = Self {
            items,
            ..Self::default()
        };
        picker.refilter();
        picker
    }

    /// Replace the items, keeping the cursor on the same logical item where
    /// `identity` still finds it in the new list.
    pub(crate) fn replace_items<K: PartialEq>(
        &mut self,
        items: Vec<T>,
        identity: impl Fn(&T) -> K,
    ) {
        let before = self.current().map(&identity);
        self.items = items;
        self.refilter();
        if let Some(before) = before
            && let Some(position) = self
                .filtered
                .iter()
                .position(|index| identity(&self.items[*index]) == before)
        {
            self.selected = position;
        }
    }

    pub(crate) fn items(&self) -> &[T] {
        &self.items
    }

    /// Filtered items in order, each with its index into `items`.
    pub(crate) fn filtered(&self) -> impl ExactSizeIterator<Item = (usize, &T)> + '_ {
        self.filtered
            .iter()
            .map(|index| (*index, &self.items[*index]))
    }

    /// Position of the cursor within the filtered list.
    #[must_use]
    pub(crate) fn cursor(&self) -> usize {
        self.selected.min(self.filtered.len().saturating_sub(1))
    }

    /// The highlighted item, if any.
    #[must_use]
    pub(crate) fn current(&self) -> Option<&T> {
        self.filtered
            .get(self.cursor())
            .map(|index| &self.items[*index])
    }

    /// Move the cursor to the item at `index` in `items`, if it is visible.
    pub(crate) fn select_item(&mut self, index: usize) -> bool {
        match self
            .filtered
            .iter()
            .position(|candidate| *candidate == index)
        {
            Some(position) => {
                self.selected = position;
                true
            }
            None => false,
        }
    }

    pub(crate) fn move_up(&mut self) -> bool {
        let before = self.cursor();
        self.selected = before.saturating_sub(1);
        self.selected != before
    }

    pub(crate) fn move_down(&mut self) -> bool {
        let before = self.cursor();
        self.selected = (before + 1).min(self.filtered.len().saturating_sub(1));
        self.selected != before
    }

    /// Append sanitized characters to the query, bounded by
    /// [`MAX_QUERY_BYTES`]. Resets the cursor because the filtered list
    /// changed shape. Returns whether the query changed.
    pub(crate) fn push_query(&mut self, text: &str) -> bool {
        let before = self.query.len();
        for character in text.chars() {
            if self.query.len() + character.len_utf8() > MAX_QUERY_BYTES {
                break;
            }
            if let Some(character) = terminal_safe_character(character) {
                self.query.push(character);
            }
        }
        if self.query.len() == before {
            return false;
        }
        self.selected = 0;
        self.refilter();
        true
    }

    /// Remove the last query character and reset the cursor. Returns whether
    /// anything was removed.
    pub(crate) fn pop_query(&mut self) -> bool {
        if self.query.pop().is_none() {
            return false;
        }
        self.selected = 0;
        self.refilter();
        true
    }

    fn refilter(&mut self) {
        let query = self.query.to_ascii_lowercase();
        let mut texts = Vec::new();
        self.filtered.clear();
        for (index, item) in self.items.iter().enumerate() {
            if query.is_empty() {
                if item.listed() {
                    self.filtered.push(index);
                }
                continue;
            }
            texts.clear();
            item.search_text(&mut texts);
            if texts.iter().any(|text| fuzzy_matches(&query, text)) {
                self.filtered.push(index);
            }
        }
    }
}

/// Case-insensitive subsequence match: every query character appears in
/// `candidate` in order. `"mdl"` matches `"models"`; `"lsd"` does not.
#[must_use]
pub(crate) fn fuzzy_matches(query: &str, candidate: &str) -> bool {
    let mut query = query.chars();
    let mut wanted = match query.next() {
        Some(character) => character,
        None => return true,
    };
    for character in candidate.chars() {
        if character.to_ascii_lowercase() == wanted {
            match query.next() {
                Some(next) => wanted = next,
                None => return true,
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Named(&'static str);

    impl PickerItem for Named {
        fn search_text<'a>(&'a self, out: &mut Vec<&'a str>) {
            out.push(self.0);
        }
    }

    fn names() -> Vec<Named> {
        vec![Named("alpha"), Named("beta"), Named("gamma")]
    }

    #[test]
    fn cursor_clamps_to_the_filtered_length() {
        let mut picker = Picker::with_items(names());
        assert!(picker.move_down());
        assert!(picker.move_down());
        assert!(!picker.move_down(), "already at the end");
        assert_eq!(picker.current().map(|item| item.0), Some("gamma"));
        picker.push_query("a");
        assert_eq!(picker.cursor(), 0, "a query change resets the cursor");
        assert_eq!(picker.filtered().len(), 3);
        picker.push_query("l");
        assert_eq!(picker.filtered().len(), 1);
        assert_eq!(picker.current().map(|item| item.0), Some("alpha"));
    }

    #[test]
    fn matches_is_case_insensitive_and_empty_query_matches_everything() {
        let mut picker = Picker::with_items(names());
        assert_eq!(picker.filtered().len(), 3);
        picker.push_query("GAM");
        assert_eq!(picker.current().map(|item| item.0), Some("gamma"));
        assert!(picker.pop_query());
        assert!(picker.pop_query());
        assert!(picker.pop_query());
        assert!(!picker.pop_query());
        assert_eq!(picker.filtered().len(), 3);
    }

    #[test]
    fn fuzzy_matching_is_an_ordered_subsequence() {
        assert!(fuzzy_matches("mdl", "models"));
        assert!(fuzzy_matches("", "anything"));
        assert!(!fuzzy_matches("lsd", "models"));
        assert!(!fuzzy_matches("modelsx", "models"));
    }

    #[test]
    fn replacing_items_keeps_the_cursor_on_the_same_identity() {
        let mut picker = Picker::with_items(names());
        picker.move_down();
        assert_eq!(picker.current().map(|item| item.0), Some("beta"));
        picker.replace_items(vec![Named("zeta"), Named("alpha"), Named("beta")], |item| {
            item.0
        });
        assert_eq!(picker.current().map(|item| item.0), Some("beta"));
        picker.replace_items(vec![Named("only")], |item| item.0);
        assert_eq!(picker.current().map(|item| item.0), Some("only"));
    }

    #[test]
    fn query_is_bounded_and_sanitized() {
        let mut picker = Picker::with_items(names());
        assert!(picker.push_query("a\u{1b}b"));
        assert_eq!(picker.query, "ab");
        let long = "x".repeat(MAX_QUERY_BYTES + 10);
        picker.push_query(&long);
        assert_eq!(picker.query.len(), MAX_QUERY_BYTES);
    }
}
