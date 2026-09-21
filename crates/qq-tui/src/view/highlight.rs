//! Off-tick syntax highlighting for completed messages.
//!
//! Tree-sitter costs roughly 150 µs per short code block, so a first frame
//! with dozens of completed messages would spend more than ten milliseconds
//! highlighting on the render tick. Instead the renderer caches a plain layout
//! immediately and requests a highlighted layout here. Jobs run on Tokio's
//! blocking pool with a bounded in-flight count; results return on a bounded
//! channel that the event loop drains like any other update. A result whose
//! generation no longer matches the cached entry is dropped.

use qq_protocol::MessageId;
use tokio::sync::mpsc;

use crate::{render::Line, theme};

/// Concurrent highlight jobs. Beyond this, requests are skipped and retried on
/// a later frame; the plain layout stays visible in the meantime.
const MAX_IN_FLIGHT: usize = 4;
/// Completed results waiting for the loop. Sized above the in-flight cap so a
/// finished job never blocks on a slow loop.
const RESULT_CAPACITY: usize = 16;

/// Identity of one highlight request so stale results can be discarded.
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
    /// Schedule `layout` on the blocking pool. Returns `false` without
    /// scheduling when the in-flight cap is reached or no runtime is
    /// available; callers keep the plain layout and may retry later.
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
        // Style helpers read a thread-local palette; the blocking thread has
        // never had one installed, so without this the layout comes back in
        // the compiled `qq` colors under every other theme.
        let palette = theme::active();
        handle.spawn_blocking(move || {
            theme::activate(palette);
            let lines = layout();
            // A full inbox means the loop is gone or hopelessly behind; the
            // plain layout remains correct, so dropping is the right outcome.
            let _ = results.try_send(Highlighted { key, lines });
        });
        true
    }

    /// Await the next finished job. Pending forever when nothing is in
    /// flight so it can sit in a `select!` without spinning.
    pub(crate) async fn next(&mut self) -> Highlighted {
        if self.in_flight == 0 {
            return std::future::pending().await;
        }
        match self.inbox.recv().await {
            Some(result) => {
                self.in_flight -= 1;
                result
            }
            // The sender half lives in `self`, so the channel cannot close.
            None => std::future::pending().await,
        }
    }

    /// Non-blocking drain used by tests and benchmarks.
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
        // Saturated: the caller keeps its plain layout and retries later.
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
            ..Palette::QQ
        };
        theme::activate(themed);
        let mut highlighter = Highlighter::default();
        assert!(highlighter.request(key(1), || vec![Line::styled("kw", code_keyword())]));
        let result = highlighter.next().await;
        assert_eq!(result.lines[0].spans[0].style.color, Some(Color::Magenta));
        theme::activate(Palette::QQ);
    }
}
