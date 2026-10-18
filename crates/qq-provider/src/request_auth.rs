//! Request-time HTTP authorization.

use std::{future::Future, pin::Pin, sync::Arc};

use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};

#[cfg(feature = "provider-bedrock")]
use crate::aws::{AwsCredentialLease, SigV4Authorizer};
use crate::{ProviderError, ProviderErrorKind};

pub type RequestCredentialFuture<'a> =
    Pin<Box<dyn Future<Output = Result<RequestCredential, RequestCredentialError>> + Send + 'a>>;

pub trait RequestCredentialProvider: Send + Sync {
    fn credential(&self) -> RequestCredentialFuture<'_>;
}

#[derive(Clone)]
pub struct SharedRequestCredentialProvider(Arc<dyn RequestCredentialProvider>);

impl SharedRequestCredentialProvider {
    pub fn new(provider: impl RequestCredentialProvider + 'static) -> Self {
        Self(Arc::new(provider))
    }
}

impl RequestCredentialProvider for SharedRequestCredentialProvider {
    fn credential(&self) -> RequestCredentialFuture<'_> {
        self.0.credential()
    }
}

impl std::fmt::Debug for SharedRequestCredentialProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SharedRequestCredentialProvider([REDACTED])")
    }
}

impl PartialEq for SharedRequestCredentialProvider {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for SharedRequestCredentialProvider {}

/// Most headers one request-time credential may set. Codex, the largest
/// built-in intent, sets four.
const MAX_CREDENTIAL_HEADERS: usize = 8;
/// Most header-value bytes one request-time credential may set.
const MAX_CREDENTIAL_HEADER_BYTES: usize = 64 * 1_024;

const CHATGPT_ACCOUNT_ID: HeaderName = HeaderName::from_static("chatgpt-account-id");
const ORIGINATOR: HeaderName = HeaderName::from_static("originator");
const X_OPENAI_FEDRAMP: HeaderName = HeaderName::from_static("x-openai-fedramp");

/// The headers one request-time credential places on an attempt.
///
/// Values are validated when the credential is built, so applying it per
/// attempt only clones prebuilt `HeaderValue`s. Each header records whether
/// its value is secret: secret values are redacted from every diagnostic the
/// attempt can produce, while non-secret metadata (`originator: qq`,
/// `x-openai-fedramp: true`) is sent but never registered for redaction,
/// which would otherwise rewrite every matching substring of a diagnostic.
#[derive(Clone)]
pub struct RequestCredential {
    headers: Vec<CredentialHeader>,
}

#[derive(Clone)]
struct CredentialHeader {
    name: HeaderName,
    value: HeaderValue,
    /// The secret part of `value`, redacted from diagnostics; `None` for
    /// non-secret metadata.
    secret: Option<String>,
}

impl RequestCredential {
    pub fn bearer(token: impl Into<String>) -> Result<Self, RequestCredentialError> {
        let token = token.into();
        let mut credential = Self::empty();
        credential.push_bearer(token)?;
        Ok(credential)
    }

    pub fn codex(
        access_token: impl Into<String>,
        account_id: impl Into<String>,
        is_fedramp: bool,
    ) -> Result<Self, RequestCredentialError> {
        let mut credential = Self::empty();
        credential.push_bearer(access_token.into())?;
        credential.push_secret(CHATGPT_ACCOUNT_ID, account_id.into())?;
        credential.push_metadata(ORIGINATOR, HeaderValue::from_static("qq"));
        if is_fedramp {
            credential.push_metadata(X_OPENAI_FEDRAMP, HeaderValue::from_static("true"));
        }
        Ok(credential)
    }

    const fn empty() -> Self {
        Self {
            headers: Vec::new(),
        }
    }

    fn push_bearer(&mut self, token: String) -> Result<(), RequestCredentialError> {
        validate_credential_value(&token)?;
        let value = sensitive_value(&format!("Bearer {token}"))?;
        self.headers.push(CredentialHeader {
            name: AUTHORIZATION,
            value,
            secret: Some(token),
        });
        Ok(())
    }

    fn push_secret(
        &mut self,
        name: HeaderName,
        secret: String,
    ) -> Result<(), RequestCredentialError> {
        validate_credential_value(&secret)?;
        let value = sensitive_value(&secret)?;
        self.headers.push(CredentialHeader {
            name,
            value,
            secret: Some(secret),
        });
        Ok(())
    }

    fn push_metadata(&mut self, name: HeaderName, value: HeaderValue) {
        self.headers.push(CredentialHeader {
            name,
            value,
            secret: None,
        });
    }

    fn has(&self, name: &HeaderName) -> bool {
        self.headers.iter().any(|header| header.name == *name)
    }
}

impl std::fmt::Debug for RequestCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RequestCredential([REDACTED])")
    }
}

fn validate_credential_value(value: &str) -> Result<(), RequestCredentialError> {
    if value.is_empty() || HeaderValue::from_str(value).is_err() {
        return Err(RequestCredentialError::Invalid);
    }
    Ok(())
}

fn sensitive_value(raw: &str) -> Result<HeaderValue, RequestCredentialError> {
    let mut value = HeaderValue::from_str(raw).map_err(|_| RequestCredentialError::Invalid)?;
    value.set_sensitive(true);
    Ok(value)
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RequestCredentialError {
    /// No credential could be loaded. `remedy` is opaque text authored by
    /// the [`RequestCredentialProvider`] (which knows the provider, profile,
    /// and how the user stores a credential); it is precomputed when the
    /// provider is constructed so the request path never formats it.
    #[error("{}", .remedy.as_deref().unwrap_or(MISSING_CREDENTIAL_MESSAGE))]
    Missing { remedy: Option<Arc<str>> },
    #[error("request credentials are invalid")]
    Invalid,
    #[error("credential refresh was rejected")]
    RefreshRejected,
    #[error("credential refresh is temporarily unavailable")]
    RefreshUnavailable,
    #[error("credential storage is unavailable")]
    StorageUnavailable,
    #[error("credential loading timed out")]
    TimedOut,
    #[error("credential loading capacity is exhausted")]
    CapacityUnavailable,
    #[error("credential loading worker stopped unexpectedly")]
    WorkerFailed,
}

const MISSING_CREDENTIAL_MESSAGE: &str = "request credentials are missing";

impl RequestCredentialError {
    /// A missing credential with no provider-specific guidance.
    pub const fn missing() -> Self {
        Self::Missing { remedy: None }
    }

    /// A missing credential whose message tells the user what to do. The
    /// text is shared, not copied, so a provider can attach the same remedy
    /// to every failure without allocating.
    pub fn missing_with_remedy(remedy: Arc<str>) -> Self {
        Self::Missing {
            remedy: Some(remedy),
        }
    }
}

/// The headers a recipe's request-time auth intent lets a credential set.
///
/// Only credential-owned names appear here: request-controlled names
/// (`http::is_request_controlled_header`) and protocol constants such as
/// `anthropic-version` are never allowed, and every allowed name is also in
/// the adapter's protected list, so a configured static header cannot carry
/// it. A credential must set every `required` name and nothing outside
/// `allowed`, or the attempt fails before any header is applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestCredentialKindExpected {
    Bearer,
    Codex,
}

impl RequestCredentialKindExpected {
    fn allowed(self) -> &'static [HeaderName] {
        static BEARER: [HeaderName; 1] = [AUTHORIZATION];
        static CODEX: [HeaderName; 4] = [
            AUTHORIZATION,
            CHATGPT_ACCOUNT_ID,
            ORIGINATOR,
            X_OPENAI_FEDRAMP,
        ];
        match self {
            Self::Bearer => &BEARER,
            Self::Codex => &CODEX,
        }
    }

    fn required(self) -> &'static [HeaderName] {
        static BEARER: [HeaderName; 1] = [AUTHORIZATION];
        static CODEX: [HeaderName; 3] = [AUTHORIZATION, CHATGPT_ACCOUNT_ID, ORIGINATOR];
        match self {
            Self::Bearer => &BEARER,
            Self::Codex => &CODEX,
        }
    }
}

/// How each attempt of a request is authorized at send time. Exactly one
/// mechanism applies to a compiled provider; static headers need none.
#[derive(Clone, Default)]
pub(crate) enum RequestAuthorizer {
    #[default]
    None,
    #[cfg(feature = "provider-bedrock")]
    SigV4(Arc<SigV4Authorizer>),
    Credential(
        SharedRequestCredentialProvider,
        RequestCredentialKindExpected,
    ),
}

impl RequestAuthorizer {
    #[cfg(feature = "provider-bedrock")]
    pub(crate) fn bedrock_mantle_sigv4(
        region: impl Into<Arc<str>>,
        credentials: AwsCredentialLease,
    ) -> Self {
        Self::SigV4(Arc::new(SigV4Authorizer::new(region, credentials)))
    }

    pub(crate) fn request_time_bearer(credentials: SharedRequestCredentialProvider) -> Self {
        Self::request_credentials(credentials, RequestCredentialKindExpected::Bearer)
    }

    pub(crate) fn request_time_codex(credentials: SharedRequestCredentialProvider) -> Self {
        Self::request_credentials(credentials, RequestCredentialKindExpected::Codex)
    }

    const fn request_credentials(
        credentials: SharedRequestCredentialProvider,
        expected: RequestCredentialKindExpected,
    ) -> Self {
        Self::Credential(credentials, expected)
    }

    #[cfg(all(test, feature = "provider-bedrock"))]
    pub(crate) fn with_sigv4_for_test(authorizer: SigV4Authorizer) -> Self {
        Self::SigV4(Arc::new(authorizer))
    }

    /// Authorizes one attempt and returns any secret bytes it placed on the
    /// request, for redaction. Empty for the static and SigV4 cases.
    pub(crate) async fn authorize(
        &self,
        request: &mut reqwest::Request,
    ) -> Result<Vec<String>, ProviderError> {
        match self {
            Self::None => Ok(Vec::new()),
            #[cfg(feature = "provider-bedrock")]
            Self::SigV4(authorizer) => {
                authorizer.sign(request).await?;
                Ok(Vec::new())
            }
            Self::Credential(provider, expected) => {
                let credential = provider
                    .0
                    .credential()
                    .await
                    .map_err(request_credential_error)?;
                apply_request_credential(request, credential, *expected)
            }
        }
    }
}

fn request_credential_error(error: RequestCredentialError) -> ProviderError {
    let kind = match error {
        // A load that timed out or found no loader capacity already waited
        // its full bound; resending would wait it again before the first
        // event, so it is terminal like a rejected credential. It stays a
        // `ResponseFailed` rather than `CredentialsUnavailable`, which means
        // "no credential configured" to the live-provider canary.
        RequestCredentialError::TimedOut
        | RequestCredentialError::CapacityUnavailable
        | RequestCredentialError::Missing { .. }
        | RequestCredentialError::Invalid
        | RequestCredentialError::RefreshRejected => ProviderErrorKind::Authentication,
        RequestCredentialError::RefreshUnavailable
        | RequestCredentialError::StorageUnavailable
        | RequestCredentialError::WorkerFailed => ProviderErrorKind::Unavailable,
    };
    ProviderError::ResponseFailed {
        kind,
        message: error.to_string(),
    }
}

fn apply_request_credential(
    request: &mut reqwest::Request,
    credential: RequestCredential,
    expected: RequestCredentialKindExpected,
) -> Result<Vec<String>, ProviderError> {
    let allowed = expected.allowed();
    let value_bytes: usize = credential
        .headers
        .iter()
        .map(|header| header.value.len())
        .sum();
    let names_allowed = credential
        .headers
        .iter()
        .all(|header| allowed.contains(&header.name));
    let unique = credential
        .headers
        .iter()
        .enumerate()
        .all(|(index, header)| {
            credential.headers[..index]
                .iter()
                .all(|earlier| earlier.name != header.name)
        });
    if credential.headers.len() > MAX_CREDENTIAL_HEADERS
        || value_bytes > MAX_CREDENTIAL_HEADER_BYTES
        || !names_allowed
        || !unique
        || !expected.required().iter().all(|name| credential.has(name))
    {
        return Err(ProviderError::Configuration(
            "request credential kind did not match configured authorization intent".to_owned(),
        ));
    }

    // Each attempt starts from a clone of the unauthorized request, so this
    // only matters for an intent's optional names: a credential that omits
    // `x-openai-fedramp` must not inherit one.
    for name in allowed {
        request.headers_mut().remove(name);
    }
    let mut redactions = Vec::new();
    for header in credential.headers {
        request.headers_mut().insert(header.name, header.value);
        if let Some(secret) = header.secret {
            redactions.push(secret);
        }
    }
    Ok(redactions)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use reqwest::header::AUTHORIZATION;

    use super::*;

    #[test]
    fn a_missing_credential_displays_its_remedy_when_one_is_attached() {
        let remedy: Arc<str> =
            Arc::from("no credential for provider `example`: run `example login`");
        let with_remedy = RequestCredentialError::missing_with_remedy(remedy.clone());
        let without_remedy = RequestCredentialError::missing();

        assert_eq!(with_remedy.to_string(), remedy.as_ref());
        assert_eq!(
            without_remedy.to_string(),
            "request credentials are missing"
        );
        assert_ne!(with_remedy, without_remedy);
    }

    #[test]
    fn a_missing_credential_is_an_authentication_failure_with_or_without_a_remedy() {
        let remedy: Arc<str> = Arc::from("no credential for provider `example`");
        for error in [
            RequestCredentialError::missing(),
            RequestCredentialError::missing_with_remedy(remedy.clone()),
        ] {
            let expected_message = error.to_string();
            let provider_error = request_credential_error(error);

            assert_eq!(provider_error.kind(), ProviderErrorKind::Authentication);
            assert!(matches!(
                &provider_error,
                ProviderError::ResponseFailed { message, .. } if *message == expected_message
            ));
        }
        assert_eq!(
            request_credential_error(RequestCredentialError::missing_with_remedy(remedy))
                .to_string(),
            "provider response failed: no credential for provider `example`"
        );
    }

    #[test]
    fn timed_out_and_exhausted_credential_loads_are_terminal_for_this_request() {
        for error in [
            RequestCredentialError::TimedOut,
            RequestCredentialError::CapacityUnavailable,
        ] {
            let message = error.to_string();
            let provider_error = request_credential_error(error);
            assert_eq!(provider_error.kind(), ProviderErrorKind::Authentication);
            assert!(matches!(
                provider_error,
                ProviderError::ResponseFailed { kind: ProviderErrorKind::Authentication, message: got }
                    if got == message
            ));
        }

        for error in [
            RequestCredentialError::WorkerFailed,
            RequestCredentialError::RefreshUnavailable,
            RequestCredentialError::StorageUnavailable,
        ] {
            assert_eq!(
                request_credential_error(error).kind(),
                ProviderErrorKind::Unavailable
            );
        }
    }

    struct MissingCredentials {
        remedy: Arc<str>,
    }

    impl RequestCredentialProvider for MissingCredentials {
        fn credential(&self) -> RequestCredentialFuture<'_> {
            let remedy = self.remedy.clone();
            Box::pin(async move { Err(RequestCredentialError::missing_with_remedy(remedy)) })
        }
    }

    #[tokio::test]
    async fn authorization_surfaces_the_provider_remedy_without_touching_the_request() {
        let authorizer = RequestAuthorizer::request_time_bearer(
            SharedRequestCredentialProvider::new(MissingCredentials {
                remedy: Arc::from("no credential for provider `example`: run `example login`"),
            }),
        );
        let mut request = reqwest::Client::new()
            .get("https://example.test")
            .build()
            .unwrap();

        let error = authorizer.authorize(&mut request).await.unwrap_err();

        assert_eq!(error.kind(), ProviderErrorKind::Authentication);
        assert_eq!(
            error.to_string(),
            "provider response failed: no credential for provider `example`: run `example login`"
        );
        assert!(request.headers().get(AUTHORIZATION).is_none());
    }

    struct RotatingRequestCredentials {
        calls: AtomicUsize,
    }

    impl RequestCredentialProvider for RotatingRequestCredentials {
        fn credential(&self) -> RequestCredentialFuture<'_> {
            let token = format!("token-{}", self.calls.fetch_add(1, Ordering::Relaxed));
            Box::pin(async move { RequestCredential::bearer(token) })
        }
    }

    #[tokio::test]
    async fn request_credentials_are_resolved_for_every_request() {
        let authorizer = RequestAuthorizer::request_time_bearer(
            SharedRequestCredentialProvider::new(RotatingRequestCredentials {
                calls: AtomicUsize::new(0),
            }),
        );
        let client = reqwest::Client::new();
        let mut first = client.get("https://example.test").build().unwrap();
        let mut second = client.get("https://example.test").build().unwrap();

        let first_redactions = authorizer.authorize(&mut first).await.unwrap();
        let second_redactions = authorizer.authorize(&mut second).await.unwrap();

        assert_eq!(first.headers()[AUTHORIZATION], "Bearer token-0");
        assert_eq!(second.headers()[AUTHORIZATION], "Bearer token-1");
        assert_eq!(first_redactions, ["token-0"]);
        assert_eq!(second_redactions, ["token-1"]);
        assert!(first.headers()[AUTHORIZATION].is_sensitive());
    }

    struct StaticWrongKindCredentials {
        credential: RequestCredential,
    }

    impl RequestCredentialProvider for StaticWrongKindCredentials {
        fn credential(&self) -> RequestCredentialFuture<'_> {
            let credential = self.credential.clone();
            Box::pin(async move { Ok(credential) })
        }
    }

    #[tokio::test]
    async fn request_credential_kind_mismatch_is_rejected_without_mutating_the_request() {
        let cases = [
            (
                RequestAuthorizer::request_time_bearer(SharedRequestCredentialProvider::new(
                    StaticWrongKindCredentials {
                        credential: RequestCredential::codex(
                            "codex-secret",
                            "codex-account",
                            false,
                        )
                        .unwrap(),
                    },
                )),
                "codex-secret",
            ),
            (
                RequestAuthorizer::request_time_codex(SharedRequestCredentialProvider::new(
                    StaticWrongKindCredentials {
                        credential: RequestCredential::bearer("bearer-secret").unwrap(),
                    },
                )),
                "bearer-secret",
            ),
        ];

        for (authorizer, secret) in cases {
            let mut request = reqwest::Client::new()
                .get("https://example.test")
                .build()
                .unwrap();
            let error = authorizer.authorize(&mut request).await.unwrap_err();

            assert!(matches!(error, ProviderError::Configuration(_)));
            let rendered = format!("{error:?} {error}");
            assert!(!rendered.contains(secret));
            assert!(request.headers().get(AUTHORIZATION).is_none());
            assert!(request.headers().get("chatgpt-account-id").is_none());
        }
    }

    fn request() -> reqwest::Request {
        reqwest::Client::new()
            .get("https://example.test")
            .build()
            .unwrap()
    }

    fn credential(headers: Vec<CredentialHeader>) -> RequestCredential {
        RequestCredential { headers }
    }

    fn header(name: &'static str, value: &'static str, secret: bool) -> CredentialHeader {
        CredentialHeader {
            name: HeaderName::from_static(name),
            value: HeaderValue::from_static(value),
            secret: secret.then(|| value.to_owned()),
        }
    }

    #[test]
    fn codex_credentials_redact_secrets_but_not_metadata() {
        for (is_fedramp, expected_fedramp) in [(true, Some("true")), (false, None)] {
            let mut request = request();
            // A header left from an earlier attempt must not survive a
            // credential that no longer sets it.
            request.headers_mut().insert(
                HeaderName::from_static("x-openai-fedramp"),
                HeaderValue::from_static("true"),
            );
            let redactions = apply_request_credential(
                &mut request,
                RequestCredential::codex("codex-token", "codex-account", is_fedramp).unwrap(),
                RequestCredentialKindExpected::Codex,
            )
            .unwrap();

            assert_eq!(request.headers()[AUTHORIZATION], "Bearer codex-token");
            assert_eq!(request.headers()["chatgpt-account-id"], "codex-account");
            assert_eq!(request.headers()["originator"], "qq");
            assert_eq!(
                request
                    .headers()
                    .get("x-openai-fedramp")
                    .map(|value| value.to_str().unwrap()),
                expected_fedramp
            );
            assert!(request.headers()[AUTHORIZATION].is_sensitive());
            assert!(request.headers()["chatgpt-account-id"].is_sensitive());
            // Only secret values are redacted: registering `true` or `qq`
            // would rewrite every matching substring of a diagnostic.
            assert_eq!(redactions, ["codex-token", "codex-account"]);
        }
    }

    #[test]
    fn credential_headers_outside_the_recipe_intent_are_rejected_before_any_is_applied() {
        let cases = [
            (
                "request-controlled name",
                credential(vec![
                    header("authorization", "Bearer bearer-secret", true),
                    header("host", "attacker.test", false),
                ]),
                RequestCredentialKindExpected::Bearer,
            ),
            (
                "protocol constant",
                credential(vec![
                    header("authorization", "Bearer bearer-secret", true),
                    header("anthropic-version", "2099-01-01", false),
                ]),
                RequestCredentialKindExpected::Bearer,
            ),
            (
                "another intent's credential name",
                credential(vec![
                    header("authorization", "Bearer bearer-secret", true),
                    header("chatgpt-account-id", "account-secret", true),
                ]),
                RequestCredentialKindExpected::Bearer,
            ),
            (
                "duplicate name",
                credential(vec![
                    header("authorization", "Bearer bearer-secret", true),
                    header("authorization", "Bearer second-secret", true),
                ]),
                RequestCredentialKindExpected::Bearer,
            ),
            (
                "missing required name",
                credential(vec![header("authorization", "Bearer bearer-secret", true)]),
                RequestCredentialKindExpected::Codex,
            ),
            (
                "too many headers",
                credential(
                    (0..=MAX_CREDENTIAL_HEADERS)
                        .map(|_| header("authorization", "Bearer bearer-secret", true))
                        .collect(),
                ),
                RequestCredentialKindExpected::Bearer,
            ),
        ];
        for (case, credential, expected) in cases {
            let mut request = request();
            let error = apply_request_credential(&mut request, credential, expected).unwrap_err();
            assert!(matches!(error, ProviderError::Configuration(_)), "{case}");
            assert!(!format!("{error:?} {error}").contains("secret"), "{case}");
            assert!(request.headers().is_empty(), "{case}: nothing applied");
        }
    }

    #[test]
    fn credential_header_bytes_are_bounded() {
        let large = "x".repeat(MAX_CREDENTIAL_HEADER_BYTES);
        let error = apply_request_credential(
            &mut request(),
            RequestCredential::bearer(large).unwrap(),
            RequestCredentialKindExpected::Bearer,
        )
        .unwrap_err();
        assert!(matches!(error, ProviderError::Configuration(_)));
    }
}
