//! Request-time HTTP authorization.

use std::{future::Future, pin::Pin, sync::Arc};

use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};

#[cfg(feature = "provider-bedrock")]
use crate::aws::{AwsCredentialLease, SigV4Authorizer};
use crate::{ProviderError, ProviderErrorKind, credentials::SecretLiteral};

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

#[derive(Clone)]
pub struct RequestCredential {
    kind: RequestCredentialKind,
}

#[derive(Clone)]
enum RequestCredentialKind {
    Bearer(SecretLiteral),
    Codex {
        access_token: SecretLiteral,
        account_id: SecretLiteral,
        is_fedramp: bool,
    },
}

impl RequestCredential {
    pub fn bearer(token: impl Into<String>) -> Result<Self, RequestCredentialError> {
        let token = SecretLiteral::new(token);
        validate_credential_value(token.expose_secret())?;
        Ok(Self {
            kind: RequestCredentialKind::Bearer(token),
        })
    }

    pub fn codex(
        access_token: impl Into<String>,
        account_id: impl Into<String>,
        is_fedramp: bool,
    ) -> Result<Self, RequestCredentialError> {
        let access_token = SecretLiteral::new(access_token);
        let account_id = SecretLiteral::new(account_id);
        validate_credential_value(access_token.expose_secret())?;
        validate_credential_value(account_id.expose_secret())?;
        Ok(Self {
            kind: RequestCredentialKind::Codex {
                access_token,
                account_id,
                is_fedramp,
            },
        })
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RequestCredentialError {
    #[error("request credentials are missing")]
    Missing,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestCredentialKindExpected {
    Bearer,
    Codex,
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
        RequestCredentialError::Missing
        | RequestCredentialError::Invalid
        | RequestCredentialError::RefreshRejected => ProviderErrorKind::Authentication,
        RequestCredentialError::RefreshUnavailable
        | RequestCredentialError::StorageUnavailable
        | RequestCredentialError::TimedOut
        | RequestCredentialError::CapacityUnavailable
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
    if !matches!(
        (&credential.kind, expected),
        (
            RequestCredentialKind::Bearer(_),
            RequestCredentialKindExpected::Bearer
        ) | (
            RequestCredentialKind::Codex { .. },
            RequestCredentialKindExpected::Codex
        )
    ) {
        return Err(ProviderError::Configuration(
            "request credential kind did not match configured authorization intent".to_owned(),
        ));
    }

    let mut redactions = Vec::new();
    match credential.kind {
        RequestCredentialKind::Bearer(token) => {
            let token = token.expose_secret();
            insert_sensitive_header(request, AUTHORIZATION, &format!("Bearer {token}"))?;
            redactions.push(token.to_owned());
        }
        RequestCredentialKind::Codex {
            access_token,
            account_id,
            is_fedramp,
        } => {
            let access_token = access_token.expose_secret();
            let account_id = account_id.expose_secret();
            insert_sensitive_header(request, AUTHORIZATION, &format!("Bearer {access_token}"))?;
            insert_sensitive_header(
                request,
                HeaderName::from_static("chatgpt-account-id"),
                account_id,
            )?;
            request.headers_mut().insert(
                HeaderName::from_static("originator"),
                HeaderValue::from_static("qq"),
            );
            if is_fedramp {
                request.headers_mut().insert(
                    HeaderName::from_static("x-openai-fedramp"),
                    HeaderValue::from_static("true"),
                );
            } else {
                request
                    .headers_mut()
                    .remove(HeaderName::from_static("x-openai-fedramp"));
            }
            redactions.push(access_token.to_owned());
            redactions.push(account_id.to_owned());
        }
    }
    Ok(redactions)
}

fn insert_sensitive_header(
    request: &mut reqwest::Request,
    name: HeaderName,
    value: &str,
) -> Result<(), ProviderError> {
    let mut value = HeaderValue::from_str(value).map_err(|_| {
        ProviderError::Configuration("request credential produced an invalid header".to_owned())
    })?;
    value.set_sensitive(true);
    request.headers_mut().insert(name, value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use reqwest::header::AUTHORIZATION;

    use super::*;

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
}
