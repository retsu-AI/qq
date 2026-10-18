//! Transport and bookkeeping shared by the provider OAuth flows.

use std::{
    io::Read,
    time::{Duration, SystemTime, SystemTimeError, UNIX_EPOCH},
};

use reqwest::blocking::{Client, Response};
use serde::de::DeserializeOwned;

use super::{AuthError, validate_credential_name};

/// The built-in providers whose stored credentials are OAuth token sets that
/// QQ refreshes. Logins, refreshes, rotations and removals of one provider's
/// credentials are serialised through its process mutex and lock file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OAuthProvider {
    OpenAiCodex,
    XAi,
}

impl OAuthProvider {
    pub(crate) const ALL: [Self; 2] = [Self::OpenAiCodex, Self::XAi];

    /// The provider half of its `PROVIDER/PROFILE` credential names.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::OpenAiCodex => "openai-codex",
            Self::XAi => "xai",
        }
    }

    /// The lock file in the credential data directory. The names are an
    /// on-disk contract: concurrently running QQ versions must agree on them.
    pub(crate) const fn lock_file_name(self) -> &'static str {
        match self {
            Self::OpenAiCodex => "openai-codex.lock",
            Self::XAi => "xai.lock",
        }
    }

    /// Position in [`Self::ALL`], for per-provider arrays.
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::OpenAiCodex => 0,
            Self::XAi => 1,
        }
    }

    /// The provider whose credentials are stored under `name`, if any.
    pub(crate) fn for_credential(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|provider| {
            name.strip_prefix(provider.name())
                .is_some_and(|rest| rest.starts_with('/'))
        })
    }

    pub(crate) fn lock_unavailable(self) -> AuthError {
        match self {
            Self::OpenAiCodex => super::codex::CodexAuthError::RefreshLockUnavailable.into(),
            Self::XAi => super::xai::XaiAuthError::RefreshLockUnavailable.into(),
        }
    }
}

/// The HTTPS-only, redirect-free client every OAuth token and device endpoint
/// is called with. Built per call: logins and refreshes are rare, and a
/// shared client would outlive the store that owns its configuration.
pub(super) fn http_client() -> Result<Client, reqwest::Error> {
    Client::builder()
        .use_rustls_tls()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("qq/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// Reads at most `limit` bytes of `response` and decodes them as JSON.
/// `None` covers a read failure, an oversized body and malformed JSON alike:
/// every caller reports all three as its own "invalid response" error.
pub(super) fn read_json<T: DeserializeOwned>(response: &mut Response, limit: usize) -> Option<T> {
    let mut bytes = Vec::new();
    response
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > limit {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

/// Whether `value` is an absolute HTTPS URL with a host. Verification URLs
/// shown to the user come from the authorization server's response; only
/// HTTPS ones are opened or printed.
pub(super) fn is_https_url(value: &str) -> bool {
    reqwest::Url::parse(value).is_ok_and(|url| url.scheme() == "https" && url.host().is_some())
}

/// Seconds since the Unix epoch.
pub(super) fn unix_time() -> Result<u64, SystemTimeError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
}

/// The stored credential name for `provider`'s `profile`: `PROVIDER/PROFILE`.
pub(super) fn profile_credential_name(
    provider: OAuthProvider,
    profile: &str,
) -> Result<String, AuthError> {
    let name = format!("{}/{profile}", provider.name());
    validate_credential_name(&name)?;
    Ok(name)
}
