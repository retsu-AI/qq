//! Transport and bookkeeping shared by the provider OAuth flows.

use std::{
    io::Read,
    time::{Duration, SystemTime, SystemTimeError, UNIX_EPOCH},
};

use reqwest::blocking::{Client, Response};
use serde::de::DeserializeOwned;

use super::{AuthError, validate_credential_name};

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

/// Seconds since the Unix epoch.
pub(super) fn unix_time() -> Result<u64, SystemTimeError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
}

/// The stored credential name for `provider`'s `profile`: `PROVIDER/PROFILE`.
pub(super) fn profile_credential_name(provider: &str, profile: &str) -> Result<String, AuthError> {
    let name = format!("{provider}/{profile}");
    validate_credential_name(&name)?;
    Ok(name)
}
