//! xAI API-key and OAuth credentials.

use std::{
    io::Read,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use reqwest::blocking::Response;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

use super::{
    AuthError, CredentialBackend, CredentialStore, MissingCredentialRemedies, Secret,
    resolve_provider_credential, validate_credential_name,
};
use qq_provider::{
    RequestCredential, RequestCredentialError, RequestCredentialFuture, RequestCredentialProvider,
    SecretRef, SharedRequestCredentialProvider, XAI_CREDENTIAL_ENDPOINT,
};

const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const DEVICE_ENDPOINT: &str = "https://auth.x.ai/oauth2/device/code";
const TOKEN_ENDPOINT: &str = "https://auth.x.ai/oauth2/token";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_TOKEN_BYTES: usize = 256 * 1024;
const REFRESH_SKEW: u64 = 5 * 60;
const DEFAULT_TOKEN_LIFETIME: u64 = 60 * 60;
const STORED_CREDENTIAL_VERSION: u32 = 1;
const MAX_DEVICE_LIFETIME: u64 = 60 * 60;
const MAX_POLL_INTERVAL: u64 = 30;

#[derive(Clone, Copy, Debug, Error)]
pub enum XaiAuthError {
    #[error("the xAI OAuth device request failed")]
    DeviceRequestFailed,
    #[error("the xAI OAuth device response was invalid")]
    DeviceResponseInvalid,
    #[error("xAI OAuth authorization was denied")]
    AuthorizationDenied,
    #[error("xAI OAuth authorization expired")]
    AuthorizationExpired,
    #[error("xAI OAuth authorization timed out")]
    AuthorizationTimedOut,
    #[error("the xAI OAuth token refresh request failed")]
    RefreshRequestFailed,
    #[error("the xAI OAuth token refresh was rejected")]
    RefreshRejected,
    #[error("the xAI OAuth token response was invalid")]
    TokenResponseInvalid,
    #[error("stored xAI OAuth credentials are invalid")]
    StoredCredentialInvalid,
    #[error("stored xAI OAuth credentials use unsupported version {version}")]
    UnsupportedStoredCredentialVersion { version: u32 },
    #[error("the xAI OAuth credential refresh lock is unavailable")]
    RefreshLockUnavailable,
    #[error("the system clock is unavailable")]
    ClockUnavailable,
}

#[derive(Clone)]
pub(super) struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
struct DeviceResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}

impl TryFrom<DeviceResponse> for DeviceAuthorization {
    type Error = XaiAuthError;

    fn try_from(response: DeviceResponse) -> Result<Self, Self::Error> {
        if response.device_code.is_empty()
            || response.user_code.is_empty()
            || response.expires_in == 0
            || response.expires_in > MAX_DEVICE_LIFETIME
            || response
                .interval
                .is_some_and(|interval| interval > MAX_POLL_INTERVAL)
            || !is_https_url(&response.verification_uri)
            || response
                .verification_uri_complete
                .as_deref()
                .is_some_and(|url| !is_https_url(url))
        {
            return Err(XaiAuthError::DeviceResponseInvalid);
        }
        Ok(Self {
            device_code: response.device_code,
            user_code: response.user_code,
            verification_uri: response.verification_uri,
            verification_uri_complete: response.verification_uri_complete,
            expires_in: response.expires_in,
            interval: response.interval.unwrap_or(5).max(1),
        })
    }
}

#[derive(Clone)]
pub(super) struct TokenSet {
    pub(super) access_token: String,
    pub(super) refresh_token: Option<String>,
    pub(super) expires_in: Option<u64>,
}

pub(super) enum DevicePoll {
    Pending,
    SlowDown,
    Denied,
    Expired,
    Complete(TokenSet),
}

pub(super) trait XaiTokenClient: Send + Sync {
    fn start_device(&self) -> Result<DeviceAuthorization, XaiAuthError>;
    fn poll_device(&self, device_code: &str) -> Result<DevicePoll, XaiAuthError>;
    fn refresh(&self, refresh_token: &str) -> Result<TokenSet, XaiAuthError>;
}

pub(super) struct SystemXaiTokenClient;

impl XaiTokenClient for SystemXaiTokenClient {
    fn start_device(&self) -> Result<DeviceAuthorization, XaiAuthError> {
        let response = oauth_client()?
            .post(DEVICE_ENDPOINT)
            .header("referer", "qq")
            .form(&[("client_id", CLIENT_ID), ("scope", SCOPE)])
            .send()
            .map_err(|_| XaiAuthError::DeviceRequestFailed)?;
        let body: DeviceResponse = decode_success(response, XaiAuthError::DeviceResponseInvalid)?;
        body.try_into()
    }

    fn poll_device(&self, device_code: &str) -> Result<DevicePoll, XaiAuthError> {
        let response = oauth_client()?
            .post(TOKEN_ENDPOINT)
            .form(&[
                ("grant_type", DEVICE_GRANT),
                ("client_id", CLIENT_ID),
                ("device_code", device_code),
            ])
            .send()
            .map_err(|_| XaiAuthError::DeviceRequestFailed)?;
        decode_poll(response)
    }

    fn refresh(&self, refresh_token: &str) -> Result<TokenSet, XaiAuthError> {
        let response = oauth_client()?
            .post(TOKEN_ENDPOINT)
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", CLIENT_ID),
                ("refresh_token", refresh_token),
            ])
            .send()
            .map_err(|_| XaiAuthError::RefreshRequestFailed)?;
        if !response.status().is_success() {
            return Err(XaiAuthError::RefreshRejected);
        }
        decode_token(response)
    }
}

fn oauth_client() -> Result<reqwest::blocking::Client, XaiAuthError> {
    reqwest::blocking::Client::builder()
        .use_rustls_tls()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("qq/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| XaiAuthError::DeviceRequestFailed)
}

fn decode_poll(mut response: Response) -> Result<DevicePoll, XaiAuthError> {
    if response.status().is_success() {
        return decode_token(response).map(DevicePoll::Complete);
    }
    #[derive(Deserialize)]
    struct ErrorBody {
        error: String,
    }
    let body: ErrorBody = decode_bounded(&mut response, XaiAuthError::TokenResponseInvalid)?;
    match body.error.as_str() {
        "authorization_pending" => Ok(DevicePoll::Pending),
        "slow_down" => Ok(DevicePoll::SlowDown),
        "access_denied" => Ok(DevicePoll::Denied),
        "expired_token" => Ok(DevicePoll::Expired),
        _ => Err(XaiAuthError::TokenResponseInvalid),
    }
}

fn decode_token(mut response: Response) -> Result<TokenSet, XaiAuthError> {
    #[derive(Deserialize)]
    struct TokenBody {
        access_token: String,
        refresh_token: Option<String>,
        expires_in: Option<u64>,
    }
    let body: TokenBody = decode_bounded(&mut response, XaiAuthError::TokenResponseInvalid)?;
    if body.access_token.is_empty()
        || body.access_token.len() > MAX_TOKEN_BYTES
        || body
            .refresh_token
            .as_ref()
            .is_some_and(|token| token.is_empty() || token.len() > MAX_TOKEN_BYTES)
    {
        return Err(XaiAuthError::TokenResponseInvalid);
    }
    Ok(TokenSet {
        access_token: body.access_token,
        refresh_token: body.refresh_token,
        expires_in: body.expires_in,
    })
}

fn decode_success<T: DeserializeOwned>(
    mut response: Response,
    error: XaiAuthError,
) -> Result<T, XaiAuthError> {
    if !response.status().is_success() {
        return Err(error);
    }
    decode_bounded(&mut response, error)
}

fn decode_bounded<T: DeserializeOwned>(
    response: &mut Response,
    error: XaiAuthError,
) -> Result<T, XaiAuthError> {
    let mut bytes = Vec::new();
    response
        .take((MAX_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| error)?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(error);
    }
    serde_json::from_slice(&bytes).map_err(|_| error)
}

fn is_https_url(value: &str) -> bool {
    reqwest::Url::parse(value).is_ok_and(|url| url.scheme() == "https" && url.host().is_some())
}

pub struct XaiLogin {
    authorization: DeviceAuthorization,
}

impl XaiLogin {
    pub fn start(store: &CredentialStore) -> Result<Self, AuthError> {
        Ok(Self {
            authorization: store.xai_client.start_device()?,
        })
    }

    #[must_use]
    pub fn verification_url(&self) -> &str {
        self.authorization
            .verification_uri_complete
            .as_deref()
            .unwrap_or(&self.authorization.verification_uri)
    }

    #[must_use]
    pub fn user_code(&self) -> &str {
        &self.authorization.user_code
    }

    pub fn complete(
        self,
        store: &CredentialStore,
        profile: &str,
        allow_file_fallback: bool,
    ) -> Result<CredentialBackend, AuthError> {
        let name = credential_name(profile)?;
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(self.authorization.expires_in))
            .ok_or(XaiAuthError::DeviceResponseInvalid)?;
        let mut interval = Duration::from_secs(self.authorization.interval);
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(XaiAuthError::AuthorizationTimedOut.into());
            }
            thread::sleep(interval.min(deadline.saturating_duration_since(now)));
            if Instant::now() >= deadline {
                return Err(XaiAuthError::AuthorizationTimedOut.into());
            }
            match store
                .xai_client
                .poll_device(&self.authorization.device_code)?
            {
                DevicePoll::Pending => {}
                DevicePoll::SlowDown => {
                    interval = interval
                        .saturating_add(Duration::from_secs(5))
                        .min(Duration::from_secs(MAX_POLL_INTERVAL));
                }
                DevicePoll::Denied => return Err(XaiAuthError::AuthorizationDenied.into()),
                DevicePoll::Expired => return Err(XaiAuthError::AuthorizationExpired.into()),
                DevicePoll::Complete(tokens) => {
                    let credential = StoredXaiCredential::from_tokens(tokens, None, unix_time()?)?;
                    return store.set_with_metadata(
                        &name,
                        credential.encode()?,
                        allow_file_fallback,
                        Some("xai-oauth"),
                        Some(XAI_CREDENTIAL_ENDPOINT),
                    );
                }
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredXaiCredential {
    version: u32,
    access_token: String,
    refresh_token: String,
    expires_at: u64,
}

impl StoredXaiCredential {
    fn from_tokens(
        tokens: TokenSet,
        previous_refresh_token: Option<String>,
        now: u64,
    ) -> Result<Self, XaiAuthError> {
        let refresh_token = tokens
            .refresh_token
            .or(previous_refresh_token)
            .ok_or(XaiAuthError::TokenResponseInvalid)?;
        let lifetime = tokens.expires_in.unwrap_or(DEFAULT_TOKEN_LIFETIME);
        let expires_at = now
            .saturating_add(lifetime)
            .saturating_sub(REFRESH_SKEW.min(lifetime));
        let credential = Self {
            version: STORED_CREDENTIAL_VERSION,
            access_token: tokens.access_token,
            refresh_token,
            expires_at,
        };
        credential.validate()?;
        Ok(credential)
    }

    fn parse(bytes: &[u8]) -> Result<Self, XaiAuthError> {
        let credential: Self =
            serde_json::from_slice(bytes).map_err(|_| XaiAuthError::StoredCredentialInvalid)?;
        if credential.version != STORED_CREDENTIAL_VERSION {
            return Err(XaiAuthError::UnsupportedStoredCredentialVersion {
                version: credential.version,
            });
        }
        credential.validate()?;
        Ok(credential)
    }

    fn validate(&self) -> Result<(), XaiAuthError> {
        if self.access_token.is_empty()
            || self.refresh_token.is_empty()
            || self.access_token.len() > MAX_TOKEN_BYTES
            || self.refresh_token.len() > MAX_TOKEN_BYTES
            || self.expires_at == 0
        {
            return Err(XaiAuthError::StoredCredentialInvalid);
        }
        Ok(())
    }

    fn encode(&self) -> Result<Vec<u8>, XaiAuthError> {
        serde_json::to_vec(self).map_err(|_| XaiAuthError::StoredCredentialInvalid)
    }

    fn needs_refresh(&self, now: u64) -> bool {
        self.expires_at <= now
    }
}

impl CredentialStore {
    pub fn xai_request_credentials(
        &self,
        profile: &str,
        explicit: Option<SecretRef>,
    ) -> SharedRequestCredentialProvider {
        // Remedy text is authored here, once, so the request path never
        // formats provider-specific guidance.
        let remedies = MissingCredentialRemedies::new(
            "xai",
            profile,
            &["qq auth login xai --oauth", "qq auth login xai"],
            Some("XAI_API_KEY"),
        );
        SharedRequestCredentialProvider::new(XaiRequestCredentials {
            store: self.clone(),
            profile: profile.to_owned(),
            explicit,
            remedies,
        })
    }

    pub fn resolve_xai(
        &self,
        profile: &str,
        explicit: Option<&SecretRef>,
    ) -> Result<Secret, AuthError> {
        if let Some(reference) = explicit {
            return self.resolve_with_endpoint(reference, Some(XAI_CREDENTIAL_ENDPOINT));
        }
        let name = credential_name(profile)?;
        match self.status(&name)? {
            Some(metadata) if metadata.kind.as_deref() == Some("xai-oauth") => {
                self.resolve_xai_oauth(profile)
            }
            Some(metadata) if metadata.kind.as_deref() == Some("xai") => {
                self.resolve_with_endpoint(&SecretRef::Stored(name), Some(XAI_CREDENTIAL_ENDPOINT))
            }
            Some(_) => Err(XaiAuthError::StoredCredentialInvalid.into()),
            None => resolve_provider_credential(
                self,
                None,
                &name,
                "XAI_API_KEY",
                Some(XAI_CREDENTIAL_ENDPOINT),
            ),
        }
    }

    pub(super) fn resolve_xai_oauth(&self, profile: &str) -> Result<Secret, AuthError> {
        let name = credential_name(profile)?;
        let now = unix_time()?;
        let credential = self.load_xai_oauth(&name)?;
        if !credential.needs_refresh(now) {
            return Ok(Secret::from_secret_bytes(
                credential.access_token.into_bytes(),
            ));
        }

        let _refresh = self
            .lock_xai_operation(&name)?
            .expect("xAI credential names always require the xAI lock");
        let credential = self.load_xai_oauth(&name)?;
        let now = unix_time()?;
        if !credential.needs_refresh(now) {
            return Ok(Secret::from_secret_bytes(
                credential.access_token.into_bytes(),
            ));
        }

        let previous_refresh_token = credential.refresh_token;
        let tokens = self.xai_client.refresh(&previous_refresh_token)?;
        let credential =
            StoredXaiCredential::from_tokens(tokens, Some(previous_refresh_token), now)?;
        self.replace_with_metadata_normalized(
            &name,
            &credential.encode()?,
            "xai-oauth".to_owned(),
            XAI_CREDENTIAL_ENDPOINT.to_owned(),
        )?;
        Ok(Secret::from_secret_bytes(
            credential.access_token.into_bytes(),
        ))
    }

    fn load_xai_oauth(&self, name: &str) -> Result<StoredXaiCredential, AuthError> {
        let metadata =
            self.status(name)?
                .ok_or_else(|| AuthError::StoredCredentialNotRegistered {
                    name: name.to_owned(),
                })?;
        if metadata.kind.as_deref() != Some("xai-oauth") {
            return Err(XaiAuthError::StoredCredentialInvalid.into());
        }
        let secret = self.resolve_with_endpoint(
            &SecretRef::Stored(name.to_owned()),
            Some(XAI_CREDENTIAL_ENDPOINT),
        )?;
        StoredXaiCredential::parse(secret.expose_secret_bytes()).map_err(Into::into)
    }
}

struct XaiRequestCredentials {
    store: CredentialStore,
    profile: String,
    explicit: Option<SecretRef>,
    remedies: MissingCredentialRemedies,
}

impl RequestCredentialProvider for XaiRequestCredentials {
    fn credential(&self) -> RequestCredentialFuture<'_> {
        let store = self.store.clone();
        let profile = self.profile.clone();
        let explicit = self.explicit.clone();
        Box::pin(async move {
            let secret = store
                .load_request_credential(move |store| {
                    store.resolve_xai(&profile, explicit.as_ref())
                })
                .await?
                .map_err(|error| map_request_credential_error(error, &self.remedies))?;
            RequestCredential::bearer(
                secret
                    .expose_secret_str()
                    .map_err(|_| RequestCredentialError::Invalid)?,
            )
        })
    }
}

fn map_request_credential_error(
    error: AuthError,
    remedies: &MissingCredentialRemedies,
) -> RequestCredentialError {
    match error {
        AuthError::EnvironmentMissing { .. }
        | AuthError::ProviderCredentialMissing { .. }
        | AuthError::StoredCredentialNotRegistered { .. }
        | AuthError::StoredCredentialMissing { .. } => remedies.missing(&error),
        AuthError::XAi(XaiAuthError::RefreshRejected | XaiAuthError::AuthorizationDenied) => {
            RequestCredentialError::RefreshRejected
        }
        AuthError::XAi(
            XaiAuthError::RefreshRequestFailed
            | XaiAuthError::TokenResponseInvalid
            | XaiAuthError::AuthorizationTimedOut,
        ) => RequestCredentialError::RefreshUnavailable,
        AuthError::XAi(
            XaiAuthError::StoredCredentialInvalid
            | XaiAuthError::UnsupportedStoredCredentialVersion { .. },
        )
        | AuthError::SecretNotUnicode => RequestCredentialError::Invalid,
        _ => RequestCredentialError::StorageUnavailable,
    }
}

fn credential_name(profile: &str) -> Result<String, AuthError> {
    let name = format!("xai/{profile}");
    validate_credential_name(&name)?;
    Ok(name)
}

fn unix_time() -> Result<u64, XaiAuthError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| XaiAuthError::ClockUnavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_xai_device_lifetime_is_accepted() {
        let response: DeviceResponse = serde_json::from_value(serde_json::json!({
            "device_code": "device-code",
            "user_code": "USER-CODE",
            "verification_uri": "https://accounts.x.ai/activate",
            "verification_uri_complete": "https://accounts.x.ai/activate?user_code=USER-CODE",
            "expires_in": 1800,
            "interval": 5
        }))
        .unwrap();

        let authorization = DeviceAuthorization::try_from(response).unwrap();

        assert_eq!(authorization.expires_in, 1800);
        assert_eq!(authorization.interval, 5);
    }
}
