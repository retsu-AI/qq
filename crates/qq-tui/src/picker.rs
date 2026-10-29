use crate::app::terminal_safe_character;

pub(crate) const MAX_QUERY_BYTES: usize = 256;

pub(crate) trait PickerItem {
    fn search_text<'a>(&'a self, out: &mut Vec<&'a str>);
}

impl PickerItem for usize {
    fn search_text<'a>(&'a self, _: &mut Vec<&'a str>) {}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Picker<T = ()> {
    items: Vec<T>,
    pub query: String,
    selected: usize,
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

impl Picker<()> {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

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

    pub(crate) fn filtered(&self) -> impl ExactSizeIterator<Item = (usize, &T)> + '_ {
        self.filtered
            .iter()
            .map(|index| (*index, &self.items[*index]))
    }

    #[must_use]
    pub(crate) fn cursor(&self) -> usize {
        self.selected.min(self.filtered.len().saturating_sub(1))
    }

    #[must_use]
    pub(crate) fn current(&self) -> Option<&T> {
        self.filtered
            .get(self.cursor())
            .map(|index| &self.items[*index])
    }

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
                self.filtered.push(index);
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
