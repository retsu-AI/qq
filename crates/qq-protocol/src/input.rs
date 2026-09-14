//! Structured run input and opaque correlation metadata.
//!
//! A prompt is a bounded list of typed parts rather than one string. Text
//! parts are carried verbatim; workspace file parts name a capability-scoped
//! path whose bytes the runtime reads when the run starts, so admission never
//! performs I/O and a stale or oversized attachment fails the run before its
//! first provider request rather than the command that queued it.

use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Deserializer, Serialize, de};
use thiserror::Error;

use crate::ContentHash;

/// Most parts one prompt or steering message may carry.
pub const MAX_INPUT_PARTS: usize = 32;
/// Total bytes of `Text` parts per prompt or steering message.
pub const MAX_INPUT_TEXT_BYTES: usize = 128 * 1024;
/// Most `WorkspaceFile` parts per prompt or steering message.
pub const MAX_INPUT_FILE_PARTS: usize = 8;
/// Longest workspace-relative path a file part may name.
pub const MAX_INPUT_PATH_BYTES: usize = 4096;
/// Largest single file the runtime will attach when resolving a part.
pub const MAX_INPUT_FILE_BYTES: usize = 256 * 1024;
/// Largest resolved input (text plus every attached file) per message.
pub const MAX_RESOLVED_INPUT_BYTES: usize = 1024 * 1024;

/// One typed unit of user input.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputPart {
    Text {
        text: String,
    },
    /// A file inside the session's workspace, attached by reference. The
    /// runtime reads it through the workspace capability when the run starts;
    /// when `expected_hash` is present the bytes must still hash to it.
    /// `range` (1-based, inclusive, `start <= end`) attaches only those
    /// lines; the whole file is still hashed and recorded so an edit needs
    /// no redundant read.
    WorkspaceFile {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_hash: Option<ContentHash>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        range: Option<LineRange>,
    },
}

/// An inclusive 1-based line window of an attached file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

impl InputPart {
    /// A single text part.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// A whole-file attachment without a hash.
    #[must_use]
    pub fn workspace_file(path: impl Into<String>) -> Self {
        Self::WorkspaceFile {
            path: path.into(),
            expected_hash: None,
            range: None,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> InputPartKind {
        match self {
            Self::Text { .. } => InputPartKind::Text,
            Self::WorkspaceFile { .. } => InputPartKind::WorkspaceFile,
        }
    }
}

/// The variant vocabulary of [`InputPart`], advertised by server capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputPartKind {
    Text,
    WorkspaceFile,
}

impl InputPartKind {
    /// Every kind this protocol revision defines, in declaration order.
    pub const ALL: [Self; 2] = [Self::Text, Self::WorkspaceFile];
}

/// Why a list of input parts was rejected before admission. Every variant is
/// decidable from the parts alone; none requires filesystem or provider
/// access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum InputError {
    #[error("input must carry at least one part")]
    Empty,
    #[error("input may carry at most {MAX_INPUT_PARTS} parts, got {count}")]
    TooManyParts { count: usize },
    #[error("input text totals {bytes} bytes; the limit is {MAX_INPUT_TEXT_BYTES}")]
    TextTooLarge { bytes: usize },
    #[error("input text is empty or whitespace and no file is attached")]
    Blank,
    #[error("input may attach at most {MAX_INPUT_FILE_PARTS} workspace files, got {count}")]
    TooManyFiles { count: usize },
    #[error("workspace file part {index} has an empty path")]
    EmptyPath { index: usize },
    #[error("workspace file part {index} path exceeds {MAX_INPUT_PATH_BYTES} bytes")]
    PathTooLong { index: usize },
    #[error("workspace file part {index} path contains a NUL byte")]
    PathHasNul { index: usize },
    #[error("workspace file part {index} path is absolute; paths are workspace-relative")]
    AbsolutePath { index: usize },
    #[error(
        "workspace file part {index} range is empty or inverted; lines are 1-based and start <= end"
    )]
    InvalidRange { index: usize },
}

/// Checks the syntactic bounds shared by prompts and steering input.
pub fn validate_input(parts: &[InputPart]) -> Result<(), InputError> {
    if parts.is_empty() {
        return Err(InputError::Empty);
    }
    if parts.len() > MAX_INPUT_PARTS {
        return Err(InputError::TooManyParts { count: parts.len() });
    }
    let mut text_bytes = 0_usize;
    let mut has_visible_text = false;
    let mut files = 0_usize;
    for (index, part) in parts.iter().enumerate() {
        match part {
            InputPart::Text { text } => {
                text_bytes = text_bytes.saturating_add(text.len());
                has_visible_text |= !text.trim().is_empty();
            }
            InputPart::WorkspaceFile { path, range, .. } => {
                files += 1;
                if path.is_empty() {
                    return Err(InputError::EmptyPath { index });
                }
                if path.len() > MAX_INPUT_PATH_BYTES {
                    return Err(InputError::PathTooLong { index });
                }
                if path.as_bytes().contains(&0) {
                    return Err(InputError::PathHasNul { index });
                }
                if path.starts_with('/') || path.starts_with('\\') {
                    return Err(InputError::AbsolutePath { index });
                }
                if let Some(range) = range
                    && (range.start == 0 || range.end < range.start)
                {
                    return Err(InputError::InvalidRange { index });
                }
            }
        }
    }
    if text_bytes > MAX_INPUT_TEXT_BYTES {
        return Err(InputError::TextTooLarge { bytes: text_bytes });
    }
    if files > MAX_INPUT_FILE_PARTS {
        return Err(InputError::TooManyFiles { count: files });
    }
    if !has_visible_text && files == 0 {
        return Err(InputError::Blank);
    }
    Ok(())
}

/// Most entries one correlation map may carry.
pub const MAX_CORRELATION_ENTRIES: usize = 8;
/// Longest correlation key in bytes.
pub const MAX_CORRELATION_KEY_BYTES: usize = 64;
/// Longest correlation value in bytes.
pub const MAX_CORRELATION_VALUE_BYTES: usize = 256;
/// Total key plus value bytes across one correlation map.
pub const MAX_CORRELATION_BYTES: usize = 2048;

/// Opaque caller-owned attribution for a session or run: a gateway's user,
/// channel, thread, request, or job identifiers. QQ stores and echoes it; it
/// never interprets the keys, derives identity from them, or treats them as
/// authorization. Bounded so it can ride every summary without growing the
/// event stream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Correlation(BTreeMap<String, String>);

impl Correlation {
    /// Validates the bounds and wraps the map.
    pub fn new(entries: BTreeMap<String, String>) -> Result<Self, CorrelationError> {
        if entries.len() > MAX_CORRELATION_ENTRIES {
            return Err(CorrelationError::TooManyEntries {
                count: entries.len(),
            });
        }
        let mut total = 0_usize;
        for (key, value) in &entries {
            if key.is_empty() {
                return Err(CorrelationError::EmptyKey);
            }
            if key.len() > MAX_CORRELATION_KEY_BYTES {
                return Err(CorrelationError::KeyTooLong { key: key.clone() });
            }
            if value.len() > MAX_CORRELATION_VALUE_BYTES {
                return Err(CorrelationError::ValueTooLong { key: key.clone() });
            }
            total = total.saturating_add(key.len()).saturating_add(value.len());
        }
        if total > MAX_CORRELATION_BYTES {
            return Err(CorrelationError::TooLarge { bytes: total });
        }
        Ok(Self(entries))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }
}

impl<'de> Deserialize<'de> for Correlation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let entries = BTreeMap::<String, String>::deserialize(deserializer)?;
        Self::new(entries).map_err(de::Error::custom)
    }
}

impl fmt::Display for Correlation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for (key, value) in &self.0 {
            if !first {
                formatter.write_str(",")?;
            }
            first = false;
            write!(formatter, "{key}={value}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CorrelationError {
    #[error("correlation may carry at most {MAX_CORRELATION_ENTRIES} entries, got {count}")]
    TooManyEntries { count: usize },
    #[error("correlation keys must not be empty")]
    EmptyKey,
    #[error("correlation key {key:?} exceeds {MAX_CORRELATION_KEY_BYTES} bytes")]
    KeyTooLong { key: String },
    #[error("correlation value for {key:?} exceeds {MAX_CORRELATION_VALUE_BYTES} bytes")]
    ValueTooLong { key: String },
    #[error("correlation totals {bytes} bytes; the limit is {MAX_CORRELATION_BYTES}")]
    TooLarge { bytes: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str) -> InputPart {
        InputPart::WorkspaceFile {
            path: path.to_owned(),
            expected_hash: None,
            range: None,
        }
    }

    #[test]
    fn input_parts_use_tagged_snake_case_encoding() {
        let parts = vec![
            InputPart::text("Review this"),
            InputPart::WorkspaceFile {
                path: "src/lib.rs".to_owned(),
                expected_hash: Some(ContentHash::from_bytes([0x11; 32])),
                range: None,
            },
        ];
        let json = serde_json::to_string(&parts).unwrap();
        assert_eq!(
            json,
            format!(
                "[{{\"type\":\"text\",\"text\":\"Review this\"}},{{\"type\":\"workspace_file\",\"path\":\"src/lib.rs\",\"expected_hash\":\"{}\"}}]",
                "11".repeat(32)
            )
        );
        assert_eq!(
            serde_json::from_str::<Vec<InputPart>>(&json).unwrap(),
            parts
        );
        assert!(serde_json::from_str::<InputPart>(r#"{"type":"image","url":"x"}"#).is_err());
        assert!(
            serde_json::from_str::<InputPart>(r#"{"type":"text","text":"x","extra":1}"#).is_err()
        );
        assert_eq!(
            InputPartKind::ALL.map(|kind| serde_json::to_string(&kind).unwrap()),
            ["\"text\"", "\"workspace_file\""]
        );
    }

    #[test]
    fn input_validation_covers_every_bound() {
        assert_eq!(validate_input(&[]), Err(InputError::Empty));
        let many = vec![InputPart::text("x"); MAX_INPUT_PARTS + 1];
        assert_eq!(
            validate_input(&many),
            Err(InputError::TooManyParts {
                count: MAX_INPUT_PARTS + 1
            })
        );
        let big = InputPart::text("a".repeat(MAX_INPUT_TEXT_BYTES + 1));
        assert_eq!(
            validate_input(&[big]),
            Err(InputError::TextTooLarge {
                bytes: MAX_INPUT_TEXT_BYTES + 1
            })
        );
        assert_eq!(
            validate_input(&[InputPart::text("  \n")]),
            Err(InputError::Blank)
        );
        assert_eq!(
            validate_input(&[InputPart::text(" \n"), file("a.rs")]),
            Ok(())
        );
        let files: Vec<_> = (0..=MAX_INPUT_FILE_PARTS)
            .map(|index| file(&format!("f{index}")))
            .collect();
        assert_eq!(
            validate_input(&files),
            Err(InputError::TooManyFiles {
                count: MAX_INPUT_FILE_PARTS + 1
            })
        );
        assert_eq!(
            validate_input(&[file("")]),
            Err(InputError::EmptyPath { index: 0 })
        );
        assert_eq!(
            validate_input(&[
                InputPart::text("x"),
                file(&"p".repeat(MAX_INPUT_PATH_BYTES + 1))
            ]),
            Err(InputError::PathTooLong { index: 1 })
        );
        assert_eq!(
            validate_input(&[file("a\0b")]),
            Err(InputError::PathHasNul { index: 0 })
        );
        assert_eq!(
            validate_input(&[file("/etc/passwd")]),
            Err(InputError::AbsolutePath { index: 0 })
        );
        assert_eq!(
            validate_input(&[InputPart::text("hi"), file("src/main.rs")]),
            Ok(())
        );
    }

    #[test]
    fn correlation_is_a_bounded_transparent_map() {
        let correlation = Correlation::new(BTreeMap::from([
            ("thread".to_owned(), "t-1".to_owned()),
            ("channel".to_owned(), "c-9".to_owned()),
        ]))
        .unwrap();
        let json = serde_json::to_string(&correlation).unwrap();
        assert_eq!(json, r#"{"channel":"c-9","thread":"t-1"}"#);
        assert_eq!(
            serde_json::from_str::<Correlation>(&json).unwrap(),
            correlation
        );
        assert_eq!(correlation.to_string(), "channel=c-9,thread=t-1");
        assert_eq!(correlation.get("thread"), Some("t-1"));
        assert!(Correlation::default().is_empty());

        let too_many: BTreeMap<_, _> = (0..=MAX_CORRELATION_ENTRIES)
            .map(|index| (format!("k{index}"), String::new()))
            .collect();
        assert!(matches!(
            Correlation::new(too_many),
            Err(CorrelationError::TooManyEntries { .. })
        ));
        assert_eq!(
            Correlation::new(BTreeMap::from([(String::new(), "v".to_owned())])),
            Err(CorrelationError::EmptyKey)
        );
        assert!(matches!(
            Correlation::new(BTreeMap::from([(
                "k".repeat(MAX_CORRELATION_KEY_BYTES + 1),
                String::new()
            )])),
            Err(CorrelationError::KeyTooLong { .. })
        ));
        assert!(matches!(
            Correlation::new(BTreeMap::from([(
                "k".to_owned(),
                "v".repeat(MAX_CORRELATION_VALUE_BYTES + 1)
            )])),
            Err(CorrelationError::ValueTooLong { .. })
        ));
        let large: BTreeMap<_, _> = (0..MAX_CORRELATION_ENTRIES)
            .map(|index| {
                (
                    format!("key-{index}"),
                    "v".repeat(MAX_CORRELATION_VALUE_BYTES),
                )
            })
            .collect();
        assert!(matches!(
            Correlation::new(large),
            Err(CorrelationError::TooLarge { .. })
        ));
        let oversize_json = format!(
            r#"{{"k":"{}"}}"#,
            "v".repeat(MAX_CORRELATION_VALUE_BYTES + 1)
        );
        assert!(serde_json::from_str::<Correlation>(&oversize_json).is_err());
    }
}
