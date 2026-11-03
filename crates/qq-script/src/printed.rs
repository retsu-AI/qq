#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Printed {
    pub head: String,
    pub tail: String,
    pub omitted_bytes: usize,
}

impl Printed {
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.head.len() + self.tail.len() + self.omitted_bytes
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total_bytes() == 0
    }

    #[must_use]
    pub fn render(&self) -> String {
        if self.omitted_bytes == 0 {
            return format!("{}{}", self.head, self.tail);
        }
        format!(
            "{}\n[... {} bytes of printed output omitted ...]\n{}",
            self.head, self.omitted_bytes, self.tail
        )
    }
}

#[derive(Debug)]
pub(crate) struct PrintBuffer {
    head_limit: usize,
    tail_limit: usize,
    printed: Printed,
}

impl PrintBuffer {
    pub(crate) fn new(head_limit: usize, tail_limit: usize) -> Self {
        Self {
            head_limit,
            tail_limit,
            printed: Printed::default(),
        }
    }

    pub(crate) fn push_str(&mut self, mut text: &str) {
        if self.printed.tail.is_empty() && self.printed.omitted_bytes == 0 {
            let room = self.head_limit.saturating_sub(self.printed.head.len());
            let cut = floor_boundary(text, room);
            self.printed.head.push_str(&text[..cut]);
            text = &text[cut..];
        }
        if text.is_empty() {
            return;
        }
        self.printed.tail.push_str(text);
        if self.printed.tail.len() > self.tail_limit {
            let excess = self.printed.tail.len() - self.tail_limit;
            let cut = ceil_boundary(&self.printed.tail, excess);
            self.printed.omitted_bytes += cut;
            self.printed.tail.drain(..cut);
        }
    }

    pub(crate) fn into_printed(self) -> Printed {
        self.printed
    }
}

fn floor_boundary(text: &str, index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    (0..=index)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0)
}

fn ceil_boundary(text: &str, index: usize) -> usize {
    (index..=text.len())
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_output_is_kept_whole() {
        let mut buffer = PrintBuffer::new(8, 8);
        buffer.push_str("hello\n");
        let printed = buffer.into_printed();
        assert_eq!(printed.render(), "hello\n");
        assert_eq!(printed.omitted_bytes, 0);
    }

    #[test]
    fn long_output_keeps_head_and_tail() {
        let mut buffer = PrintBuffer::new(4, 4);
        for chunk in ["abcdef", "ghij", "klmnop"] {
            buffer.push_str(chunk);
        }
        let printed = buffer.into_printed();
        assert_eq!(printed.head, "abcd");
        assert_eq!(printed.tail, "mnop");
        assert_eq!(printed.omitted_bytes, 8);
        assert_eq!(printed.total_bytes(), 16);
        assert_eq!(
            printed.render(),
            "abcd\n[... 8 bytes of printed output omitted ...]\nmnop"
        );
    }

    #[test]
    fn cuts_land_on_char_boundaries() {
        let mut buffer = PrintBuffer::new(3, 3);
        buffer.push_str("ééééé");
        let printed = buffer.into_printed();
        assert_eq!(printed.head, "é");
        assert_eq!(printed.tail, "é");
        assert_eq!(printed.total_bytes(), 10);
    }
}
