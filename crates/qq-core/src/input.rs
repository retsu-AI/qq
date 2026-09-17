//! Structured input resolution.
//!
//! Admission validates input parts syntactically (see
//! [`qq_protocol::validate_input`]); this module turns them into one user
//! message when the run starts. Text parts are concatenated verbatim.
//! Workspace file parts are read through the run's workspace capability,
//! bounded, optionally hash-checked, and rendered as fenced attachments after
//! the text. Each attached file is recorded in the session's file state so a
//! later edit satisfies the read-before-write rule without a redundant read.
//!
//! The bytes the model saw are also returned as [`ResolvedAttachment`]s so
//! the run can persist them when it starts. A later run reconstructs the
//! prompt from those stored bytes with [`render_file_attachment`], never from
//! the current file, so the assembled context stays byte-identical to the
//! request the model actually saw even after the file changes or disappears.

use std::sync::Arc;

use qq_protocol::{
    InputPart, MAX_INPUT_FILE_BYTES, MAX_RESOLVED_INPUT_BYTES, RunFailureKind, validate_input,
};
use thiserror::Error;

use crate::workspace::{FileState, Workspace, content_hash};

/// Why input parts could not become a message. Every variant fails the run
/// before its first provider request as `RunFailureKind::InvalidCommand`.
#[derive(Debug, Error)]
pub(crate) enum InputResolutionError {
    #[error("{0}")]
    Invalid(#[from] qq_protocol::InputError),
    #[error("workspace file {path:?}: {message}")]
    Path { path: String, message: String },
    #[error("workspace file {path:?} is not a regular file")]
    NotAFile { path: String },
    #[error("workspace file {path:?} could not be read: {message}")]
    Read { path: String, message: String },
    #[error("workspace file {path:?} exceeds {MAX_INPUT_FILE_BYTES} bytes")]
    FileTooLarge { path: String },
    #[error("workspace file {path:?} is not valid UTF-8")]
    NotUtf8 { path: String },
    #[error("workspace file {path:?} changed: expected content hash {expected}, found {actual}")]
    HashMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("workspace file {path:?} range starts at line {start} but the file has {lines} lines")]
    RangeOutOfBounds {
        path: String,
        start: u32,
        lines: usize,
    },
    #[error("resolved input exceeds {MAX_RESOLVED_INPUT_BYTES} bytes")]
    TooLarge,
}

impl InputResolutionError {
    pub(crate) const fn failure_kind(&self) -> RunFailureKind {
        RunFailureKind::InvalidCommand
    }
}

/// The text a list of parts renders to without touching the filesystem: text
/// parts verbatim, file parts as placeholders naming the path. Used for
/// titles, transcript rows, and history search, where attachment bytes do
/// not belong.
pub(crate) fn render_text(parts: &[InputPart]) -> String {
    let mut text = String::new();
    for part in parts {
        match part {
            InputPart::Text { text: chunk } => text.push_str(chunk),
            InputPart::WorkspaceFile { path, .. } => {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push('@');
                text.push_str(path);
                text.push('\n');
            }
        }
    }
    text
}

/// One file the model saw in a prompt: the contained path as rendered, the
/// whole-file content hash, the attached window (the whole file when there is
/// no range), and its bytes. `window` is `(start, end, total)` in lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedAttachment {
    pub(crate) path: String,
    pub(crate) digest: String,
    pub(crate) window: Option<(usize, usize, usize)>,
    pub(crate) content: String,
}

/// The provider-visible message text plus the attachments it embeds, in
/// prompt order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedInput {
    pub(crate) text: String,
    pub(crate) attachments: Vec<ResolvedAttachment>,
}

/// The text parts of a prompt concatenated verbatim: the prefix of the
/// provider-visible text before any attachment block.
pub(crate) fn render_text_parts(parts: &[InputPart]) -> String {
    let mut text = String::new();
    for part in parts {
        if let InputPart::Text { text: chunk } = part {
            text.push_str(chunk);
        }
    }
    text
}

/// Reads every file part and renders the provider-visible message text.
/// Blocking: call from `spawn_blocking` or a dedicated thread.
pub(crate) fn resolve_blocking(
    parts: &[InputPart],
    workspace: &Workspace,
    file_state: &Arc<FileState>,
) -> Result<ResolvedInput, InputResolutionError> {
    validate_input(parts)?;
    let mut text = String::new();
    let mut attachments = String::new();
    let mut resolved = Vec::new();
    for part in parts {
        match part {
            InputPart::Text { text: chunk } => text.push_str(chunk),
            InputPart::WorkspaceFile {
                path,
                expected_hash,
                range,
            } => {
                let contained = match workspace.contained_path(path) {
                    Ok(contained) => contained,
                    Err(error) => {
                        return Err(InputResolutionError::Path {
                            path: path.clone(),
                            message: error.to_string(),
                        });
                    }
                };
                if !workspace.root().is_file(&contained) {
                    return Err(InputResolutionError::NotAFile { path: path.clone() });
                }
                let file = match workspace.root().open(&contained) {
                    Ok(file) => file,
                    Err(error) => {
                        return Err(InputResolutionError::Read {
                            path: path.clone(),
                            message: error.to_string(),
                        });
                    }
                };
                // No metadata was read here before; a zero hint keeps that
                // path to one open and one bounded read.
                let bytes = match crate::workspace::read_bounded(file, MAX_INPUT_FILE_BYTES, 0) {
                    Ok(bytes) => bytes,
                    Err(crate::workspace::BoundedReadError::TooLarge { .. }) => {
                        return Err(InputResolutionError::FileTooLarge { path: path.clone() });
                    }
                    Err(crate::workspace::BoundedReadError::Io(error)) => {
                        return Err(InputResolutionError::Read {
                            path: path.clone(),
                            message: error.to_string(),
                        });
                    }
                };
                let actual = content_hash(&bytes);
                if let Some(expected) = expected_hash {
                    let expected = expected.to_string();
                    if expected != actual {
                        return Err(InputResolutionError::HashMismatch {
                            path: path.clone(),
                            expected,
                            actual,
                        });
                    }
                }
                let content = match String::from_utf8(bytes) {
                    Ok(content) => content,
                    Err(_) => return Err(InputResolutionError::NotUtf8 { path: path.clone() }),
                };
                let recorded = contained.to_string_lossy().into_owned();
                file_state.record(recorded.clone(), actual.clone());
                // A range attaches only those lines; the whole file was
                // hashed and recorded above, so an edit needs no re-read.
                let attachment = match range {
                    None => ResolvedAttachment {
                        path: recorded,
                        digest: actual,
                        window: None,
                        content,
                    },
                    Some(range) => {
                        let total = content.lines().count();
                        let start = usize::try_from(range.start).unwrap_or(usize::MAX);
                        if start > total {
                            return Err(InputResolutionError::RangeOutOfBounds {
                                path: path.clone(),
                                start: range.start,
                                lines: total,
                            });
                        }
                        let end = usize::try_from(range.end).unwrap_or(usize::MAX).min(total);
                        let window: String = content
                            .split_inclusive('\n')
                            .skip(start - 1)
                            .take(end - start + 1)
                            .collect();
                        ResolvedAttachment {
                            path: recorded,
                            digest: actual,
                            window: Some((start, end, total)),
                            content: window,
                        }
                    }
                };
                render_file_attachment(
                    &mut attachments,
                    &attachment.path,
                    attachment.window,
                    Some(&attachment.content),
                );
                resolved.push(attachment);
            }
        }
    }
    if !attachments.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&attachments);
    }
    if text.len() > MAX_RESOLVED_INPUT_BYTES {
        return Err(InputResolutionError::TooLarge);
    }
    Ok(ResolvedInput {
        text,
        attachments: resolved,
    })
}

/// Appends the placeholder-free prompt text for one message: `text` followed
/// by each attachment rendered exactly as the model first saw it. Used by
/// context reconstruction; the live run renders through `resolve_blocking`,
/// which produces the same bytes for the same inputs.
pub(crate) fn render_resolved_prompt<'a>(
    text: &str,
    attachments: impl IntoIterator<Item = (&'a str, Option<(usize, usize, usize)>, Option<&'a str>)>,
) -> String {
    let mut prompt = text.to_owned();
    let mut rendered = String::new();
    for (path, window, content) in attachments {
        render_file_attachment(&mut rendered, path, window, content);
    }
    if !rendered.is_empty() {
        if !prompt.is_empty() && !prompt.ends_with('\n') {
            prompt.push('\n');
        }
        prompt.push_str(&rendered);
    }
    prompt
}

/// Renders one `<attached-file>` block. `None` content is a stored
/// attachment whose bytes the per-session retention cap reclaimed; the block
/// says so explicitly rather than reverting to the bare `@path` placeholder.
fn render_file_attachment(
    into: &mut String,
    path: &str,
    window: Option<(usize, usize, usize)>,
    content: Option<&str>,
) {
    into.push_str("\n<attached-file path=\"");
    into.push_str(path);
    into.push('"');
    if let Some((start, end, total)) = window {
        into.push_str(&format!(" lines=\"{start}-{end}/{total}\""));
    }
    let Some(content) = content else {
        into.push_str(" evicted=\"true\">\n");
        into.push_str(
            "[the attached content was evicted from session storage; \
             read the file again to see its current state]\n",
        );
        into.push_str("</attached-file>\n");
        return;
    };
    into.push_str(">\n");
    // A fence longer than any backtick run inside the file (and never shorter
    // than four) keeps the content unambiguous for the model.
    let longest_run = content.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest_run.max(3) + 1);
    into.push_str(&fence);
    into.push('\n');
    into.push_str(content);
    if !content.ends_with('\n') {
        into.push('\n');
    }
    into.push_str(&fence);
    into.push_str("\n</attached-file>\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.md"), "# Notes\n\nuse ``` fences\n").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/data.txt"), b"tail no newline").unwrap();
        std::fs::write(dir.path().join("bin.dat"), [0xff, 0xfe, 0x00]).unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let workspace = Workspace::open(&root).unwrap();
        (dir, workspace)
    }

    #[test]
    fn text_and_files_render_with_safe_fences_and_record_file_state() {
        let (_dir, workspace) = workspace();
        let state = Arc::new(FileState::default());
        let parts = vec![
            InputPart::text("Summarize:"),
            InputPart::WorkspaceFile {
                path: "notes.md".to_owned(),
                expected_hash: None,
                range: None,
            },
            InputPart::WorkspaceFile {
                path: "./sub/data.txt".to_owned(),
                expected_hash: None,
                range: None,
            },
        ];
        let resolved = resolve_blocking(&parts, &workspace, &state).unwrap();
        let text = resolved.text;
        assert!(text.starts_with("Summarize:\n\n<attached-file path=\"notes.md\">\n````\n# Notes"));
        assert!(text.contains("````\n</attached-file>\n\n<attached-file path=\"sub/data.txt\">\n````\ntail no newline\n````\n</attached-file>\n"));
        assert_eq!(
            state.recorded("notes.md").unwrap(),
            content_hash(b"# Notes\n\nuse ``` fences\n")
        );
        assert!(state.recorded("sub/data.txt").is_some());
        assert_eq!(
            render_text(&parts),
            "Summarize:\n@notes.md\n@./sub/data.txt\n"
        );
        // The attachments the run persists reproduce the live text exactly.
        assert_eq!(resolved.attachments.len(), 2);
        assert_eq!(resolved.attachments[0].path, "notes.md");
        assert_eq!(
            resolved.attachments[0].digest,
            content_hash(b"# Notes\n\nuse ``` fences\n")
        );
        assert_eq!(resolved.attachments[1].content, "tail no newline");
        let reconstructed = render_resolved_prompt(
            "Summarize:",
            resolved.attachments.iter().map(|attachment| {
                (
                    attachment.path.as_str(),
                    attachment.window,
                    Some(attachment.content.as_str()),
                )
            }),
        );
        assert_eq!(reconstructed, text);
    }

    #[test]
    fn an_evicted_attachment_renders_an_explicit_stub() {
        let text = render_resolved_prompt("see\n", [("notes.md", Some((3, 3, 3)), None)]);
        assert_eq!(
            text,
            "see\n\n<attached-file path=\"notes.md\" lines=\"3-3/3\" evicted=\"true\">\n\
             [the attached content was evicted from session storage; \
             read the file again to see its current state]\n</attached-file>\n"
        );
    }

    #[test]
    fn a_range_attaches_only_those_lines_but_records_the_whole_file() {
        let (_dir, workspace) = workspace();
        let state = Arc::new(FileState::default());
        let parts = vec![
            InputPart::text("see"),
            InputPart::WorkspaceFile {
                path: "notes.md".to_owned(),
                expected_hash: None,
                range: Some(qq_protocol::LineRange { start: 3, end: 99 }),
            },
        ];
        let resolved = resolve_blocking(&parts, &workspace, &state).unwrap();
        let text = resolved.text;
        assert!(
            text.contains(
                "<attached-file path=\"notes.md\" lines=\"3-3/3\">\n````\nuse ``` fences\n````\n"
            ),
            "{text}"
        );
        assert!(!text.contains("# Notes"));
        assert_eq!(resolved.attachments[0].window, Some((3, 3, 3)));
        assert_eq!(resolved.attachments[0].content, "use ``` fences\n");
        // The hash is of the whole file: an edit after this needs no read.
        assert_eq!(
            state.recorded("notes.md").unwrap(),
            content_hash(b"# Notes\n\nuse ``` fences\n")
        );
        let past = vec![InputPart::WorkspaceFile {
            path: "notes.md".to_owned(),
            expected_hash: None,
            range: Some(qq_protocol::LineRange { start: 4, end: 4 }),
        }];
        assert!(matches!(
            resolve_blocking(&past, &workspace, &state),
            Err(InputResolutionError::RangeOutOfBounds {
                start: 4,
                lines: 3,
                ..
            })
        ));
        let inverted = vec![InputPart::WorkspaceFile {
            path: "notes.md".to_owned(),
            expected_hash: None,
            range: Some(qq_protocol::LineRange { start: 0, end: 4 }),
        }];
        assert!(matches!(
            resolve_blocking(&inverted, &workspace, &state),
            Err(InputResolutionError::Invalid(
                qq_protocol::InputError::InvalidRange { index: 0 }
            ))
        ));
    }

    #[test]
    fn every_failure_is_typed_and_happens_before_any_provider_work() {
        let (_dir, workspace) = workspace();
        let state = Arc::new(FileState::default());
        let file = |path: &str, hash: Option<[u8; 32]>| InputPart::WorkspaceFile {
            path: path.to_owned(),
            expected_hash: hash.map(qq_protocol::ContentHash::from_bytes),
            range: None,
        };
        assert!(matches!(
            resolve_blocking(&[], &workspace, &state),
            Err(InputResolutionError::Invalid(_))
        ));
        assert!(matches!(
            resolve_blocking(&[file("../etc/passwd", None)], &workspace, &state),
            Err(InputResolutionError::Path { .. })
        ));
        assert!(matches!(
            resolve_blocking(&[file("sub", None)], &workspace, &state),
            Err(InputResolutionError::NotAFile { .. })
        ));
        assert!(matches!(
            resolve_blocking(&[file("missing.txt", None)], &workspace, &state),
            Err(InputResolutionError::Path { .. })
        ));
        assert!(matches!(
            resolve_blocking(&[file("bin.dat", None)], &workspace, &state),
            Err(InputResolutionError::NotUtf8 { .. })
        ));
        let Err(InputResolutionError::HashMismatch {
            expected, actual, ..
        }) = resolve_blocking(&[file("notes.md", Some([9; 32]))], &workspace, &state)
        else {
            panic!("stale hash must be reported")
        };
        assert_eq!(expected, "09".repeat(32));
        assert_eq!(actual, content_hash(b"# Notes\n\nuse ``` fences\n"));
        assert!(state.recorded("bin.dat").is_none());
        let error = InputResolutionError::TooLarge;
        assert_eq!(error.failure_kind(), RunFailureKind::InvalidCommand);
    }

    #[test]
    fn oversized_attachments_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("big.txt"),
            "x".repeat(MAX_INPUT_FILE_BYTES + 1),
        )
        .unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let workspace = Workspace::open(&root).unwrap();
        let state = Arc::new(FileState::default());
        assert!(matches!(
            resolve_blocking(
                &[InputPart::WorkspaceFile {
                    path: "big.txt".to_owned(),
                    expected_hash: None,
                    range: None,
                }],
                &workspace,
                &state
            ),
            Err(InputResolutionError::FileTooLarge { .. })
        ));
    }
}
