//! Plain-text command output.

use std::{
    borrow::Cow,
    io::{self, Write},
};

use futures_util::{Stream, StreamExt};
use qq_protocol::RunEvent;
use thiserror::Error;

pub async fn render(
    events: impl Stream<Item = RunEvent>,
    writer: &mut impl Write,
    mode: OutputMode,
) -> Result<(), OutputError> {
    let mut events = Box::pin(events);
    let mut wrote_text = false;
    let mut ends_with_newline = false;

    while let Some(event) = events.next().await {
        match event {
            RunEvent::Started
            | RunEvent::ActivityChanged { .. }
            | RunEvent::ReasoningStarted { .. }
            | RunEvent::ReasoningDelta { .. }
            | RunEvent::ReasoningCompleted { .. }
            | RunEvent::Usage { .. } => {}
            RunEvent::OutputTextDelta { text } | RunEvent::RefusalDelta { text } => {
                let text = output_text(&text, mode);
                writer.write_all(text.as_bytes())?;
                writer.flush()?;
                if !text.is_empty() {
                    wrote_text = true;
                    ends_with_newline = text.ends_with('\n');
                }
            }
            RunEvent::Completed => {
                if !wrote_text || !ends_with_newline {
                    writer.write_all(b"\n")?;
                }
                writer.flush()?;
                return Ok(());
            }
            RunEvent::Failed { message, .. } => {
                if wrote_text && !ends_with_newline {
                    writer.write_all(b"\n")?;
                    writer.flush()?;
                }
                return Err(OutputError::RunFailed(message));
            }
        }
    }

    Err(OutputError::IncompleteRun)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Raw,
    Terminal,
}

fn output_text(text: &str, mode: OutputMode) -> Cow<'_, str> {
    if mode == OutputMode::Raw
        || !text
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        return Cow::Borrowed(text);
    }

    Cow::Owned(
        text.chars()
            .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
            .collect(),
    )
}

#[derive(Debug, Error)]
pub enum OutputError {
    #[error("could not write output: {0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    RunFailed(String),
    #[error("run ended without a terminal event")]
    IncompleteRun,
}

#[cfg(test)]
mod tests {
    use futures_util::stream;

    use super::*;

    #[tokio::test]
    async fn writes_text_deltas_as_plain_text() {
        let events = stream::iter([
            RunEvent::Started,
            RunEvent::OutputTextDelta {
                text: "hel".to_owned(),
            },
            RunEvent::OutputTextDelta {
                text: "lo".to_owned(),
            },
            RunEvent::Completed,
        ]);
        let mut output = Vec::new();

        render(events, &mut output, OutputMode::Raw).await.unwrap();

        assert_eq!(output, b"hello\n");
    }

    #[tokio::test]
    async fn excludes_reasoning_from_plain_text_output() {
        let events = stream::iter([
            RunEvent::ReasoningStarted {
                kind: qq_protocol::ReasoningKind::Summary,
            },
            RunEvent::ReasoningDelta {
                kind: qq_protocol::ReasoningKind::Summary,
                text: "private intermediate text".to_owned(),
            },
            RunEvent::ReasoningCompleted {
                kind: qq_protocol::ReasoningKind::Summary,
            },
            RunEvent::OutputTextDelta {
                text: "answer".to_owned(),
            },
            RunEvent::Completed,
        ]);
        let mut output = Vec::new();

        render(events, &mut output, OutputMode::Raw).await.unwrap();

        assert_eq!(output, b"answer\n");
    }

    #[tokio::test]
    async fn writes_refusals_without_adding_a_second_newline() {
        let events = stream::iter([
            RunEvent::Started,
            RunEvent::RefusalDelta {
                text: "cannot help\n".to_owned(),
            },
            RunEvent::Completed,
        ]);
        let mut output = Vec::new();

        render(events, &mut output, OutputMode::Raw).await.unwrap();

        assert_eq!(output, b"cannot help\n");
    }

    #[tokio::test]
    async fn strips_terminal_control_characters_but_keeps_layout() {
        let events = stream::iter([
            RunEvent::OutputTextDelta {
                text: "safe\x1b]52;clipboard\x07\n\ttext".to_owned(),
            },
            RunEvent::Completed,
        ]);
        let mut output = Vec::new();

        render(events, &mut output, OutputMode::Terminal)
            .await
            .unwrap();

        assert_eq!(output, b"safe]52;clipboard\n\ttext\n");
    }
}
