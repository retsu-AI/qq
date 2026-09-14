//! Shared loopback HTTP fixtures for provider interface tests.
//!
//! Compiled for unit tests and, behind the `test-support` feature, for this
//! package's own integration tests. Not part of the crate's public API.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    thread::{self, JoinHandle},
    time::Duration,
};

pub struct CapturedRequest {
    wire: String,
}

impl CapturedRequest {
    pub fn request_line(&self) -> Option<&str> {
        self.head().lines().next()
    }

    pub fn header(&self, expected_name: &str) -> Option<&str> {
        self.head()
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case(expected_name))
            .map(|(_, value)| value.trim())
    }

    pub fn json_body(&self) -> serde_json::Value {
        serde_json::from_str(self.body()).expect("captured request body must be JSON")
    }

    fn head(&self) -> &str {
        self.wire
            .split_once("\r\n\r\n")
            .expect("captured request must contain an HTTP head")
            .0
    }

    fn body(&self) -> &str {
        self.wire
            .split_once("\r\n\r\n")
            .expect("captured request must contain an HTTP body separator")
            .1
    }
}

pub struct LoopbackServer {
    pub base_url: String,
    request: JoinHandle<CapturedRequest>,
}

impl LoopbackServer {
    pub fn sse(body: impl Into<String>) -> Self {
        Self::respond(200, "text/event-stream", body)
    }

    /// Serves an SSE response whose body arrives as the given wire chunks,
    /// flushed one at a time — for byte-boundary and UTF-8 frame-splitting
    /// tests. Chunks may split multi-byte characters.
    pub fn sse_chunks(chunks: Vec<Vec<u8>>) -> Self {
        Self::respond_chunks(200, Some("text/event-stream"), chunks)
    }

    pub fn respond(status: u16, content_type: &'static str, body: impl Into<String>) -> Self {
        Self::respond_chunks(status, Some(content_type), vec![body.into().into_bytes()])
    }

    /// The general form: arbitrary status, optional `Content-Type` (omitted
    /// entirely when `None`, so missing-header behavior is testable), and a
    /// scripted body written chunk by chunk.
    pub fn respond_chunks(
        status: u16,
        content_type: Option<&'static str>,
        chunks: Vec<Vec<u8>>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener must bind");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let request = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("loopback request must connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("loopback read timeout must be configurable");
            let request = read_request(&mut stream);
            let reason = match status {
                200 => "OK",
                401 => "Unauthorized",
                _ => "Test Response",
            };
            let content_type_header = content_type
                .map(|value| format!("Content-Type: {value}\r\n"))
                .unwrap_or_default();
            let content_length = chunks.iter().map(Vec::len).sum::<usize>();
            let head = format!(
                "HTTP/1.1 {status} {reason}\r\n{content_type_header}Content-Length: {content_length}\r\nConnection: close\r\n\r\n"
            );
            stream
                .write_all(head.as_bytes())
                .expect("loopback response head must be written");
            for chunk in chunks {
                stream
                    .write_all(&chunk)
                    .expect("loopback response chunk must be written");
                stream.flush().expect("loopback response chunk must flush");
                thread::sleep(Duration::from_millis(1));
            }
            CapturedRequest {
                wire: String::from_utf8(request).expect("captured request must be UTF-8"),
            }
        });
        Self { base_url, request }
    }

    pub fn capture(self) -> CapturedRequest {
        self.request.join().expect("loopback server must not panic")
    }
}

fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0; 4_096];
    loop {
        let read = stream.read(&mut buffer).expect("request read must succeed");
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let body_start = header_end + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or_default();
        if request.len() >= body_start + content_length {
            break;
        }
    }
    request
}

/// Encodes the HTTP body an adapter for `protocol` would send for `request`,
/// exactly as `sse_exchange` serializes it, without a transport. For the
/// `provider_encode` benchmark and body-shape tests.
///
/// # Panics
///
/// Panics when the request cannot be encoded for the protocol (a Google
/// tool result whose call is not in the transcript, or a body serde_json
/// refuses); both are programming errors in a fixture.
#[must_use]
pub fn encode_body(protocol: crate::HttpProtocol, request: &crate::ModelRequest) -> Vec<u8> {
    use crate::providers::{anthropic, google, openai, openai_chat};
    let encoded = match protocol {
        crate::HttpProtocol::OpenAiResponses => serde_json::to_vec(&openai::ResponsesRequest::new(
            request,
            openai::ResponsesRequestKind::Standard,
        )),
        crate::HttpProtocol::OpenAiChatCompletions => {
            serde_json::to_vec(&openai_chat::ChatCompletionsRequest::from(request))
        }
        crate::HttpProtocol::AnthropicMessages => {
            serde_json::to_vec(&anthropic::MessagesRequest::from(request))
        }
        crate::HttpProtocol::GoogleGenerateContent => serde_json::to_vec(
            &google::GenerateContentRequest::new(request, 4096)
                .expect("fixture requests must encode for Google"),
        ),
    };
    encoded.expect("a wire request body must serialize")
}

/// Frames one SSE body chunk-by-chunk through a provider decoder and parses
/// every event as its adapter would, without a transport. For the
/// `sse_decode` benchmark. Returns the number of events parsed.
///
/// # Panics
///
/// Panics when the body is not a well-formed stream for the protocol; the
/// bench fixtures are.
pub fn decode_sse_body(protocol: crate::HttpProtocol, chunks: &[&[u8]], parse: bool) -> usize {
    use crate::providers::{anthropic, openai};
    let redactions: [String; 0] = [];
    let mut decoder = match protocol {
        crate::HttpProtocol::OpenAiResponses | crate::HttpProtocol::OpenAiChatCompletions => {
            openai::sse_decoder(usize::MAX)
        }
        crate::HttpProtocol::AnthropicMessages => anthropic::sse_decoder(usize::MAX),
        crate::HttpProtocol::GoogleGenerateContent => {
            panic!("the sse_decode bench covers the OpenAI and Anthropic decoders")
        }
    };
    let mut parsed = 0;
    for chunk in chunks {
        for event in decoder.push(chunk).expect("fixture body frames") {
            if parse {
                match protocol {
                    crate::HttpProtocol::AnthropicMessages => {
                        anthropic::decode_event(event, &redactions).expect("fixture event decodes");
                    }
                    _ => {
                        openai::decode_event(&event.data, &redactions)
                            .expect("fixture event decodes");
                    }
                }
            }
            parsed += 1;
        }
    }
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Raw TCP client so assertions run against the exact wire bytes the
    /// harness emits, independent of any HTTP client's normalization.
    fn raw_exchange(base_url: &str, body: &str) -> (String, String) {
        let authority = base_url
            .strip_prefix("http://")
            .expect("loopback base URL must be plain HTTP");
        let mut stream = TcpStream::connect(authority).expect("loopback connect must succeed");
        let request = format!(
            "POST /probe HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(request.as_bytes())
            .expect("probe request must be written");
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .expect("probe response must be readable");
        let response = String::from_utf8(response).expect("harness responses are UTF-8 in tests");
        let (head, body) = response
            .split_once("\r\n\r\n")
            .expect("harness response must contain a head/body separator");
        (head.to_owned(), body.to_owned())
    }

    fn head_header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
        head.lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim())
    }

    #[test]
    fn sse_chunks_reassemble_in_order_even_when_split_mid_utf8() {
        let heart = "❤".as_bytes();
        let server = LoopbackServer::sse_chunks(vec![
            b"data: {\"text\":\"".to_vec(),
            heart[..1].to_vec(),
            heart[1..].to_vec(),
            b"\"}\n\n".to_vec(),
        ]);

        let (head, body) = raw_exchange(&server.base_url, "{}");

        assert!(head.starts_with("HTTP/1.1 200"), "unexpected head: {head}");
        assert_eq!(
            head_header(&head, "content-type"),
            Some("text/event-stream")
        );
        assert_eq!(
            head_header(&head, "content-length"),
            Some("data: {\"text\":\"❤\"}\n\n".len().to_string().as_str())
        );
        assert_eq!(body, "data: {\"text\":\"❤\"}\n\n");
    }

    #[test]
    fn respond_chunks_supports_arbitrary_status_and_content_type() {
        let server = LoopbackServer::respond_chunks(
            429,
            Some("application/json; charset=utf-8"),
            vec![b"{\"error\":\"slow down\"}".to_vec()],
        );

        let (head, body) = raw_exchange(&server.base_url, "{}");

        assert!(head.starts_with("HTTP/1.1 429"), "unexpected head: {head}");
        assert_eq!(
            head_header(&head, "content-type"),
            Some("application/json; charset=utf-8")
        );
        assert_eq!(body, "{\"error\":\"slow down\"}");
    }

    #[test]
    fn respond_chunks_can_omit_the_content_type_header() {
        let server = LoopbackServer::respond_chunks(200, None, vec![b"data: [DONE]\n\n".to_vec()]);

        let (head, body) = raw_exchange(&server.base_url, "{}");

        assert_eq!(head_header(&head, "content-type"), None);
        assert_eq!(body, "data: [DONE]\n\n");
    }

    #[test]
    fn capture_exposes_request_line_headers_and_json_body() {
        let server = LoopbackServer::respond(200, "application/json", "{}");

        let (_, _) = raw_exchange(&server.base_url, "{\"model\":\"test-model\"}");
        let request = server.capture();

        assert_eq!(request.request_line(), Some("POST /probe HTTP/1.1"));
        assert_eq!(request.header("content-type"), Some("application/json"));
        assert_eq!(request.header("x-absent"), None);
        assert_eq!(request.json_body()["model"], "test-model");
    }
}
