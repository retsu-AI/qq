//! The two-turn read loop the `tool_dispatch` bench times, shared with the
//! test that proves one iteration finishes. A bench whose fixture stops
//! completing hangs instead of failing; the test keeps that from going
//! unnoticed (ENG-1003).

use futures_util::stream;
use qq_provider::{ContentBlock, ModelRequest, Provider, ProviderEvent, ProviderStream};

/// Calls `read_file` once, then answers once the result is in context.
pub struct ReadToolProvider;

impl Provider for ReadToolProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let has_result = request
            .messages()
            .iter()
            .flat_map(|message| message.content())
            .any(|block| matches!(block, ContentBlock::ToolResult { .. }));
        if has_result {
            // A real answer: an empty, unmetered completion after fresh tool
            // results is a swallowed gateway fault the run loop retries.
            Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "done".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]))
        } else {
            Box::pin(stream::iter([
                Ok(ProviderEvent::ToolCallStarted {
                    id: "benchmark-call".to_owned(),
                    name: "read_file".to_owned(),
                }),
                Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: "benchmark-call".to_owned(),
                    json: r#"{"path":"input.txt"}"#.to_owned(),
                }),
                Ok(ProviderEvent::ToolCallCompleted {
                    id: "benchmark-call".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]))
        }
    }
}
