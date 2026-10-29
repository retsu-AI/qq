use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    ProviderError, ProviderErrorKind, ProviderUsage,
    http::{HttpRejection, is_retryable_status, note_attempts},
    sanitize::sanitize_message,
};

#[cfg(test)]
use crate::http::{build_client, build_direct_client};

#[derive(Debug, Clone, Copy)]
pub(crate) struct Text<'a>(pub(crate) &'a str);

impl serde::Serialize for Text<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut literal = String::with_capacity(self.0.len() + 2);
        write_json_string(&mut literal, self.0);
        serializer.serialize_newtype_struct(RAW_VALUE_TOKEN, &RawLiteral(&literal))
    }
}

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

#[derive(Debug, Clone)]
pub(crate) struct OwnedOrBorrowedText<'a>(pub(crate) std::borrow::Cow<'a, str>);

impl serde::Serialize for OwnedOrBorrowedText<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Text(&self.0).serialize(serializer)
    }
}

pub(crate) fn write_json_string(out: &mut String, value: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push('"');
    let bytes = value.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        let run = clean_run_len(&bytes[start..]);
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

#[inline]
fn clean_run_len(bytes: &[u8]) -> usize {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGHS: u64 = 0x8080_8080_8080_8080;
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

    let status = status.as_u16();
    if status == 413 || (status == 400 && names_context_overflow(&message)) {
        return ProviderError::ResponseFailed {
            kind: ProviderErrorKind::ContextExceeded,
            message,
        };
    }

    let error = match rejection.retry_after() {
        Some(retry_after) if is_retryable_status(rejection.status()) => ProviderError::RetryAfter {
            status,
            message,
            retry_after,
        },
        _ => ProviderError::Api { status, message },
    };
    note_attempts(error, rejection.attempts())
}

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

pub(crate) fn names_transient_failure(message: &str) -> bool {
    let lowered = message.to_lowercase();
    lowered.contains("overloaded") || lowered.contains("internal error")
}

pub(crate) fn status_error_kind(status: u16) -> ProviderErrorKind {
    match status {
        400 | 404 | 409 | 422 => ProviderErrorKind::InvalidRequest,
        401 | 403 => ProviderErrorKind::Authentication,
        429 => ProviderErrorKind::RateLimited,
        500..=599 => ProviderErrorKind::Unavailable,
        _ => ProviderErrorKind::Response,
    }
}

pub(crate) fn value_as_status(value: &Value) -> Option<u16> {
    value
        .as_u64()
        .and_then(|status| u16::try_from(status).ok())
        .or_else(|| value.as_str()?.parse().ok())
}

pub(crate) struct ToolCallLedger<K> {
    calls: BTreeMap<K, String>,
    reused_key: &'static str,
    unknown_key: &'static str,
}

impl<K: Ord> ToolCallLedger<K> {
    pub(crate) fn new(reused_key: &'static str, unknown_key: &'static str) -> Self {
        Self {
            calls: BTreeMap::new(),
            reused_key,
            unknown_key,
        }
    }

    pub(crate) fn insert(&mut self, key: K, id: String) -> Result<(), ProviderError> {
        if self.calls.insert(key, id).is_some() {
            return Err(ProviderError::Protocol(self.reused_key.to_owned()));
        }
        Ok(())
    }

    pub(crate) fn get(&self, key: &K) -> Result<&str, ProviderError> {
        self.calls
            .get(key)
            .map(String::as_str)
            .ok_or_else(|| ProviderError::Protocol(self.unknown_key.to_owned()))
    }

    pub(crate) fn remove(&mut self, key: &K) -> Option<String> {
        self.calls.remove(key)
    }

    pub(crate) fn drain(&mut self) -> impl Iterator<Item = String> {
        std::mem::take(&mut self.calls).into_values()
    }
}

pub(crate) struct UsageOnce {
    usage: Option<ProviderUsage>,
    duplicate: &'static str,
}

impl UsageOnce {
    pub(crate) fn new(duplicate: &'static str) -> Self {
        Self {
            usage: None,
            duplicate,
        }
    }

    pub(crate) fn set(&mut self, usage: ProviderUsage) -> Result<(), ProviderError> {
        if self.usage.replace(usage).is_some() {
            return Err(ProviderError::Protocol(self.duplicate.to_owned()));
        }
        Ok(())
    }

    pub(crate) fn stored_mut(&mut self) -> Option<&mut ProviderUsage> {
        self.usage.as_mut()
    }

    pub(crate) fn finish(self) -> Option<ProviderUsage> {
        self.usage
    }
}

pub(crate) fn subtract_cached_input_tokens(
    total: u64,
    cached: u64,
    underflow: &'static str,
) -> Result<u64, ProviderError> {
    total
        .checked_sub(cached)
        .ok_or_else(|| ProviderError::Protocol(underflow.to_owned()))
}

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
