use std::{net::IpAddr, pin::Pin, sync::Arc, time::Duration};

use futures_core::Stream;
use futures_util::StreamExt;
use reqwest::{
    StatusCode, Url,
    header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, RETRY_AFTER},
};
use tokio::time::Instant;

use crate::{
    ProviderError, limits::ByteCounter, request_auth::RequestAuthorizer, sanitize::sanitize_message,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(300);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const ERROR_BODY_BYTES_LIMIT: usize = 16 * 1_024;
const DEFAULT_ATTEMPTS: u32 = 4;
const DEFAULT_BASE_DELAY: Duration = Duration::from_millis(500);
const DEFAULT_MAX_DELAY: Duration = Duration::from_secs(8);
const DEFAULT_TOTAL_BUDGET: Duration = Duration::from_secs(30);

/// The single retry owner for one compiled provider.
///
/// Every send a logical request may cost — a rejected or failed HTTP exchange
/// before any body arrives, or an SSE stream that ends or fails before it has
/// yielded one semantic event — draws from the same attempt count and time
/// budget. Once a stream has yielded an event the request is never resent:
/// a retry could duplicate output the caller has already seen. Nothing above
/// the provider retries a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttemptPolicy {
    max_attempts: u32,
    base_delay: Duration,
    max_delay: Duration,
    total_budget: Duration,
}

impl Default for AttemptPolicy {
    /// Four attempts with 500 ms → 8 s full-jitter exponential backoff inside
    /// a 30 s budget: enough to ride out an overload blip without holding a
    /// turn open for minutes.
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_ATTEMPTS,
            base_delay: DEFAULT_BASE_DELAY,
            max_delay: DEFAULT_MAX_DELAY,
            total_budget: DEFAULT_TOTAL_BUDGET,
        }
    }
}

impl AttemptPolicy {
    /// Single attempt, no sleeps. For live canaries and single-shot tests.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            max_attempts: 1,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            total_budget: Duration::ZERO,
        }
    }

    /// Bounded attempts with the given backoff. `max_attempts` is clamped to
    /// at least one; the cap is clamped to at least the base.
    #[must_use]
    pub fn new(
        max_attempts: u32,
        base_delay: Duration,
        max_delay: Duration,
        total_budget: Duration,
    ) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            base_delay,
            max_delay: max_delay.max(base_delay),
            total_budget,
        }
    }

    #[must_use]
    pub const fn max_attempts(self) -> u32 {
        self.max_attempts
    }

    #[must_use]
    pub const fn base_delay(self) -> Duration {
        self.base_delay
    }

    #[must_use]
    pub const fn max_delay(self) -> Duration {
        self.max_delay
    }

    #[must_use]
    pub const fn total_budget(self) -> Duration {
        self.total_budget
    }

    /// Whether another send may run after `completed_attempts` finished tries.
    pub(crate) const fn has_remaining_attempt(self, completed_attempts: u32) -> bool {
        completed_attempts < self.max_attempts
    }

    /// Exponential delay for the retry about to start.
    ///
    /// `retry_index` is zero for the first retry after the initial try.
    pub(crate) fn exponential_delay(self, retry_index: u32) -> Duration {
        let multiplier = 1u32.checked_shl(retry_index.min(31)).unwrap_or(u32::MAX);
        self.base_delay
            .saturating_mul(multiplier)
            .min(self.max_delay)
    }

    /// Delay before the next attempt, honoring `Retry-After` when present.
    pub(crate) fn delay_before_retry(
        self,
        retry_index: u32,
        retry_after: Option<Duration>,
    ) -> Duration {
        let exponential = self.exponential_delay(retry_index);
        match retry_after {
            Some(retry_after) => exponential.max(retry_after).min(self.max_delay),
            None => exponential,
        }
    }

    /// Returns whether sleeping `delay` still fits inside the total budget.
    pub(crate) fn can_afford(self, elapsed: Duration, delay: Duration) -> bool {
        elapsed
            .checked_add(delay)
            .is_some_and(|total| total <= self.total_budget)
    }
}

/// The attempts one logical request has spent, shared between the HTTP
/// exchange (pre-body retries) and the stream driver (pre-first-event
/// restarts) so their sum never exceeds the policy.
#[derive(Debug)]
pub(crate) struct AttemptLedger {
    policy: AttemptPolicy,
    started: Instant,
    attempts: u32,
}

impl AttemptLedger {
    pub(crate) fn new(policy: AttemptPolicy) -> Self {
        Self {
            policy,
            started: Instant::now(),
            attempts: 0,
        }
    }

    /// Records one send about to happen and returns its ordinal (1-based).
    pub(crate) fn begin_attempt(&mut self) -> u32 {
        self.attempts = self.attempts.saturating_add(1);
        self.attempts
    }

    pub(crate) const fn attempts(&self) -> u32 {
        self.attempts
    }

    /// The delay to sleep before another attempt, or `None` when the policy
    /// has no attempt or time left. `retry_after` comes from the provider.
    pub(crate) fn next_delay(&self, retry_after: Option<Duration>) -> Option<Duration> {
        if !self.policy.has_remaining_attempt(self.attempts) {
            return None;
        }
        let retry_index = self.attempts.saturating_sub(1);
        let delay = full_jitter(
            self.policy.delay_before_retry(retry_index, retry_after),
            random_u32(),
        );
        self.policy
            .can_afford(self.started.elapsed(), delay)
            .then_some(delay)
    }
}

/// The attempt ledger shared between the exchange and the stream restart
/// loop. Both touch it only between polls of one stream, so the lock is
/// never contended and never held across an await.
#[derive(Debug, Clone)]
pub(crate) struct SharedLedger(Arc<std::sync::Mutex<AttemptLedger>>);

impl SharedLedger {
    pub(crate) fn new(policy: AttemptPolicy) -> Self {
        Self(Arc::new(std::sync::Mutex::new(AttemptLedger::new(policy))))
    }

    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, AttemptLedger> {
        // A poisoned lock means a panic already unwound through this task's
        // stream; the count it holds is still the right one to report.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Appends the attempt count to a failure the policy gave up on, so the
/// outcome a caller records shows how many sends the turn cost.
pub(crate) fn note_attempts(error: ProviderError, attempts: u32) -> ProviderError {
    if attempts <= 1 {
        return error;
    }
    let suffix = format!(" (gave up after {attempts} attempts with backoff)");
    match error {
        ProviderError::Configuration(message) => ProviderError::Configuration(message + &suffix),
        ProviderError::CredentialsUnavailable(message) => {
            ProviderError::CredentialsUnavailable(message + &suffix)
        }
        ProviderError::Transport(message) => ProviderError::Transport(message + &suffix),
        ProviderError::Api { status, message } => ProviderError::Api {
            status,
            message: message + &suffix,
        },
        ProviderError::ResponseFailed { kind, message } => ProviderError::ResponseFailed {
            kind,
            message: message + &suffix,
        },
        ProviderError::ResponseIncomplete(message) => {
            ProviderError::ResponseIncomplete(message + &suffix)
        }
        ProviderError::Protocol(message) => ProviderError::Protocol(message + &suffix),
    }
}

/// Pre-stream statuses that may be retried by [`HttpExchange`].
pub(crate) fn is_retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

/// Parses `Retry-After` delta-seconds. HTTP-date forms are ignored.
pub(crate) fn retry_after_delay(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    let seconds = value.parse::<u64>().ok()?;
    Some(Duration::from_secs(seconds))
}

/// Full jitter over `0..=delay` from a uniform 32-bit random value.
pub(crate) fn full_jitter(delay: Duration, random: u32) -> Duration {
    if delay.is_zero() {
        return Duration::ZERO;
    }
    let nanos = delay.as_nanos();
    let span = nanos.saturating_add(1);
    let pick = (span * u128::from(random)) >> 32;
    Duration::from_nanos(u64::try_from(pick).unwrap_or(u64::MAX))
}

fn random_u32() -> u32 {
    getrandom::u32().unwrap_or_else(|_| {
        // Fall back to a process-wide xorshift seeded from the wall clock if
        // the OS RNG is unavailable. Retry jitter only needs to break
        // synchronized stampedes, and successive draws must differ.
        use std::sync::atomic::{AtomicU64, Ordering};
        static STATE: AtomicU64 = AtomicU64::new(0);
        let mut state = STATE.load(Ordering::Relaxed);
        if state == 0 {
            state = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0x9E37_79B9_7F4A_7C15, |since| since.as_nanos() as u64)
                | 1;
        }
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        STATE.store(state, Ordering::Relaxed);
        (state >> 32) as u32
    })
}

pub(crate) struct SafeHeaders {
    headers: HeaderMap,
    protocol_owned: Vec<HeaderName>,
    redactions: Vec<String>,
}

impl SafeHeaders {
    pub(crate) fn new(protocol_owned: impl IntoIterator<Item = HeaderName>) -> Self {
        Self {
            headers: HeaderMap::new(),
            protocol_owned: protocol_owned.into_iter().collect(),
            redactions: Vec::new(),
        }
    }

    pub(crate) fn insert_configured(
        &mut self,
        configured: impl IntoIterator<Item = (String, String)>,
        redact_whitespace_only: bool,
    ) -> Result<(), ProviderError> {
        for (name, value) in configured {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                ProviderError::Configuration("static header name is invalid".to_owned())
            })?;
            if is_request_controlled_header(&name) || self.protocol_owned.contains(&name) {
                return Err(ProviderError::Configuration(format!(
                    "static header `{name}` is controlled by the provider"
                )));
            }
            if self.headers.contains_key(&name) {
                return Err(ProviderError::Configuration(format!(
                    "static header `{name}` is duplicated"
                )));
            }

            let mut header_value = HeaderValue::from_str(&value).map_err(|_| {
                ProviderError::Configuration("static header value is invalid".to_owned())
            })?;
            header_value.set_sensitive(true);
            if if redact_whitespace_only {
                !value.is_empty()
            } else {
                !value.trim().is_empty()
            } {
                self.redactions.push(value);
            }
            self.headers.insert(name, header_value);
        }
        Ok(())
    }

    pub(crate) fn insert_owned(&mut self, name: HeaderName, value: HeaderValue) {
        self.headers.insert(name, value);
    }

    pub(crate) fn extend_owned(&mut self, headers: HeaderMap) {
        self.headers.extend(headers);
    }

    pub(crate) fn push_redaction(&mut self, value: String) {
        self.redactions.push(value);
    }

    pub(crate) fn finish(mut self) -> (HeaderMap, Vec<String>) {
        normalize_redactions(&mut self.redactions);
        (self.headers, self.redactions)
    }
}

pub(crate) fn is_request_controlled_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "accept"
            | "connection"
            | "content-length"
            | "content-type"
            | "expect"
            | "host"
            | "http2-settings"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "user-agent"
    )
}

#[derive(Clone)]
pub(crate) struct HttpExchange {
    client: reqwest::Client,
    authorizer: RequestAuthorizer,
    redactions: Arc<[String]>,
    attempts: AttemptPolicy,
}

pub(crate) struct ExchangeMessages {
    pub(crate) wire_overflow: &'static str,
    pub(crate) wire_limit: &'static str,
}

pub(crate) enum ExchangeOutcome {
    Success(HttpResponse),
    Rejected(HttpRejection),
}

pub(crate) struct HttpResponse {
    headers: HeaderMap,
    redactions: Arc<[String]>,
    body: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, ProviderError>> + Send>>,
}

pub(crate) struct HttpRejection {
    status: StatusCode,
    body: Vec<u8>,
    redactions: Arc<[String]>,
    /// Sends spent before this rejection was final, for the failure message.
    attempts: u32,
}

impl HttpExchange {
    /// The static redactions are normalized here, once, so an attempt whose
    /// authorizer adds nothing can share the list as-is.
    pub(crate) fn new(
        client: reqwest::Client,
        authorizer: RequestAuthorizer,
        redactions: Arc<[String]>,
    ) -> Self {
        let mut normalized = redactions.to_vec();
        normalize_redactions(&mut normalized);
        Self {
            client,
            authorizer,
            redactions: Arc::from(normalized),
            attempts: AttemptPolicy::default(),
        }
    }

    pub(crate) fn set_attempt_policy(&mut self, attempts: AttemptPolicy) {
        self.attempts = attempts;
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_attempt_policy(mut self, attempts: AttemptPolicy) -> Self {
        self.attempts = attempts;
        self
    }

    /// `execute` under a fresh ledger for this exchange's policy.
    #[cfg(test)]
    pub(crate) async fn execute_fresh(
        &self,
        request: reqwest::Request,
        wire_limit: usize,
        messages: ExchangeMessages,
    ) -> Result<ExchangeOutcome, ProviderError> {
        self.execute(request, wire_limit, messages, &self.ledger())
            .await
    }

    /// A fresh ledger for one logical request under this exchange's policy.
    pub(crate) fn ledger(&self) -> SharedLedger {
        SharedLedger::new(self.attempts)
    }

    pub(crate) fn request(&self, method: reqwest::Method, url: Url) -> reqwest::RequestBuilder {
        self.client.request(method, url)
    }

    pub(crate) fn static_redactions(&self) -> &[String] {
        &self.redactions
    }

    /// Sends `request`, retrying pre-body failures (transport errors and
    /// retryable statuses) against `ledger`. The ledger outlives this call
    /// so a stream restart after a success here draws from the same count.
    pub(crate) async fn execute(
        &self,
        request: reqwest::Request,
        wire_limit: usize,
        messages: ExchangeMessages,
        ledger: &SharedLedger,
    ) -> Result<ExchangeOutcome, ProviderError> {
        let mut request = request;

        loop {
            let attempts = ledger.lock().begin_attempt();
            // Clone before consuming the request so a later attempt can resend.
            // Streaming bodies that cannot be cloned stay single-shot.
            let next_request = if self.attempts.has_remaining_attempt(attempts) {
                request.try_clone()
            } else {
                None
            };

            // Static redactions are already normalized; only a request-time
            // credential adds to them, and most authorizers add nothing.
            let dynamic = self.authorizer.authorize(&mut request).await?;
            let redactions: Arc<[String]> = if dynamic.is_empty() {
                Arc::clone(&self.redactions)
            } else {
                let mut merged = self.redactions.as_ref().to_vec();
                merged.extend(dynamic);
                normalize_redactions(&mut merged);
                Arc::from(merged)
            };

            let response = match self.client.execute(request).await {
                Ok(response) => response,
                Err(error) => {
                    let transport = transport_error(error, redactions.as_ref());
                    let Some((next, delay)) = next_request.zip(ledger.lock().next_delay(None))
                    else {
                        return Err(note_attempts(transport, ledger.lock().attempts()));
                    };
                    tokio::time::sleep(delay).await;
                    request = next;
                    continue;
                }
            };

            let status = response.status();
            if status.is_success() {
                let headers = response.headers().clone();
                let chunks = response.bytes_stream();
                let body_redactions = Arc::clone(&redactions);
                let body = Box::pin(async_stream::try_stream! {
                    let mut chunks = chunks;
                    let mut wire_bytes = ByteCounter::new(
                        wire_limit,
                        messages.wire_overflow,
                        messages.wire_limit,
                    );
                    while let Some(chunk) = chunks.next().await {
                        let chunk = chunk
                            .map_err(|error| transport_error(error, body_redactions.as_ref()))?;
                        wire_bytes.add(chunk.len())?;
                        yield chunk;
                    }
                });

                return Ok(ExchangeOutcome::Success(HttpResponse {
                    headers,
                    redactions,
                    body,
                }));
            }

            let retry_after = retry_after_delay(response.headers());
            let body = read_error_body(response).await;
            let rejection = HttpRejection {
                status,
                body,
                redactions,
                attempts: ledger.lock().attempts(),
            };

            if !is_retryable_status(status) {
                return Ok(ExchangeOutcome::Rejected(rejection));
            }

            let Some((next, delay)) = next_request.zip(ledger.lock().next_delay(retry_after))
            else {
                return Ok(ExchangeOutcome::Rejected(rejection));
            };

            tokio::time::sleep(delay).await;
            request = next;
        }
    }
}

impl HttpResponse {
    pub(crate) fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub(crate) fn redactions(&self) -> &[String] {
        &self.redactions
    }

    pub(crate) fn into_body(
        self,
    ) -> Pin<Box<dyn Stream<Item = Result<bytes::Bytes, ProviderError>> + Send>> {
        self.body
    }
}

impl HttpRejection {
    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }

    pub(crate) fn body(&self) -> &[u8] {
        &self.body
    }

    pub(crate) fn redactions(&self) -> &[String] {
        &self.redactions
    }

    pub(crate) const fn attempts(&self) -> u32 {
        self.attempts
    }
}

fn normalize_redactions(redactions: &mut Vec<String>) {
    redactions.retain(|redaction| !redaction.is_empty());
    redactions.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    redactions.dedup();
}

pub(crate) fn build_client() -> Result<reqwest::Client, ProviderError> {
    client_builder()
        .build()
        .map_err(|error| ProviderError::Configuration(error.to_string()))
}

pub(crate) fn build_direct_client() -> Result<reqwest::Client, ProviderError> {
    client_builder()
        .no_proxy()
        .build()
        .map_err(|error| ProviderError::Configuration(error.to_string()))
}

fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .use_rustls_tls()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("qq/", env!("CARGO_PKG_VERSION")))
}

pub(crate) fn transport_error(error: reqwest::Error, redactions: &[String]) -> ProviderError {
    ProviderError::Transport(sanitize_message(&describe_transport(&error), redactions))
}

/// reqwest's `Display` prints only the error kind and drops the cause chain,
/// and `bytes_stream` files every body read failure — peer reset, truncated
/// framing, idle timeout — under one kind that renders as "error decoding
/// response body". Name the phase ourselves and append the causes so the
/// message says what happened. The URL is left out: it can carry credentials
/// and the provider is already named by the caller.
fn describe_transport(error: &reqwest::Error) -> String {
    let body_phase = error.is_body() || error.is_decode();
    if error.is_timeout() {
        // The only cause below a timeout is reqwest's own "operation timed
        // out"; the phase says more than the chain would.
        return if body_phase {
            format!(
                "timed out waiting for response body (no bytes for {}s, or request past {}s)",
                READ_TIMEOUT.as_secs(),
                REQUEST_TIMEOUT.as_secs()
            )
        } else if error.is_connect() {
            format!("timed out connecting after {}s", CONNECT_TIMEOUT.as_secs())
        } else {
            "timed out waiting for response headers".to_owned()
        };
    }

    let mut message = String::from(if error.is_connect() {
        "connection failed"
    } else if body_phase {
        "response body failed"
    } else if error.is_redirect() {
        "redirect failed"
    } else if error.is_builder() {
        "request build failed"
    } else {
        "request failed"
    });
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        // Nested reqwest errors (the body wrapper under a `Decode`) would only
        // add another kind label.
        if !cause.is::<reqwest::Error>() {
            let text = cause.to_string();
            if !text.is_empty() && !message.ends_with(&text) {
                message.push_str(": ");
                message.push_str(&text);
            }
        }
        source = cause.source();
    }
    message
}

pub(crate) fn is_event_stream_headers(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(';').next().is_some_and(|media_type| {
                media_type.trim().eq_ignore_ascii_case("text/event-stream")
            })
        })
}

async fn read_error_body(response: reqwest::Response) -> Vec<u8> {
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();

    while let Some(chunk) = chunks.next().await {
        let Ok(chunk) = chunk else {
            break;
        };
        let remaining = ERROR_BODY_BYTES_LIMIT.saturating_sub(body.len());
        if remaining == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if body.len() == ERROR_BODY_BYTES_LIMIT {
            break;
        }
    }

    body
}

pub(crate) fn validate_endpoint(endpoint: &str, allow_http: bool) -> Result<Url, ProviderError> {
    let url = Url::parse(endpoint).map_err(|_| {
        ProviderError::Configuration("endpoint must be a valid absolute URL".to_owned())
    })?;

    if url.fragment().is_some() {
        return Err(ProviderError::Configuration(
            "endpoint URL must not contain a fragment".to_owned(),
        ));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || endpoint_authority_contains_at_sign(endpoint)
    {
        return Err(ProviderError::Configuration(
            "endpoint URL must not contain user information".to_owned(),
        ));
    }
    if url.host_str().is_none() {
        return Err(ProviderError::Configuration(
            "endpoint URL must contain a host".to_owned(),
        ));
    }

    match url.scheme() {
        "https" => Ok(url),
        "http" if allow_http && is_loopback_host(&url) => Ok(url),
        "http" => Err(ProviderError::Configuration(
            "plain HTTP is allowed only for explicitly enabled loopback endpoints".to_owned(),
        )),
        _ => Err(ProviderError::Configuration(
            "endpoint URL must use HTTPS".to_owned(),
        )),
    }
}

fn endpoint_authority_contains_at_sign(endpoint: &str) -> bool {
    let Some((_, remainder)) = endpoint.split_once("://") else {
        return false;
    };
    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    remainder[..authority_end].contains('@')
}

fn is_loopback_host(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }

    let address = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    address
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering},
        thread::{self, JoinHandle},
    };

    use crate::request_auth::{
        RequestCredential, RequestCredentialError, RequestCredentialProvider,
        SharedRequestCredentialProvider,
    };

    use super::*;

    #[test]
    fn default_attempt_policy_matches_documented_constants() {
        let policy = AttemptPolicy::default();
        assert_eq!(policy.max_attempts(), 4);
        assert_eq!(policy.base_delay(), Duration::from_millis(500));
        assert_eq!(policy.max_delay(), Duration::from_secs(8));
        assert_eq!(policy.total_budget(), Duration::from_secs(30));
        assert!(policy.has_remaining_attempt(0));
        assert!(policy.has_remaining_attempt(3));
        assert!(!policy.has_remaining_attempt(4));
    }

    #[test]
    fn disabled_attempt_policy_allows_one_attempt() {
        let policy = AttemptPolicy::disabled();
        assert_eq!(policy.max_attempts(), 1);
        assert!(policy.has_remaining_attempt(0));
        assert!(!policy.has_remaining_attempt(1));
        assert_eq!(policy.exponential_delay(0), Duration::ZERO);
        assert!(!policy.can_afford(Duration::ZERO, Duration::from_millis(1)));
    }

    #[test]
    fn retryable_status_classification_matches_policy() {
        for status in [
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
        ] {
            assert!(
                is_retryable_status(status),
                "{status} should be retryable pre-stream"
            );
        }

        for status in [
            StatusCode::OK,
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::CONFLICT,
            StatusCode::UNPROCESSABLE_ENTITY,
            StatusCode::NOT_IMPLEMENTED,
        ] {
            assert!(!is_retryable_status(status), "{status} must not be retried");
        }
    }

    #[test]
    fn retry_after_parses_delta_seconds_and_ignores_invalid_values() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("2"));
        assert_eq!(retry_after_delay(&headers), Some(Duration::from_secs(2)));

        headers.insert(RETRY_AFTER, HeaderValue::from_static(" 7 "));
        assert_eq!(retry_after_delay(&headers), Some(Duration::from_secs(7)));

        headers.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        assert_eq!(retry_after_delay(&headers), None);

        headers.insert(RETRY_AFTER, HeaderValue::from_static("soon"));
        assert_eq!(retry_after_delay(&headers), None);

        assert_eq!(retry_after_delay(&HeaderMap::new()), None);
    }

    #[test]
    fn exponential_delays_double_then_cap_at_max_delay() {
        let policy = AttemptPolicy::default();
        assert_eq!(policy.exponential_delay(0), Duration::from_millis(500));
        assert_eq!(policy.exponential_delay(1), Duration::from_secs(1));
        assert_eq!(policy.exponential_delay(2), Duration::from_secs(2));
        assert_eq!(policy.exponential_delay(3), Duration::from_secs(4));
        assert_eq!(policy.exponential_delay(4), Duration::from_secs(8));
        assert_eq!(policy.exponential_delay(5), Duration::from_secs(8));
        assert_eq!(policy.exponential_delay(31), Duration::from_secs(8));
    }

    #[test]
    fn delay_before_retry_honors_retry_after_then_caps() {
        let policy = AttemptPolicy::default();
        assert_eq!(
            policy.delay_before_retry(0, None),
            Duration::from_millis(500)
        );
        assert_eq!(
            policy.delay_before_retry(0, Some(Duration::from_secs(2))),
            Duration::from_secs(2)
        );
        assert_eq!(
            policy.delay_before_retry(0, Some(Duration::from_millis(100))),
            Duration::from_millis(500)
        );
        assert_eq!(
            policy.delay_before_retry(0, Some(Duration::from_secs(30))),
            Duration::from_secs(8)
        );
    }

    #[test]
    fn budget_helper_rejects_unaffordable_sleeps() {
        let policy = AttemptPolicy::default();
        assert!(policy.can_afford(Duration::ZERO, Duration::from_secs(30)));
        assert!(!policy.can_afford(
            Duration::ZERO,
            Duration::from_secs(30) + Duration::from_nanos(1)
        ));
        assert!(policy.can_afford(Duration::from_secs(29), Duration::from_secs(1)));
        assert!(!policy.can_afford(
            Duration::from_secs(29),
            Duration::from_secs(1) + Duration::from_nanos(1)
        ));
        assert!(!policy.can_afford(Duration::from_secs(10), Duration::MAX));
    }

    #[test]
    fn full_jitter_spans_zero_through_delay() {
        let delay = Duration::from_millis(1_000);
        assert_eq!(full_jitter(delay, 0), Duration::ZERO);
        assert_eq!(full_jitter(delay, u32::MAX), delay);
        assert_eq!(full_jitter(Duration::ZERO, u32::MAX), Duration::ZERO);

        let mid = full_jitter(delay, u32::MAX / 2);
        assert!(mid > Duration::ZERO);
        assert!(mid < delay);
    }

    #[test]
    fn exchange_defaults_to_attempt_policy_and_accepts_override() {
        let exchange = default_exchange(RequestAuthorizer::default(), Vec::new());
        assert_eq!(exchange.attempts, AttemptPolicy::default());
        let disabled = exchange.with_attempt_policy(AttemptPolicy::disabled());
        assert_eq!(disabled.attempts, AttemptPolicy::disabled());
    }

    const WIRE_OVERFLOW: &str = "test wire size overflowed";
    const WIRE_LIMIT: &str = "test wire size exceeded the limit";

    fn messages() -> ExchangeMessages {
        ExchangeMessages {
            wire_overflow: WIRE_OVERFLOW,
            wire_limit: WIRE_LIMIT,
        }
    }

    fn exchange(authorizer: RequestAuthorizer, redactions: Vec<String>) -> HttpExchange {
        // Keep multi-attempt coverage fast without pausing the runtime clock;
        // paused time breaks reqwest's connect/request timers.
        exchange_with_retry(authorizer, redactions, fast_retry_policy())
    }

    fn exchange_with_retry(
        authorizer: RequestAuthorizer,
        redactions: Vec<String>,
        retry: AttemptPolicy,
    ) -> HttpExchange {
        HttpExchange::new(
            build_direct_client().unwrap(),
            authorizer,
            Arc::from(redactions),
        )
        .with_attempt_policy(retry)
    }

    fn fast_retry_policy() -> AttemptPolicy {
        AttemptPolicy::new(
            3,
            Duration::from_millis(1),
            Duration::from_millis(1),
            Duration::from_secs(1),
        )
    }

    fn default_exchange(authorizer: RequestAuthorizer, redactions: Vec<String>) -> HttpExchange {
        exchange_with_retry(authorizer, redactions, AttemptPolicy::default())
    }

    #[tokio::test]
    async fn exchange_splits_success_and_exposes_narrow_metadata() {
        let body = b"event bytes".to_vec();
        let (url, server) = serve_response(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
                body.len()
            ),
            body.clone(),
        );
        let request = build_direct_client().unwrap().get(url).build().unwrap();

        let outcome = exchange(
            RequestAuthorizer::default(),
            vec![
                "short".to_owned(),
                "longer-redaction".to_owned(),
                "short".to_owned(),
                String::new(),
            ],
        )
        .execute_fresh(request, body.len(), messages())
        .await
        .unwrap();
        let ExchangeOutcome::Success(response) = outcome else {
            panic!("successful status must produce a successful exchange");
        };

        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        assert_eq!(
            response.redactions(),
            &["longer-redaction".to_owned(), "short".to_owned()]
        );
        let streamed = response
            .into_body()
            .map(|chunk| chunk.unwrap())
            .collect::<Vec<_>>()
            .await
            .concat();
        assert_eq!(streamed, body);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn exchange_bounds_rejections_and_preserves_status() {
        let body = vec![b'x'; ERROR_BODY_BYTES_LIMIT + 1_024];
        let (url, server) = serve_response(
            format!(
                "HTTP/1.1 429 Too Many Requests\r\nContent-Length: {}\r\n\r\n",
                body.len()
            ),
            body,
        );
        let request = build_direct_client().unwrap().get(url).build().unwrap();

        // Disable retries so a single 429 fixture still bounds the body once.
        let outcome = exchange_with_retry(
            RequestAuthorizer::default(),
            Vec::new(),
            AttemptPolicy::disabled(),
        )
        .execute_fresh(request, usize::MAX, messages())
        .await
        .unwrap();
        let ExchangeOutcome::Rejected(rejection) = outcome else {
            panic!("non-success status must produce a rejection");
        };

        assert_eq!(rejection.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(rejection.body().len(), ERROR_BODY_BYTES_LIMIT);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn exchange_success_body_enforces_exact_and_over_wire_limits() {
        let exact_body = b"12345".to_vec();
        let (url, server) = serve_response(
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n".to_owned(),
            exact_body.clone(),
        );
        let request = build_direct_client().unwrap().get(url).build().unwrap();
        let ExchangeOutcome::Success(response) = exchange(RequestAuthorizer::default(), Vec::new())
            .execute_fresh(request, exact_body.len(), messages())
            .await
            .unwrap()
        else {
            panic!("expected success");
        };
        let streamed = response.into_body().collect::<Vec<_>>().await;
        assert_eq!(streamed.len(), 1);
        assert_eq!(streamed[0].as_ref().unwrap(), exact_body.as_slice());
        server.join().unwrap();

        let (url, server) = serve_response(
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n".to_owned(),
            exact_body,
        );
        let request = build_direct_client().unwrap().get(url).build().unwrap();
        let ExchangeOutcome::Success(response) = exchange(RequestAuthorizer::default(), Vec::new())
            .execute_fresh(request, 4, messages())
            .await
            .unwrap()
        else {
            panic!("expected success");
        };
        let error = response.into_body().next().await.unwrap().unwrap_err();
        assert_eq!(
            error.to_string(),
            "provider stream was invalid: test wire size exceeded the limit"
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn exchange_authorizes_immediately_before_send_and_normalizes_redactions() {
        let secret = "dynamic-bearer-secret";
        let provider = SharedRequestCredentialProvider::new(StaticBearer(secret.to_owned()));
        let authorizer = RequestAuthorizer::request_time_bearer(provider);
        let (url, server) = serve_authorized_rejection(secret);
        let request = build_direct_client().unwrap().get(url).build().unwrap();

        let outcome = exchange(
            authorizer,
            vec!["short".to_owned(), secret.to_owned(), "short".to_owned()],
        )
        .execute_fresh(request, usize::MAX, messages())
        .await
        .unwrap();
        let ExchangeOutcome::Rejected(rejection) = outcome else {
            panic!("expected rejection");
        };

        assert_eq!(rejection.body(), b"rejected");
        assert_eq!(
            rejection.redactions(),
            &[secret.to_owned(), "short".to_owned()]
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn exchange_body_read_errors_redact_dynamic_credentials() {
        let secret = "dynamic-body-read-secret";
        let provider = SharedRequestCredentialProvider::new(StaticBearer(secret.to_owned()));
        let authorizer = RequestAuthorizer::request_time_bearer(provider);
        let (url, server) = serve_authorized_malformed_body(secret);
        let request = build_direct_client()
            .unwrap()
            .get(format!("{url}?credential={secret}"))
            .build()
            .unwrap();

        let ExchangeOutcome::Success(response) = exchange(authorizer, Vec::new())
            .execute_fresh(request, usize::MAX, messages())
            .await
            .unwrap()
        else {
            panic!("expected successful response metadata");
        };
        let error = response.into_body().next().await.unwrap().unwrap_err();
        let rendered = error.to_string();

        assert!(!rendered.contains(secret));
        assert!(!rendered.contains("credential="));
        assert!(!rendered.contains('\n'));
        server.join().unwrap();
    }

    struct StaticBearer(String);

    impl RequestCredentialProvider for StaticBearer {
        fn credential(
            &self,
        ) -> Pin<
            Box<dyn Future<Output = Result<RequestCredential, RequestCredentialError>> + Send + '_>,
        > {
            Box::pin(async { RequestCredential::bearer(self.0.clone()) })
        }
    }

    #[tokio::test]
    async fn error_body_stops_at_exactly_sixteen_kibibytes() {
        let body = vec![b'x'; ERROR_BODY_BYTES_LIMIT + 1_024];
        let (url, server) = serve_response(
            format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\n\r\n",
                body.len()
            ),
            body,
        );
        let response = build_direct_client()
            .unwrap()
            .get(url)
            .send()
            .await
            .unwrap();

        let body = read_error_body(response).await;

        assert_eq!(body.len(), ERROR_BODY_BYTES_LIMIT);
        assert!(body.iter().all(|byte| *byte == b'x'));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn error_body_preserves_partial_bytes_when_the_read_fails() {
        let partial = b"partial provider error".to_vec();
        let (url, server) = serve_response(
            "HTTP/1.1 400 Bad Request\r\nContent-Length: 1024\r\n\r\n".to_owned(),
            partial.clone(),
        );
        let response = build_direct_client()
            .unwrap()
            .get(url)
            .send()
            .await
            .unwrap();

        let body = read_error_body(response).await;

        assert_eq!(body, partial);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn successful_response_streams_all_wire_bytes() {
        let body = b"first second third".to_vec();
        let (url, server) = serve_response(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
                body.len()
            ),
            body.clone(),
        );
        let response = build_direct_client()
            .unwrap()
            .get(url)
            .send()
            .await
            .unwrap();
        assert!(is_event_stream_headers(response.headers()));

        let streamed = response
            .bytes_stream()
            .map(|chunk| chunk.unwrap())
            .collect::<Vec<_>>()
            .await
            .concat();

        assert_eq!(streamed, body);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn body_read_transport_errors_are_url_free_and_redacted() {
        let secret = "body-read-test-secret";
        let (url, server) = serve_response(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_owned(),
            format!("{secret}\r\n").into_bytes(),
        );
        let response = build_direct_client()
            .unwrap()
            .get(format!("{url}?credential={secret}"))
            .send()
            .await
            .unwrap();
        let error = response
            .bytes_stream()
            .next()
            .await
            .expect("malformed chunk framing must produce a body item")
            .unwrap_err();

        let error = transport_error(error, &[secret.to_owned()]);
        let rendered = error.to_string();

        assert!(!rendered.contains(secret));
        assert!(!rendered.contains("credential="));
        assert!(!rendered.contains('\n'));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn stalled_body_transport_error_names_the_timeout() {
        // Regression: reqwest files a body read timeout under its `Decode`
        // kind, whose `Display` is the bare "error decoding response body".
        // A stream that returns headers and then never sends a byte must say
        // that it timed out waiting for the body.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/stalled", listener.local_addr().unwrap());
        let (release, released) = std::sync::mpsc::channel::<()>();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request_head(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
                )
                .unwrap();
            // Hold the connection open without sending until the client gave up.
            released.recv().unwrap();
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .read_timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let response = client.get(&url).send().await.unwrap();
        let error = response
            .bytes_stream()
            .next()
            .await
            .expect("a stalled body must produce an error item")
            .unwrap_err();
        assert!(error.is_timeout(), "{error:?}");

        let rendered = transport_error(error, &[]).to_string();

        assert!(
            rendered.contains("timed out waiting for response body"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("error decoding response body"),
            "{rendered}"
        );
        release.send(()).unwrap();
        server.join().unwrap();
    }

    #[tokio::test]
    async fn truncated_body_transport_error_carries_the_cause() {
        // Regression: a peer that closes mid-body must render the phase and
        // the underlying cause, not only reqwest's kind label.
        let (url, server) = serve_response(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_owned(),
            b"5\r\nhel".to_vec(),
        );
        let response = build_direct_client()
            .unwrap()
            .get(&url)
            .send()
            .await
            .unwrap();
        let mut chunks = response.bytes_stream();
        let mut error = None;
        while let Some(item) = chunks.next().await {
            if let Err(failure) = item {
                error = Some(failure);
                break;
            }
        }
        let error = error.expect("a truncated chunked body must fail");

        let rendered = transport_error(error, &[]).to_string();

        assert!(
            rendered.starts_with("provider request failed: response body failed: "),
            "{rendered}"
        );
        assert!(
            !rendered.contains("error decoding response body"),
            "{rendered}"
        );
        assert!(
            rendered.len() > "provider request failed: response body failed: ".len(),
            "cause chain missing: {rendered}"
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn exchange_retries_service_unavailable_then_succeeds() {
        let body = b"ok-after-retry".to_vec();
        let (url, hits, server) = serve_scripted(vec![
            ScriptedResponse::http(
                "HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 9\r\n\r\noverloaded",
            ),
            ScriptedResponse::http(format!(
                "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                String::from_utf8(body.clone()).unwrap()
            )),
        ]);
        let request = build_direct_client().unwrap().get(&url).build().unwrap();

        let outcome = exchange(RequestAuthorizer::default(), Vec::new())
            .execute_fresh(request, body.len(), messages())
            .await
            .unwrap();
        let ExchangeOutcome::Success(response) = outcome else {
            panic!("503 then 200 must succeed after retry");
        };
        let streamed = response
            .into_body()
            .map(|chunk| chunk.unwrap())
            .collect::<Vec<_>>()
            .await
            .concat();

        assert_eq!(streamed, body);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn exchange_does_not_retry_unauthorized() {
        let (url, hits, server) = serve_scripted(vec![ScriptedResponse::http(
            "HTTP/1.1 401 Unauthorized\r\nConnection: close\r\nContent-Length: 8\r\n\r\nrejected",
        )]);
        let request = build_direct_client().unwrap().get(&url).build().unwrap();

        let outcome = exchange(RequestAuthorizer::default(), Vec::new())
            .execute_fresh(request, usize::MAX, messages())
            .await
            .unwrap();
        let ExchangeOutcome::Rejected(rejection) = outcome else {
            panic!("401 must reject without retry");
        };

        assert_eq!(rejection.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(rejection.body(), b"rejected");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn exchange_disabled_policy_never_resends() {
        let (url, hits, server) = serve_scripted(vec![ScriptedResponse::http(
            "HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 9\r\n\r\noverloaded",
        )]);
        let request = build_direct_client().unwrap().get(&url).build().unwrap();

        let outcome = exchange_with_retry(
            RequestAuthorizer::default(),
            Vec::new(),
            AttemptPolicy::disabled(),
        )
        .execute_fresh(request, usize::MAX, messages())
        .await
        .unwrap();
        let ExchangeOutcome::Rejected(rejection) = outcome else {
            panic!("disabled policy must surface the first rejection");
        };

        assert_eq!(rejection.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn exchange_exhausts_retries_and_returns_last_rejection() {
        let (url, hits, server) = serve_scripted(vec![
            ScriptedResponse::http(
                "HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 5\r\n\r\nfirst",
            ),
            ScriptedResponse::http(
                "HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 6\r\n\r\nsecond",
            ),
            ScriptedResponse::http(
                "HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 5\r\n\r\nthird",
            ),
        ]);
        let request = build_direct_client().unwrap().get(&url).build().unwrap();

        let outcome = exchange(RequestAuthorizer::default(), Vec::new())
            .execute_fresh(request, usize::MAX, messages())
            .await
            .unwrap();
        let ExchangeOutcome::Rejected(rejection) = outcome else {
            panic!("exhausted retries must reject");
        };

        assert_eq!(rejection.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(rejection.body(), b"third");
        assert_eq!(hits.load(Ordering::SeqCst), 3);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn exchange_honors_retry_after_before_success() {
        let body = b"after-wait".to_vec();
        let (url, hits, server) = serve_scripted(vec![
            ScriptedResponse::http(
                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 2\r\nConnection: close\r\nContent-Length: 4\r\n\r\nwait",
            ),
            ScriptedResponse::http(format!(
                "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                String::from_utf8(body.clone()).unwrap()
            )),
        ]);
        let request = build_direct_client().unwrap().get(&url).build().unwrap();

        // Cap Retry-After via max_delay so the test stays fast while still
        // exercising the header parse + delay selection path.
        let policy = AttemptPolicy::new(
            3,
            Duration::from_millis(1),
            Duration::from_millis(5),
            Duration::from_secs(1),
        );
        let outcome = exchange_with_retry(RequestAuthorizer::default(), Vec::new(), policy)
            .execute_fresh(request, body.len(), messages())
            .await
            .unwrap();
        let ExchangeOutcome::Success(_) = outcome else {
            panic!("429 with Retry-After then 200 must succeed");
        };

        assert_eq!(hits.load(Ordering::SeqCst), 2);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn exchange_retries_transport_failure_then_succeeds() {
        let body = b"recovered".to_vec();
        let (url, hits, server) = serve_scripted(vec![
            ScriptedResponse::Drop,
            ScriptedResponse::http(format!(
                "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                String::from_utf8(body.clone()).unwrap()
            )),
        ]);
        let request = build_direct_client().unwrap().get(&url).build().unwrap();

        let outcome = exchange(RequestAuthorizer::default(), Vec::new())
            .execute_fresh(request, body.len(), messages())
            .await
            .unwrap();
        let ExchangeOutcome::Success(response) = outcome else {
            panic!("transport failure then 200 must succeed after retry");
        };
        let streamed = response
            .into_body()
            .map(|chunk| chunk.unwrap())
            .collect::<Vec<_>>()
            .await
            .concat();

        assert_eq!(streamed, body);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn exchange_reauthorizes_on_every_retry_attempt() {
        let secret = "retry-bearer-secret";
        let authorize_hits = Arc::new(AtomicUsize::new(0));
        let provider = SharedRequestCredentialProvider::new(CountingBearer {
            token: secret.to_owned(),
            hits: Arc::clone(&authorize_hits),
        });
        let authorizer = RequestAuthorizer::request_time_bearer(provider);
        let body = b"authorized-ok".to_vec();
        let (url, hits, server) = serve_scripted_authorized(
            secret,
            vec![
                ScriptedResponse::http(
                    "HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 9\r\n\r\noverloaded",
                ),
                ScriptedResponse::http(format!(
                    "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    String::from_utf8(body.clone()).unwrap()
                )),
            ],
        );
        let request = build_direct_client().unwrap().get(&url).build().unwrap();

        let outcome = exchange(authorizer, Vec::new())
            .execute_fresh(request, body.len(), messages())
            .await
            .unwrap();
        let ExchangeOutcome::Success(response) = outcome else {
            panic!("authorized retry must succeed");
        };

        assert_eq!(response.redactions(), &[secret.to_owned()]);
        assert_eq!(authorize_hits.load(Ordering::SeqCst), 2);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        server.join().unwrap();
    }

    struct CountingBearer {
        token: String,
        hits: Arc<AtomicUsize>,
    }

    impl RequestCredentialProvider for CountingBearer {
        fn credential(
            &self,
        ) -> Pin<
            Box<dyn Future<Output = Result<RequestCredential, RequestCredentialError>> + Send + '_>,
        > {
            Box::pin(async {
                self.hits.fetch_add(1, Ordering::SeqCst);
                RequestCredential::bearer(self.token.clone())
            })
        }
    }

    fn serve_response(headers: String, body: Vec<u8>) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/response", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request_head(&mut stream);
            stream.write_all(headers.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
        });
        (url, server)
    }

    enum ScriptedResponse {
        Http(Vec<u8>),
        Drop,
    }

    impl ScriptedResponse {
        fn http(response: impl Into<Vec<u8>>) -> Self {
            Self::Http(response.into())
        }
    }

    fn serve_scripted(
        responses: Vec<ScriptedResponse>,
    ) -> (String, Arc<AtomicUsize>, JoinHandle<()>) {
        serve_scripted_with_auth(None, responses)
    }

    fn serve_scripted_authorized(
        secret: &str,
        responses: Vec<ScriptedResponse>,
    ) -> (String, Arc<AtomicUsize>, JoinHandle<()>) {
        serve_scripted_with_auth(Some(secret.to_owned()), responses)
    }

    fn serve_scripted_with_auth(
        expected_bearer: Option<String>,
        responses: Vec<ScriptedResponse>,
    ) -> (String, Arc<AtomicUsize>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/response", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let server_hits = Arc::clone(&hits);
        let server = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request_head(&mut stream);
                server_hits.fetch_add(1, Ordering::SeqCst);
                if let Some(secret) = expected_bearer.as_ref() {
                    let expected =
                        format!("authorization: Bearer {secret}\r\n").to_ascii_lowercase();
                    assert!(
                        String::from_utf8_lossy(&request)
                            .to_ascii_lowercase()
                            .contains(&expected),
                        "missing bearer authorization on attempt"
                    );
                }
                match response {
                    ScriptedResponse::Http(bytes) => {
                        stream.write_all(&bytes).unwrap();
                    }
                    ScriptedResponse::Drop => {
                        drop(stream);
                    }
                }
            }
        });
        (url, hits, server)
    }

    fn serve_authorized_rejection(secret: &str) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/response", listener.local_addr().unwrap());
        let expected = format!("authorization: Bearer {secret}\r\n").to_ascii_lowercase();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request_head(&mut stream);
            assert!(
                String::from_utf8_lossy(&request)
                    .to_ascii_lowercase()
                    .contains(&expected)
            );
            stream
                .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 8\r\n\r\nrejected")
                .unwrap();
        });
        (url, server)
    }

    fn serve_authorized_malformed_body(secret: &str) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/response", listener.local_addr().unwrap());
        let expected = format!("authorization: Bearer {secret}\r\n").to_ascii_lowercase();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request_head(&mut stream);
            assert!(
                String::from_utf8_lossy(&request)
                    .to_ascii_lowercase()
                    .contains(&expected)
            );
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\ninvalid\r\n")
                .unwrap();
        });
        (url, server)
    }

    fn read_request_head(stream: &mut TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1_024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
        }
        request
    }
}
