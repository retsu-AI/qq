//! Protocol-side support kit shared by the HTTP protocol adapters.
//!
//! Everything here passes the deletion test: it replaces implementation that
//! was repeated in at least two adapter files. Wire schemas, stream state
//! machines, and protocol-owned headers stay in the adapters.

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    ProviderError, ProviderErrorKind, ProviderUsage,
    http::{HttpRejection, note_attempts},
    sanitize::sanitize_message,
};

#[cfg(test)]
use crate::http::{build_client, build_direct_client};

/// Interprets a rejected HTTP exchange against the adapter's error envelope.
///
/// The message extracted from the decoded envelope — or, failing that, the
/// non-empty body text — is sanitized against the rejection's redactions;
/// `fallback` names the protocol for statuses without a canonical reason.
/// A borrowed transcript string that writes its own JSON escaping.
///
/// The transcript is megabytes of text and its escape loop is the body
/// encoder's hot path. `serde_json`'s loop is a per-byte table walk whose
/// speed swung 1.5x between otherwise identical builds of this crate (same
/// instruction count, different loop alignment), so the request codecs do
/// not depend on it: this type scans for the next byte that needs escaping
/// eight bytes at a time, writes the clean run in one copy, and writes the
/// escape itself. The finished literal reaches `serde_json` through its
/// raw-value hook, which is a plain `write_all`. Output is byte-identical
/// to `serde_json`'s (a test pins every byte value at every lane).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Text<'a>(pub(crate) &'a str);

impl serde::Serialize for Text<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut literal = String::with_capacity(self.0.len() + 2);
        write_json_string(&mut literal, self.0);
        // The literal is one complete JSON string token, which is the
        // invariant the raw-value hook requires of what it writes verbatim.
        serializer.serialize_newtype_struct(RAW_VALUE_TOKEN, &RawLiteral(&literal))
    }
}

/// `serde_json::raw::TOKEN`: a struct so named serializes its one field's
/// text verbatim. Stable since `RawValue` shipped; the unit test fails if it
/// is ever renamed, because the literal would then be written as an escaped
/// string.
const RAW_VALUE_TOKEN: &str = "$serde_json::private::RawValue";

struct RawLiteral<'a>(&'a str);

impl serde::Serialize for RawLiteral<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct as _;
        let mut raw = serializer.serialize_struct(RAW_VALUE_TOKEN, 1)?;
        raw.serialize_field(RAW_VALUE_TOKEN, self.0)?;
        raw.end()
    }
}

/// [`Text`] over a string a codec had to assemble (concatenated blocks).
#[derive(Debug, Clone)]
pub(crate) struct OwnedOrBorrowedText<'a>(pub(crate) std::borrow::Cow<'a, str>);

impl serde::Serialize for OwnedOrBorrowedText<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Text(&self.0).serialize(serializer)
    }
}

/// Appends `value` to `out` as a JSON string literal, byte-identical to
/// `serde_json::to_string(value)`.
pub(crate) fn write_json_string(out: &mut String, value: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push('"');
    let bytes = value.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        let run = clean_run_len(&bytes[start..]);
        // Splitting only at ASCII bytes keeps every run valid UTF-8.
        out.push_str(&value[start..start + run]);
        start += run;
        let Some(&byte) = bytes.get(start) else {
            break;
        };
        match byte {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x08 => out.push_str("\\b"),
            0x09 => out.push_str("\\t"),
            0x0A => out.push_str("\\n"),
            0x0C => out.push_str("\\f"),
            0x0D => out.push_str("\\r"),
            control => {
                out.push_str("\\u00");
                out.push(char::from(HEX[usize::from(control >> 4)]));
                out.push(char::from(HEX[usize::from(control & 0x0F)]));
            }
        }
        start += 1;
    }
    out.push('"');
}

/// Length of the prefix of `bytes` containing no quote, backslash, or
/// control byte: eight bytes per step with word-parallel tests, then a byte
/// tail.
#[inline]
fn clean_run_len(bytes: &[u8]) -> usize {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGHS: u64 = 0x8080_8080_8080_8080;
    // With every lane's high bit cleared, `lane - n` borrows into that high
    // bit exactly when the lane is below `n`; XOR with a repeated byte then
    // `- 1` does the same for equality. Lanes that had their high bit set
    // (UTF-8 lead and continuation bytes) are masked back out.
    let needs_escape = |word: u64| -> bool {
        let low = word & !HIGHS;
        let below_space = low.wrapping_sub(ONES * 0x20);
        let is_quote = (low ^ (ONES * u64::from(b'"'))).wrapping_sub(ONES);
        let is_backslash = (low ^ (ONES * u64::from(b'\\'))).wrapping_sub(ONES);
        ((below_space | is_quote | is_backslash) & !word & HIGHS) != 0
    };
    let mut index = 0;
    while let Some(chunk) = bytes.get(index..index + 8) {
        let word = u64::from_le_bytes(chunk.try_into().expect("eight bytes"));
        if needs_escape(word) {
            break;
        }
        index += 8;
    }
    while let Some(&byte) = bytes.get(index) {
        if byte < 0x20 || byte == b'"' || byte == b'\\' {
            break;
        }
        index += 1;
    }
    index
}

pub(crate) fn api_error<E: DeserializeOwned>(
    rejection: HttpRejection,
    fallback: &str,
    message: impl FnOnce(E) -> Option<String>,
) -> ProviderError {
    let status = rejection.status();
    let fallback = status.canonical_reason().unwrap_or(fallback).to_owned();
    let body_text = String::from_utf8_lossy(rejection.body());
    let message = serde_json::from_slice::<E>(rejection.body())
        .ok()
        .and_then(message)
        .or_else(|| (!body_text.trim().is_empty()).then(|| body_text.into_owned()))
        .map_or(fallback, |message| {
            sanitize_message(&message, rejection.redactions())
        });

    // A rejected request that names the context window is recoverable by
    // compaction; surface it as such rather than as a generic API failure.
    let status = status.as_u16();
    if status == 413 || (status == 400 && names_context_overflow(&message)) {
        return ProviderError::ResponseFailed {
            kind: ProviderErrorKind::ContextExceeded,
            message,
        };
    }

    note_attempts(ProviderError::Api { status, message }, rejection.attempts())
}

/// Message-text detection of context-window overflow for providers that
/// report it only as a generic 400. The phrases cover OpenAI-compatible,
/// Anthropic-compatible, and Google wire messages observed in practice.
pub(crate) fn names_context_overflow(message: &str) -> bool {
    let lowered = message.to_lowercase();
    lowered.contains("context window")
        || lowered.contains("context length")
        || lowered.contains("context_length")
        || lowered.contains("too many tokens")
        || lowered.contains("maximum number of tokens")
        || lowered.contains("input is too long")
        || lowered.contains("prompt is too long")
        || lowered.contains("exceeds the maximum")
}

/// Maps an HTTP-shaped status embedded in an error payload to an error kind.
pub(crate) fn status_error_kind(status: u16) -> ProviderErrorKind {
    match status {
        400 | 404 | 409 | 422 => ProviderErrorKind::InvalidRequest,
        401 | 403 => ProviderErrorKind::Authentication,
        429 => ProviderErrorKind::RateLimited,
        500..=599 => ProviderErrorKind::Unavailable,
        _ => ProviderErrorKind::Response,
    }
}

/// Reads a status carried as either a JSON number or a numeric string.
pub(crate) fn value_as_status(value: &Value) -> Option<u16> {
    value
        .as_u64()
        .and_then(|status| u16::try_from(status).ok())
        .or_else(|| value.as_str()?.parse().ok())
}

/// Attributes streamed tool-call fragments and stops to their call ids.
///
/// Keyed by whatever the protocol streams — a function-call item id, a
/// tool-call array index, or a content-block index. Backed by an ordered map
/// so [`ToolCallLedger::drain`] completes calls in key order (Chat
/// Completions drains open calls when the choice finishes with `tool_calls`).
pub(crate) struct ToolCallLedger<K> {
    calls: BTreeMap<K, String>,
    reused_key: &'static str,
    unknown_key: &'static str,
}

impl<K: Ord> ToolCallLedger<K> {
    /// Creates an empty ledger with the protocol's attribution errors.
    pub(crate) fn new(reused_key: &'static str, unknown_key: &'static str) -> Self {
        Self {
            calls: BTreeMap::new(),
            reused_key,
            unknown_key,
        }
    }

    /// Records a started call, rejecting a key the stream already used.
    pub(crate) fn insert(&mut self, key: K, id: String) -> Result<(), ProviderError> {
        if self.calls.insert(key, id).is_some() {
            return Err(ProviderError::Protocol(self.reused_key.to_owned()));
        }
        Ok(())
    }

    /// Resolves the call id an argument fragment belongs to.
    pub(crate) fn get(&self, key: &K) -> Result<&str, ProviderError> {
        self.calls
            .get(key)
            .map(String::as_str)
            .ok_or_else(|| ProviderError::Protocol(self.unknown_key.to_owned()))
    }

    /// Closes the call the stopped key started, if any.
    pub(crate) fn remove(&mut self, key: &K) -> Option<String> {
        self.calls.remove(key)
    }

    /// Completes every open call in key order.
    pub(crate) fn drain(&mut self) -> impl Iterator<Item = String> {
        std::mem::take(&mut self.calls).into_values()
    }
}

/// A set-once slot for the usage a stream reports.
pub(crate) struct UsageOnce {
    usage: Option<ProviderUsage>,
    duplicate: &'static str,
}

impl UsageOnce {
    /// Creates an empty slot with the protocol's duplicate-report error.
    pub(crate) fn new(duplicate: &'static str) -> Self {
        Self {
            usage: None,
            duplicate,
        }
    }

    /// Stores the reported usage, rejecting a second report.
    pub(crate) fn set(&mut self, usage: ProviderUsage) -> Result<(), ProviderError> {
        if self.usage.replace(usage).is_some() {
            return Err(ProviderError::Protocol(self.duplicate.to_owned()));
        }
        Ok(())
    }

    /// The stored usage, for protocols that update it cumulatively.
    pub(crate) fn stored_mut(&mut self) -> Option<&mut ProviderUsage> {
        self.usage.as_mut()
    }

    /// Consumes the slot when the stream completes.
    pub(crate) fn finish(self) -> Option<ProviderUsage> {
        self.usage
    }
}

/// Subtracts provider-reported cached tokens from the total input tokens.
///
/// Providers report cache reads inside the prompt/input total; the neutral
/// model carries them separately, so an underflowing report is a protocol
/// error rather than a silent wrap.
pub(crate) fn subtract_cached_input_tokens(
    total: u64,
    cached: u64,
    underflow: &'static str,
) -> Result<u64, ProviderError> {
    total
        .checked_sub(cached)
        .ok_or_else(|| ProviderError::Protocol(underflow.to_owned()))
}

/// Builds the HTTP client an adapter's exact-endpoint constructor should use:
/// loopback plain-HTTP endpoints get the proxy-free direct client, everything
/// else the standard client.
#[cfg(test)]
pub(crate) fn client_for_endpoint(
    endpoint: &reqwest::Url,
) -> Result<reqwest::Client, ProviderError> {
    if endpoint.scheme() == "http" {
        build_direct_client()
    } else {
        build_client()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_escapes_exactly_as_serde_json_does() {
        let mut sample = String::from("plain start ");
        for byte in 0u8..0x20 {
            sample.push(char::from(byte));
            sample.push_str(" mid ");
        }
        sample.push_str("quote\" backslash\\ slash/ é 漢字 🎉 \u{7f} end");
        for value in [
            sample.as_str(),
            "",
            "\"",
            "\\",
            "\n",
            "no escapes at all",
            "é",
        ] {
            let expected = serde_json::to_string(value).unwrap();
            let mut literal = String::new();
            write_json_string(&mut literal, value);
            assert_eq!(literal, expected, "{value:?}");
            assert_eq!(serde_json::to_string(&Text(value)).unwrap(), expected);
            assert_eq!(
                serde_json::to_string(&OwnedOrBorrowedText(std::borrow::Cow::Owned(
                    value.to_owned()
                )))
                .unwrap(),
                expected
            );
        }
        // Every byte value at every lane of a word agrees with the naive scan.
        for lane in 0..8 {
            for byte in 0u8..=0xff {
                let mut sample = vec![b'a'; 16];
                sample[lane] = byte;
                sample[15] = b'"';
                let naive = sample
                    .iter()
                    .position(|&b| b < 0x20 || b == b'"' || b == b'\\')
                    .unwrap();
                assert_eq!(
                    clean_run_len(&sample),
                    naive,
                    "byte {byte:#x} at lane {lane}"
                );
            }
        }
        let clean = "é漢字🎉 no escapes";
        assert_eq!(clean_run_len(clean.as_bytes()), clean.len());
        #[derive(serde::Serialize)]
        struct Wire<'a> {
            content: Text<'a>,
            parts: Vec<Text<'a>>,
        }
        assert_eq!(
            serde_json::to_string(&Wire {
                content: Text("a\"b"),
                parts: vec![Text("x"), Text("\n")],
            })
            .unwrap(),
            r#"{"content":"a\"b","parts":["x","\n"]}"#
        );
    }
}
