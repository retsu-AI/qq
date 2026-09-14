use std::string::FromUtf8Error;

use crate::ProviderError;

#[derive(Clone, Copy)]
pub(crate) enum Utf8ErrorMessage {
    Static(&'static str),
    WithSource(&'static str),
}

impl Utf8ErrorMessage {
    fn into_provider_error(self, source: FromUtf8Error) -> ProviderError {
        match self {
            Self::Static(message) => ProviderError::Protocol(message.to_owned()),
            Self::WithSource(message) => ProviderError::Protocol(format!("{message}: {source}")),
        }
    }
}

#[derive(Clone, Copy)]
enum SseMode {
    DataOnly,
    Named(Utf8ErrorMessage),
}

/// One dispatched SSE event. `data` is the joined `data:` lines; the adapter
/// parses it once and drops it, so it is the event's only allocation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub(crate) name: Option<String>,
    pub(crate) data: String,
}

/// Frames a `text/event-stream` body into events.
///
/// `push` scans each chunk for line ends once, so the cost is one pass over
/// the bytes plus one `String` per dispatched event: a line that ends inside
/// the chunk is parsed in place from the chunk, and only the tail of a line
/// that a chunk boundary splits is buffered (`partial`). The event's `data`
/// accumulates in one reusable buffer across its lines; `event` and `id`
/// fields are recognized, other fields and comments are skipped per the SSE
/// specification. CRLF, CR, and LF line ends and a leading UTF-8 BOM are
/// accepted. The per-event size bound counts every non-terminator byte of
/// the event so an oversized event is refused before its terminator.
pub(crate) struct SseDecoder {
    /// Bytes of a line split by a chunk boundary, waiting for its end.
    partial: Vec<u8>,
    /// Joined `data:` lines of the event being assembled, `\n`-separated.
    data: Vec<u8>,
    event_name: Option<String>,
    /// Non-terminator bytes seen in the current event, for the size bound.
    event_bytes: usize,
    max_event_bytes: usize,
    /// The previous byte was `\r`: a following `\n` belongs to it.
    skip_line_feed: bool,
    /// The stream's first bytes have not yet been checked for a BOM.
    at_start: bool,
    mode: SseMode,
    size_overflow: &'static str,
    size_limit: &'static str,
    data_utf8: Utf8ErrorMessage,
}

impl SseDecoder {
    pub(crate) const fn data_only(
        max_event_bytes: usize,
        size_overflow: &'static str,
        size_limit: &'static str,
        data_utf8: Utf8ErrorMessage,
    ) -> Self {
        Self::new(
            max_event_bytes,
            SseMode::DataOnly,
            size_overflow,
            size_limit,
            data_utf8,
        )
    }

    pub(crate) const fn named(
        max_event_bytes: usize,
        size_overflow: &'static str,
        size_limit: &'static str,
        data_utf8: Utf8ErrorMessage,
        name_utf8: Utf8ErrorMessage,
    ) -> Self {
        Self::new(
            max_event_bytes,
            SseMode::Named(name_utf8),
            size_overflow,
            size_limit,
            data_utf8,
        )
    }

    const fn new(
        max_event_bytes: usize,
        mode: SseMode,
        size_overflow: &'static str,
        size_limit: &'static str,
        data_utf8: Utf8ErrorMessage,
    ) -> Self {
        Self {
            partial: Vec::new(),
            data: Vec::new(),
            event_name: None,
            event_bytes: 0,
            max_event_bytes,
            skip_line_feed: false,
            at_start: true,
            mode,
            size_overflow,
            size_limit,
            data_utf8,
        }
    }

    /// Frames every complete event in `bytes` (including one completed by
    /// bytes buffered from earlier chunks). The events are appended to
    /// `events` in stream order.
    pub(crate) fn push_into(
        &mut self,
        mut bytes: &[u8],
        events: &mut Vec<SseEvent>,
    ) -> Result<(), ProviderError> {
        if self.at_start {
            // The BOM may itself straddle chunks: hold back bytes while they
            // are a BOM prefix. Held bytes that turn out not to be a BOM are
            // ordinary line bytes and are re-fed below.
            let held = self.partial.len();
            let take = (3 - held).min(bytes.len());
            self.partial.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if b"\xef\xbb\xbf".starts_with(&self.partial) {
                if self.partial.len() < 3 {
                    return Ok(());
                }
                self.partial.clear();
                self.at_start = false;
            } else {
                self.at_start = false;
                let held = std::mem::take(&mut self.partial);
                self.push_into(&held, events)?;
            }
        }
        if self.skip_line_feed {
            self.skip_line_feed = false;
            if let Some((b'\n', rest)) = bytes.split_first() {
                bytes = rest;
            }
        }
        while !bytes.is_empty() {
            let Some(end) = bytes
                .iter()
                .position(|&byte| byte == b'\n' || byte == b'\r')
            else {
                self.count(bytes.len())?;
                self.partial.extend_from_slice(bytes);
                return Ok(());
            };
            self.count(end)?;
            let terminator = bytes[end];
            let line_tail = &bytes[..end];
            bytes = &bytes[end + 1..];
            if terminator == b'\r' {
                match bytes.split_first() {
                    Some((b'\n', rest)) => bytes = rest,
                    Some(_) => {}
                    None => self.skip_line_feed = true,
                }
            }
            if self.partial.is_empty() {
                self.line(line_tail, events)?;
            } else {
                // A line the previous chunk began: complete it in the
                // buffer, parse, and release the buffer for the next split.
                let mut line = std::mem::take(&mut self.partial);
                line.extend_from_slice(line_tail);
                let result = self.line(&line, events);
                line.clear();
                self.partial = line;
                result?;
            }
        }
        Ok(())
    }

    /// `push_into` collecting into a fresh vector.
    #[cfg(test)]
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, ProviderError> {
        let mut events = Vec::new();
        self.push_into(bytes, &mut events)?;
        Ok(events)
    }

    fn count(&mut self, bytes: usize) -> Result<(), ProviderError> {
        self.event_bytes = self
            .event_bytes
            .checked_add(bytes)
            .ok_or_else(|| ProviderError::Protocol(self.size_overflow.to_owned()))?;
        if self.event_bytes > self.max_event_bytes {
            return Err(ProviderError::Protocol(self.size_limit.to_owned()));
        }
        Ok(())
    }

    fn line(&mut self, line: &[u8], events: &mut Vec<SseEvent>) -> Result<(), ProviderError> {
        if line.is_empty() {
            self.event_bytes = 0;
            let name = self.event_name.take();
            if self.data.is_empty() {
                return Ok(());
            }
            self.data.pop();
            // Moving the buffer out hands its allocation to the event; the
            // event's `data` is the one allocation it costs.
            let data = String::from_utf8(std::mem::take(&mut self.data))
                .map_err(|error| self.data_utf8.into_provider_error(error))?;
            events.push(SseEvent { name, data });
            return Ok(());
        }
        if line[0] == b':' {
            return Ok(());
        }
        let (field, value) = match line.iter().position(|&byte| byte == b':') {
            Some(colon) => {
                let value = &line[colon + 1..];
                (&line[..colon], value.strip_prefix(b" ").unwrap_or(value))
            }
            None => (line, &[][..]),
        };
        match field {
            b"event" => {
                if let SseMode::Named(error_message) = self.mode {
                    self.event_name = Some(
                        String::from_utf8(value.to_vec())
                            .map_err(|error| error_message.into_provider_error(error))?,
                    );
                }
            }
            b"data" => {
                self.data.extend_from_slice(value);
                self.data.push(b'\n');
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OVERFLOW: &str = "event size overflowed";
    const LIMIT: &str = "event exceeded size limit";
    const DATA_UTF8: Utf8ErrorMessage = Utf8ErrorMessage::Static("data was not UTF-8");
    const NAME_UTF8: Utf8ErrorMessage = Utf8ErrorMessage::Static("name was not UTF-8");

    const SOURCE: &str = concat!(
        "\u{feff}: comment\r\n",
        "event: ignored\r",
        "data: first h",
        "é\n",
        "data: second\r\r",
        "event: named\n",
        "id: 7\n",
        "data: {\"a\":1}\n",
        "data\n",
        "\n",
        "retry: 10\n",
        ": trailing comment\n",
        "data:third\r\n\r\n",
    );

    fn expected(named: bool) -> Vec<SseEvent> {
        vec![
            SseEvent {
                name: named.then(|| "ignored".to_owned()),
                data: "first hé\nsecond".to_owned(),
            },
            SseEvent {
                name: named.then(|| "named".to_owned()),
                data: "{\"a\":1}\n".to_owned(),
            },
            SseEvent {
                name: None,
                data: "third".to_owned(),
            },
        ]
    }

    #[test]
    fn data_only_handles_fragmentation_bom_line_endings_comments_and_multiline_data() {
        let mut decoder = SseDecoder::data_only(1_024, OVERFLOW, LIMIT, DATA_UTF8);
        let mut events = Vec::new();
        for byte in SOURCE.as_bytes() {
            events.extend(decoder.push(std::slice::from_ref(byte)).unwrap());
        }
        assert_eq!(events, expected(false));
    }

    /// Every split point of the source into two chunks, and every chunk size
    /// from 1 to the whole body, frames identically to one push.
    #[test]
    fn framing_is_independent_of_chunk_boundaries() {
        let whole = SseDecoder::named(1_024, OVERFLOW, LIMIT, DATA_UTF8, NAME_UTF8)
            .push(SOURCE.as_bytes())
            .unwrap();
        assert_eq!(whole, expected(true));

        let bytes = SOURCE.as_bytes();
        for split in 0..=bytes.len() {
            let mut decoder = SseDecoder::named(1_024, OVERFLOW, LIMIT, DATA_UTF8, NAME_UTF8);
            let mut events = decoder.push(&bytes[..split]).unwrap();
            events.extend(decoder.push(&bytes[split..]).unwrap());
            assert_eq!(events, whole, "split at {split}");
        }
        for size in 1..=bytes.len() {
            let mut decoder = SseDecoder::named(1_024, OVERFLOW, LIMIT, DATA_UTF8, NAME_UTF8);
            let mut events = Vec::new();
            for chunk in bytes.chunks(size) {
                decoder.push_into(chunk, &mut events).unwrap();
            }
            assert_eq!(events, whole, "chunks of {size}");
        }
    }

    #[test]
    fn a_bom_split_across_chunks_is_still_skipped_and_a_non_bom_prefix_is_kept() {
        let mut decoder = SseDecoder::data_only(1_024, OVERFLOW, LIMIT, DATA_UTF8);
        assert!(decoder.push(b"\xef").unwrap().is_empty());
        assert!(decoder.push(b"\xbb").unwrap().is_empty());
        let events = decoder.push(b"\xbfdata: x\n\n").unwrap();
        assert_eq!(events[0].data, "x");

        let mut decoder = SseDecoder::data_only(1_024, OVERFLOW, LIMIT, DATA_UTF8);
        assert!(decoder.push(b"d").unwrap().is_empty());
        assert!(decoder.push(b"a").unwrap().is_empty());
        let events = decoder.push(b"ta: y\n\n").unwrap();
        assert_eq!(events[0].data, "y");

        // A BOM prefix that turns out not to be one: the held bytes are
        // line bytes. "\xef\xbbdata" is an unknown field, so that line is
        // skipped and nothing dispatches; the next event does.
        let mut decoder = SseDecoder::data_only(1_024, OVERFLOW, LIMIT, DATA_UTF8);
        assert!(decoder.push(b"\xef\xbb").unwrap().is_empty());
        assert!(decoder.push(b"data: z\n\n").unwrap().is_empty());
        assert_eq!(decoder.push(b"data: w\n\n").unwrap()[0].data, "w");
    }

    #[test]
    fn data_only_ignores_invalid_utf8_event_names() {
        let mut decoder = SseDecoder::data_only(1_024, OVERFLOW, LIMIT, DATA_UTF8);
        let events = decoder.push(b"event: \xff\ndata: valid\n\n").unwrap();
        assert_eq!(events[0].data, "valid");
        assert_eq!(events[0].name, None);
    }

    #[test]
    fn named_mode_captures_names_and_validates_their_utf8() {
        let mut decoder = SseDecoder::named(1_024, OVERFLOW, LIMIT, DATA_UTF8, NAME_UTF8);
        let events = decoder.push(b"event: message_stop\ndata: {}\n\n").unwrap();
        assert_eq!(events[0].name.as_deref(), Some("message_stop"));

        let error = decoder.push(b"event: \xff\ndata: {}\n\n").unwrap_err();
        assert_eq!(
            error.to_string(),
            "provider stream was invalid: name was not UTF-8"
        );
    }

    #[test]
    fn invalid_utf8_data_is_refused_when_the_event_dispatches() {
        let mut decoder = SseDecoder::data_only(1_024, OVERFLOW, LIMIT, DATA_UTF8);
        let error = decoder.push(b"data: \xff\n\n").unwrap_err();
        assert_eq!(
            error.to_string(),
            "provider stream was invalid: data was not UTF-8"
        );
    }

    #[test]
    fn incomplete_eof_does_not_dispatch_and_size_limit_is_enforced_before_termination() {
        let mut decoder = SseDecoder::data_only(7, OVERFLOW, LIMIT, DATA_UTF8);
        assert!(decoder.push(b"data: x").unwrap().is_empty());
        let error = decoder.push(b"y").unwrap_err();
        assert_eq!(
            error.to_string(),
            "provider stream was invalid: event exceeded size limit"
        );

        // The bound spans lines of one event and resets per event.
        let mut decoder = SseDecoder::data_only(12, OVERFLOW, LIMIT, DATA_UTF8);
        assert!(decoder.push(b"data: abc\ndat").unwrap().is_empty());
        assert!(
            decoder
                .push(b"a: d\n\n")
                .unwrap_err()
                .to_string()
                .contains(LIMIT)
        );
        let mut decoder = SseDecoder::data_only(12, OVERFLOW, LIMIT, DATA_UTF8);
        let events = decoder.push(b"data: abcdef\n\ndata: abcdef\n\n").unwrap();
        assert_eq!(events.len(), 2);
    }
}
