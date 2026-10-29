use qq_protocol::MessageId;
use tokio::sync::mpsc;

use crate::{render::Line, theme};

const MAX_IN_FLIGHT: usize = 4;
const RESULT_CAPACITY: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HighlightKey {
    pub message_id: MessageId,
    pub width: usize,
    pub output_bytes: usize,
    pub refusal_bytes: usize,
    pub loaded_through: u64,
}

pub(crate) struct Highlighted {
    pub key: HighlightKey,
    pub lines: Vec<Line>,
}

pub(crate) struct Highlighter {
    results: mpsc::Sender<Highlighted>,
    inbox: mpsc::Receiver<Highlighted>,
    in_flight: usize,
}

impl Default for Highlighter {
    fn default() -> Self {
        let (results, inbox) = mpsc::channel(RESULT_CAPACITY);
        Self {
            results,
            inbox,
            in_flight: 0,
        }
    }
}

impl Highlighter {
    pub(crate) fn request(
        &mut self,
        key: HighlightKey,
        layout: impl FnOnce() -> Vec<Line> + Send + 'static,
    ) -> bool {
        if self.in_flight >= MAX_IN_FLIGHT {
            return false;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return false;
        };
        let results = self.results.clone();
        self.in_flight += 1;
        let palette = theme::active();
        handle.spawn_blocking(move || {
            theme::activate(palette);
            let lines = layout();
            let _ = results.try_send(Highlighted { key, lines });
        });
        true
    }

    pub(crate) async fn next(&mut self) -> Highlighted {
        if self.in_flight == 0 {
            return std::future::pending().await;
        }
        match self.inbox.recv().await {
            Some(result) => {
                self.in_flight -= 1;
                result
            }
            None => std::future::pending().await,
        }
    }

    #[cfg(any(test, feature = "bench-support"))]
    pub(crate) fn try_next(&mut self) -> Option<Highlighted> {
        let result = self.inbox.try_recv().ok()?;
        self.in_flight -= 1;
        Some(result)
    }

    #[cfg(any(test, feature = "bench-support"))]
    pub(crate) fn in_flight(&self) -> usize {
        self.in_flight
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> HighlightKey {
        HighlightKey {
            message_id: MessageId::from_bytes([byte; 16]),
            width: 80,
            output_bytes: 1,
            refusal_bytes: 0,
            loaded_through: 0,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn in_flight_is_bounded_and_results_are_delivered_in_order_of_completion() {
        let mut highlighter = Highlighter::default();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = std::sync::Arc::new(std::sync::Mutex::new(release_rx));
        for byte in 0..MAX_IN_FLIGHT as u8 {
            let gate = std::sync::Arc::clone(&release_rx);
            assert!(highlighter.request(key(byte), move || {
                gate.lock().unwrap().recv().unwrap();
                vec![Line::default()]
            }));
        }
        assert_eq!(highlighter.in_flight(), MAX_IN_FLIGHT);
        assert!(!highlighter.request(key(99), Vec::new));
        assert!(highlighter.try_next().is_none());

        for _ in 0..MAX_IN_FLIGHT {
            release_tx.send(()).unwrap();
        }
        let mut delivered = 0;
        while delivered < MAX_IN_FLIGHT {
            let result = highlighter.next().await;
            assert_eq!(result.lines.len(), 1);
            delivered += 1;
        }
        assert_eq!(highlighter.in_flight(), 0);
        assert!(highlighter.request(key(99), Vec::new));
        assert!(highlighter.next().await.lines.is_empty());
    }

    #[test]
    fn request_without_a_runtime_is_refused() {
        let mut highlighter = Highlighter::default();
        assert!(!highlighter.request(key(1), Vec::new));
        assert_eq!(highlighter.in_flight(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn jobs_lay_out_under_the_requesting_threads_palette() {
        use crate::{render::code_keyword, theme::Palette};
        use crossterm::style::Color;

        let themed = Palette {
            syn_keyword: Color::Magenta,
            ..Palette::TERMINAL
        };
        theme::activate(themed);
        let mut highlighter = Highlighter::default();
        assert!(highlighter.request(key(1), || vec![Line::styled("kw", code_keyword())]));
        let result = highlighter.next().await;
        assert_eq!(result.lines[0].spans[0].style.color, Some(Color::Magenta));
        theme::activate(Palette::TERMINAL);
    }
}
