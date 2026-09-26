//! Shared SSE request/stream driver for the HTTP protocol adapters.
//!
//! [`sse_exchange`] owns the request/response prologue every SSE adapter
//! repeats: build the JSON request, execute it through [`HttpExchange`]
//! (retry, wire limits, bounded rejection buffering), gate the success
//! Content-Type, and decode the body into [`SseEvent`]s. Adapters keep
//! ownership of their error body schema (via [`SseExchangeError::Rejected`])
//! and of folding events into `ProviderEvent`s.

use std::sync::Arc;

use futures_util::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap};
use serde::Serialize;

use crate::{
    ProviderError, ProviderErrorKind, ProviderStream,
    http::{
        ExchangeMessages, ExchangeOutcome, HttpExchange, HttpRejection, SharedLedger,
        is_event_stream_headers, note_attempts, transport_error,
    },
    sse::{SseDecoder, SseEvent},
};

/// How a success response's Content-Type is validated before streaming.
#[derive(Clone, Copy)]
pub(crate) enum ContentTypeGate {
    /// The response must declare `text/event-stream`.
    Strict,
    /// ChatGPT Codex streams valid SSE frames but often omits Content-Type
    /// entirely. A present non-SSE Content-Type is still rejected so a JSON
    /// success body cannot be misread.
    AllowMissingContentType,
}

impl ContentTypeGate {
    pub(crate) fn accepts(self, headers: &HeaderMap) -> bool {
        if is_event_stream_headers(headers) {
            return true;
        }
        matches!(self, Self::AllowMissingContentType) && headers.get(CONTENT_TYPE).is_none()
    }
}

/// Protocol-owned strings and policies for one SSE exchange.
pub(crate) struct SseExchangeSpec {
    pub(crate) messages: ExchangeMessages,
    pub(crate) non_sse_response: &'static str,
    pub(crate) content_type_gate: ContentTypeGate,
}

/// Why an SSE exchange could not begin streaming.
pub(crate) enum SseExchangeError {
    /// The provider rejected the request. The adapter interprets the buffered
    /// error body against its own error schema.
    Rejected(HttpRejection),
    Provider(ProviderError),
}

impl From<ProviderError> for SseExchangeError {
    fn from(error: ProviderError) -> Self {
        Self::Provider(error)
    }
}

impl SseExchangeError {
    /// Collapses the error into a `ProviderError`, delegating rejected
    /// requests to the adapter's error-body interpreter.
    pub(crate) fn into_provider_error(
        self,
        interpret_rejection: impl FnOnce(HttpRejection) -> ProviderError,
    ) -> ProviderError {
        match self {
            Self::Rejected(rejection) => interpret_rejection(rejection),
            Self::Provider(error) => error,
        }
    }
}

/// Decoded SSE events from an accepted exchange.
pub(crate) struct SseExchangeStream {
    redactions: Arc<[String]>,
    chunks: futures_core::stream::BoxStream<'static, Result<bytes::Bytes, ProviderError>>,
    decoder: SseDecoder,
    /// Events framed from the last chunk, yielded in order; the framer
    /// appends straight into this buffer so a chunk costs no extra list.
    pending: Vec<SseEvent>,
    next: usize,
    decoded_any: bool,
}

impl SseExchangeStream {
    /// Values echoed by the provider that must never appear in errors.
    pub(crate) fn redactions(&self) -> &Arc<[String]> {
        &self.redactions
    }

    /// Returns the next decoded SSE event, or `None` when the body ends.
    pub(crate) async fn next_event(&mut self) -> Result<Option<SseEvent>, ProviderError> {
        loop {
            if self.next < self.pending.len() {
                // Take without shifting; the buffer is reused once drained.
                let event = std::mem::replace(
                    &mut self.pending[self.next],
                    SseEvent {
                        name: None,
                        data: String::new(),
                    },
                );
                self.next += 1;
                self.decoded_any = true;
                return Ok(Some(event));
            }
            self.pending.clear();
            self.next = 0;
            match self.chunks.next().await {
                Some(chunk) => self.decoder.push_into(&chunk?, &mut self.pending)?,
                None => return Ok(None),
            }
        }
    }

    /// The error for a body that ended before the protocol's terminal event.
    /// A body that carried no event at all is a dropped connection
    /// (`Transport`, retryable under the attempt policy); one that ended
    /// mid-conversation is the provider's protocol violation.
    pub(crate) fn ended_early(&self, message: &str) -> ProviderError {
        if self.decoded_any {
            ProviderError::Protocol(message.to_owned())
        } else {
            ProviderError::Transport(format!("{message} (no events were received)"))
        }
    }
}

/// Encodes a request body into a buffer sized from the payload. The hint is a
/// lower bound on the body, so the writer grows at most once instead of
/// doubling from empty through a megabyte of transcript.
pub(crate) fn encode_body(
    body: &impl Serialize,
    size_hint: usize,
) -> Result<Vec<u8>, serde_json::Error> {
    let mut encoded = Vec::with_capacity(size_hint.saturating_add(size_hint / 8));
    serde_json::to_writer(&mut encoded, body)?;
    Ok(encoded)
}

/// Executes one SSE request and prepares its decoded event stream. Pre-body
/// retries draw from `ledger`; the caller keeps it so a restart after the
/// body has begun draws from the same count.
pub(crate) async fn sse_exchange(
    exchange: &HttpExchange,
    (endpoint, headers): (reqwest::Url, HeaderMap),
    (body, body_size_hint): (&impl Serialize, usize),
    decoder: SseDecoder,
    wire_limit: usize,
    spec: SseExchangeSpec,
    ledger: &SharedLedger,
) -> Result<SseExchangeStream, SseExchangeError> {
    let encoded = encode_body(body, body_size_hint)
        .map_err(|error| ProviderError::Protocol(format!("request encoding failed: {error}")))?;
    let request = exchange
        .request(reqwest::Method::POST, endpoint)
        .headers(headers)
        .header(ACCEPT, "text/event-stream")
        .header(CONTENT_TYPE, "application/json")
        .body(encoded)
        .build()
        .map_err(|error| transport_error(error, exchange.static_redactions()))?;
    let response = match exchange
        .execute(request, wire_limit, spec.messages, ledger)
        .await?
    {
        ExchangeOutcome::Success(response) => response,
        ExchangeOutcome::Rejected(rejection) => return Err(SseExchangeError::Rejected(rejection)),
    };

    if !spec.content_type_gate.accepts(response.headers()) {
        return Err(ProviderError::Protocol(spec.non_sse_response.to_owned()).into());
    }

    Ok(SseExchangeStream {
        redactions: Arc::from(response.redactions()),
        chunks: response.into_body(),
        decoder,
        pending: Vec::new(),
        next: 0,
        decoded_any: false,
    })
}

/// A provider failure the policy may spend another attempt on: the request
/// never reached the model, or the model never answered. Rejections,
/// authentication, and protocol violations are the caller's to see.
pub(crate) const fn is_transient(kind: ProviderErrorKind) -> bool {
    matches!(
        kind,
        ProviderErrorKind::Unavailable
            | ProviderErrorKind::RateLimited
            | ProviderErrorKind::Transport
    )
}

/// Wraps an adapter's stream factory in the provider's restart loop.
///
/// `attempt` builds one full attempt: request, exchange, decode, fold. The
/// wrapper polls it and, while the attempt has yielded no semantic event,
/// treats a transient error or an end-of-stream as a failed send and restarts
/// under `ledger`. Once one event has been yielded the attempt is committed:
/// its errors and its end pass through unchanged, because a resend could
/// duplicate output the caller already observed. The final error records the
/// attempts spent.
pub(crate) fn with_restart<F>(exchange: &HttpExchange, attempt: F) -> ProviderStream
where
    F: Fn(SharedLedger) -> ProviderStream + Send + Sync + 'static,
{
    let ledger = exchange.ledger();
    Box::pin(async_stream::stream! {
        loop {
            let mut inner = attempt(ledger.clone());
            let mut yielded = false;
            let delay = loop {
                match inner.next().await {
                    Some(Ok(event)) => {
                        yielded = true;
                        yield Ok(event);
                    }
                    Some(Err(error)) if !yielded && is_transient(error.kind()) => {
                        let (next_delay, attempts) = {
                            let mut ledger = ledger.lock();
                            (ledger.next_delay(None), ledger.attempts())
                        };
                        match next_delay {
                            Some(delay) => break delay,
                            None => {
                                yield Err(note_attempts(error, attempts));
                                return;
                            }
                        }
                    }
                    Some(Err(error)) => {
                        let attempts = ledger.lock().attempts();
                        yield Err(if yielded {
                            error
                        } else {
                            note_attempts(error, attempts)
                        });
                        return;
                    }
                    // A body that ends without any event is a dropped
                    // connection: the adapter's `ended_early` already turned
                    // it into a transient error above. A body that ends
                    // after events is the adapter's own terminal event.
                    None => return,
                }
            };
            drop(inner);
            tokio::time::sleep(delay).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use super::*;
    use crate::{
        http::{AttemptPolicy, build_direct_client},
        request_auth::{
            RequestAuthorizer, RequestCredential, RequestCredentialError, RequestCredentialFuture,
            RequestCredentialProvider, SharedRequestCredentialProvider,
        },
        sse::Utf8ErrorMessage,
        test_support::LoopbackServer,
    };

    const MESSAGES: ExchangeMessages = ExchangeMessages {
        wire_overflow: "test wire size overflowed",
        wire_limit: "test stream exceeded the configured wire size limit",
    };

    fn spec(content_type_gate: ContentTypeGate) -> SseExchangeSpec {
        SseExchangeSpec {
            messages: MESSAGES,
            non_sse_response: "test provider returned a non-SSE response",
            content_type_gate,
        }
    }

    fn decoder() -> SseDecoder {
        SseDecoder::data_only(
            64 * 1_024,
            "test event size overflowed",
            "test event exceeded the configured size limit",
            Utf8ErrorMessage::Static("test event was not UTF-8"),
        )
    }

    fn exchange(redactions: Vec<String>) -> HttpExchange {
        HttpExchange::new(
            build_direct_client().unwrap(),
            RequestAuthorizer::default(),
            Arc::from(redactions),
        )
        .with_attempt_policy(crate::http::AttemptPolicy::disabled())
    }

    async fn drive(
        server: &LoopbackServer,
        content_type_gate: ContentTypeGate,
    ) -> Result<SseExchangeStream, SseExchangeError> {
        let endpoint = reqwest::Url::parse(&format!("{}/v1/test", server.base_url)).unwrap();
        let exchange = exchange(vec!["loopback-secret".to_owned()]);
        let ledger = exchange.ledger();
        sse_exchange(
            &exchange,
            (endpoint, HeaderMap::new()),
            (&json!({"model": "test-model"}), 64),
            decoder(),
            1_024 * 1_024,
            spec(content_type_gate),
            &ledger,
        )
        .await
    }

    #[tokio::test]
    async fn sends_the_request_and_decodes_events_until_the_body_ends() {
        let server = LoopbackServer::sse("data: first\n\ndata: second\n\n");

        let mut sse = drive(&server, ContentTypeGate::Strict).await.ok().unwrap();

        let mut data = Vec::new();
        while let Some(event) = sse.next_event().await.unwrap() {
            data.push(event.data);
        }
        assert_eq!(data, ["first", "second"]);
        assert_eq!(sse.redactions().as_ref(), ["loopback-secret"]);

        let request = server.capture();
        assert_eq!(request.request_line(), Some("POST /v1/test HTTP/1.1"));
        assert_eq!(request.header("accept"), Some("text/event-stream"));
        assert_eq!(request.json_body()["model"], "test-model");
    }

    #[tokio::test]
    async fn rejections_hand_the_buffered_error_body_back_to_the_adapter() {
        let server = LoopbackServer::respond(503, "application/json", "{\"error\":\"overloaded\"}");

        let error = drive(&server, ContentTypeGate::Strict).await.err().unwrap();

        let SseExchangeError::Rejected(rejection) = error else {
            panic!("expected a rejection");
        };
        assert_eq!(rejection.status().as_u16(), 503);
        assert_eq!(rejection.body(), b"{\"error\":\"overloaded\"}");
    }

    #[tokio::test]
    async fn strict_gates_reject_missing_and_wrong_content_types() {
        for server in [
            LoopbackServer::respond_chunks(200, None, vec![b"data: x\n\n".to_vec()]),
            LoopbackServer::respond(200, "application/json", "{}"),
        ] {
            let error = drive(&server, ContentTypeGate::Strict).await.err().unwrap();

            let error = error.into_provider_error(|_| unreachable!("no rejection expected"));
            assert!(matches!(
                &error,
                ProviderError::Protocol(message)
                    if message == "test provider returned a non-SSE response"
            ));
        }
    }

    #[tokio::test]
    async fn the_codex_gate_accepts_missing_but_not_wrong_content_types() {
        let missing = LoopbackServer::respond_chunks(200, None, vec![b"data: ok\n\n".to_vec()]);
        let mut sse = drive(&missing, ContentTypeGate::AllowMissingContentType)
            .await
            .ok()
            .unwrap();
        assert_eq!(
            sse.next_event().await.unwrap().map(|event| event.data),
            Some("ok".to_owned())
        );

        let wrong = LoopbackServer::respond(200, "application/json", "{}");
        let error = drive(&wrong, ContentTypeGate::AllowMissingContentType)
            .await
            .err()
            .unwrap();
        assert!(matches!(
            error.into_provider_error(|_| unreachable!("no rejection expected")),
            ProviderError::Protocol(message)
                if message == "test provider returned a non-SSE response"
        ));
    }

    /// A scripted stream factory: each call consumes the next script entry.
    fn scripted(
        policy: AttemptPolicy,
        scripts: Vec<Vec<Result<crate::ProviderEvent, ProviderError>>>,
    ) -> (ProviderStream, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let exchange = exchange(Vec::new()).with_attempt_policy(policy);
        let calls = Arc::new(AtomicUsize::new(0));
        let scripts = Arc::new(std::sync::Mutex::new(scripts.into_iter()));
        let observed = Arc::clone(&calls);
        let stream = with_restart(&exchange, move |ledger| {
            // Every factory call is one send against the shared ledger, the
            // way `sse_exchange` records it.
            ledger.lock().begin_attempt();
            calls.fetch_add(1, Ordering::SeqCst);
            let script = scripts
                .lock()
                .unwrap()
                .next()
                .expect("the script must cover every attempt");
            Box::pin(futures_util::stream::iter(script))
        });
        (stream, observed)
    }

    fn fast(attempts: u32) -> AttemptPolicy {
        AttemptPolicy::new(
            attempts,
            std::time::Duration::from_millis(1),
            std::time::Duration::from_millis(1),
            std::time::Duration::from_secs(5),
        )
    }

    fn text(text: &str) -> Result<crate::ProviderEvent, ProviderError> {
        Ok(crate::ProviderEvent::OutputTextDelta {
            text: text.to_owned(),
        })
    }

    struct FailingCredentials {
        error: RequestCredentialError,
        calls: Arc<AtomicUsize>,
    }

    impl RequestCredentialProvider for FailingCredentials {
        fn credential(&self) -> RequestCredentialFuture<'_> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let error = self.error.clone();
            Box::pin(async move { Err::<RequestCredential, _>(error) })
        }
    }

    struct RecoveringCredentials {
        error: RequestCredentialError,
        calls: Arc<AtomicUsize>,
    }

    impl RequestCredentialProvider for RecoveringCredentials {
        fn credential(&self) -> RequestCredentialFuture<'_> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let error = self.error.clone();
            Box::pin(async move {
                if call == 0 {
                    Err(error)
                } else {
                    RequestCredential::bearer("test-token")
                }
            })
        }
    }

    #[tokio::test]
    async fn credential_timeout_and_capacity_do_not_restart_before_first_event() {
        for error in [
            RequestCredentialError::TimedOut,
            RequestCredentialError::CapacityUnavailable,
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let endpoint = reqwest::Url::parse(&format!(
                "http://{}/v1/test",
                listener.local_addr().unwrap()
            ))
            .unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let credentials = SharedRequestCredentialProvider::new(FailingCredentials {
                error: error.clone(),
                calls: Arc::clone(&calls),
            });
            let exchange = Arc::new(
                HttpExchange::new(
                    build_direct_client().unwrap(),
                    RequestAuthorizer::request_time_bearer(credentials),
                    Arc::from(Vec::<String>::new()),
                )
                .with_attempt_policy(fast(4)),
            );
            let attempt_exchange = Arc::clone(&exchange);
            let stream = with_restart(&exchange, move |ledger| {
                let exchange = Arc::clone(&attempt_exchange);
                let endpoint = endpoint.clone();
                Box::pin(async_stream::stream! {
                    let result = sse_exchange(
                        &exchange,
                        (endpoint, HeaderMap::new()),
                        (&json!({"model": "test-model"}), 64),
                        decoder(),
                        1_024 * 1_024,
                        spec(ContentTypeGate::Strict),
                        &ledger,
                    ).await;
                    match result {
                        Err(error) => yield Err(error.into_provider_error(|_| unreachable!())),
                        Ok(_) => panic!("credential failure must prevent a send"),
                    }
                })
            });
            let events: Vec<_> = stream.collect().await;
            assert_eq!(calls.load(Ordering::SeqCst), 1, "{error:?}");
            assert_eq!(events.len(), 1, "{events:?}");
            assert!(matches!(
                &events[0],
                Err(ProviderError::CredentialsUnavailable(message))
                    if message == &error.to_string()
            ));
            assert!(
                matches!(listener.accept(), Err(io_error) if io_error.kind() == std::io::ErrorKind::WouldBlock),
                "credential failure must not reach the transport"
            );
        }
    }

    #[tokio::test]
    async fn temporary_refresh_and_storage_failures_can_restart_before_first_event() {
        for error in [
            RequestCredentialError::RefreshUnavailable,
            RequestCredentialError::StorageUnavailable,
        ] {
            let server = LoopbackServer::sse("data: recovered\n\n");
            let endpoint = reqwest::Url::parse(&format!("{}/v1/test", server.base_url)).unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let credentials = SharedRequestCredentialProvider::new(RecoveringCredentials {
                error: error.clone(),
                calls: Arc::clone(&calls),
            });
            let exchange = Arc::new(
                HttpExchange::new(
                    build_direct_client().unwrap(),
                    RequestAuthorizer::request_time_bearer(credentials),
                    Arc::from(Vec::<String>::new()),
                )
                .with_attempt_policy(fast(4)),
            );
            let attempt_exchange = Arc::clone(&exchange);
            let stream = with_restart(&exchange, move |ledger| {
                let exchange = Arc::clone(&attempt_exchange);
                let endpoint = endpoint.clone();
                Box::pin(async_stream::stream! {
                    match sse_exchange(
                        &exchange,
                        (endpoint, HeaderMap::new()),
                        (&json!({"model": "test-model"}), 64),
                        decoder(),
                        1_024 * 1_024,
                        spec(ContentTypeGate::Strict),
                        &ledger,
                    ).await {
                        Err(error) => yield Err(error.into_provider_error(|_| unreachable!())),
                        Ok(mut response) => {
                            let event = response.next_event().await.unwrap().unwrap();
                            yield Ok(crate::ProviderEvent::OutputTextDelta { text: event.data });
                            yield Ok(crate::ProviderEvent::Completed { usage: None });
                        }
                    }
                })
            });
            let events: Vec<_> = stream.collect().await;
            assert_eq!(calls.load(Ordering::SeqCst), 2, "{error:?}");
            assert_eq!(events.len(), 2, "{events:?}");
            assert!(events.iter().all(Result::is_ok));
            assert_eq!(
                server.capture().request_line(),
                Some("POST /v1/test HTTP/1.1")
            );
        }
    }

    #[tokio::test]
    async fn restarts_a_stream_that_fails_before_its_first_event() {
        let (stream, calls) = scripted(
            fast(4),
            vec![
                vec![Err(ProviderError::Transport("dropped".to_owned()))],
                vec![Err(ProviderError::Api {
                    status: 503,
                    message: "overloaded".to_owned(),
                })],
                vec![
                    text("a"),
                    text("b"),
                    Ok(crate::ProviderEvent::Completed { usage: None }),
                ],
            ],
        );
        let events: Vec<_> = stream.collect().await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert_eq!(events.len(), 3, "{events:?}");
        assert!(events.iter().all(Result::is_ok));
    }

    #[tokio::test]
    async fn a_body_that_ends_before_any_event_costs_one_attempt() {
        // The adapter reports an empty body through `ended_early`, which is
        // transport-class before the first event and protocol-class after.
        let empty = SseExchangeStream {
            redactions: Arc::from(Vec::<String>::new()),
            chunks: Box::pin(futures_util::stream::empty()),
            decoder: decoder(),
            pending: Vec::new(),
            next: 0,
            decoded_any: false,
        };
        let before = empty.ended_early("stream ended before done");
        assert_eq!(before.kind(), ProviderErrorKind::Transport);
        let mut after = empty;
        after.decoded_any = true;
        assert_eq!(
            after.ended_early("stream ended before done").kind(),
            ProviderErrorKind::Protocol
        );

        let (stream, calls) = scripted(
            fast(4),
            vec![
                vec![Err(before)],
                vec![
                    text("late"),
                    Ok(crate::ProviderEvent::Completed { usage: None }),
                ],
            ],
        );
        let events: Vec<_> = stream.collect().await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(events.len(), 2, "{events:?}");
    }

    #[tokio::test]
    async fn never_restarts_after_an_event_has_been_yielded() {
        // Duplicating "a" would be visible to the caller, so the transient
        // error after it passes straight through and nothing is resent.
        let (stream, calls) = scripted(
            fast(4),
            vec![vec![
                text("a"),
                Err(ProviderError::Transport("cut mid-stream".to_owned())),
            ]],
        );
        let events: Vec<_> = stream.collect().await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(events.len(), 2);
        assert!(events[0].is_ok());
        let error = events[1].as_ref().unwrap_err();
        assert_eq!(error.kind(), ProviderErrorKind::Transport);
        assert_eq!(error.to_string(), "provider request failed: cut mid-stream");
    }

    #[tokio::test]
    async fn attempts_never_exceed_the_policy_and_the_failure_records_them() {
        let (stream, calls) = scripted(
            fast(3),
            vec![
                vec![Err(ProviderError::Transport("one".to_owned()))],
                vec![Err(ProviderError::Transport("two".to_owned()))],
                vec![Err(ProviderError::Transport("three".to_owned()))],
                vec![text("never reached")],
            ],
        );
        let events: Vec<_> = stream.collect().await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert_eq!(events.len(), 1);
        let error = events[0].as_ref().unwrap_err();
        assert_eq!(error.kind(), ProviderErrorKind::Transport);
        assert!(
            error
                .to_string()
                .ends_with("three (gave up after 3 attempts with backoff)"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn non_transient_failures_are_never_retried() {
        for error in [
            ProviderError::Api {
                status: 401,
                message: "denied".to_owned(),
            },
            ProviderError::Protocol("garbage".to_owned()),
            ProviderError::Configuration("bad".to_owned()),
        ] {
            let kind = error.kind();
            let (stream, calls) = scripted(fast(4), vec![vec![Err(error)], vec![text("no")]]);
            let events: Vec<_> = stream.collect().await;
            assert_eq!(
                calls.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "{kind:?}"
            );
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].as_ref().unwrap_err().kind(), kind);
        }
    }

    #[tokio::test]
    async fn a_disabled_policy_makes_exactly_one_attempt() {
        let (stream, calls) = scripted(
            AttemptPolicy::disabled(),
            vec![
                vec![Err(ProviderError::Transport("once".to_owned()))],
                vec![text("no")],
            ],
        );
        let events: Vec<_> = stream.collect().await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].as_ref().unwrap_err().to_string(),
            "provider request failed: once",
            "a single attempt carries no attempt suffix"
        );
    }
}
