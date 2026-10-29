use std::{
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    AuthError, CredentialBackend, CredentialStore, MissingCredentialRemedies, Secret, oauth,
    oauth::OAuthProvider,
    request::{CachedRequestCredentials, RequestCredentialSource, Resolution, Reuse},
    resolve_provider_credential,
};
use qq_provider::{
    RequestCredential, RequestCredentialError, SecretRef, SharedRequestCredentialProvider,
};

use super::{XAI_API_KEY_VARIABLE, XAI_CREDENTIAL_ENDPOINT};
use qq_protocol::CredentialEpoch;

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
            || !oauth::is_https_url(&response.verification_uri)
            || response
                .verification_uri_complete
                .as_deref()
                .is_some_and(|url| !oauth::is_https_url(url))
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

fn post(
    store: &CredentialStore,
    url: &str,
    fields: Vec<(&'static str, String)>,
    headers: &[(&'static str, &'static str)],
    timeout: Option<Duration>,
) -> Result<oauth::OAuthResponse, oauth::TransportError> {
    store.oauth.post(oauth::OAuthRequest {
        url,
        body: oauth::OAuthBody::Form(fields),
        headers,
        timeout,
        max_body: MAX_RESPONSE_BYTES,
    })
}

fn start_device(store: &CredentialStore) -> Result<DeviceAuthorization, XaiAuthError> {
    let response = post(
        store,
        DEVICE_ENDPOINT,
        vec![
            ("client_id", CLIENT_ID.to_owned()),
            ("scope", SCOPE.to_owned()),
        ],
        &[("referer", "qq")],
        None,
    )
    .map_err(|_| XaiAuthError::DeviceRequestFailed)?;
    if !response.is_success() {
        return Err(XaiAuthError::DeviceResponseInvalid);
    }
    let body: DeviceResponse = response.json().ok_or(XaiAuthError::DeviceResponseInvalid)?;
    body.try_into()
}

fn poll_device(
    store: &CredentialStore,
    device_code: &str,
    remaining: Duration,
) -> Result<DevicePoll, XaiAuthError> {
    let response = post(
        store,
        TOKEN_ENDPOINT,
        vec![
            ("grant_type", DEVICE_GRANT.to_owned()),
            ("client_id", CLIENT_ID.to_owned()),
            ("device_code", device_code.to_owned()),
        ],
        &[],
        Some(remaining),
    )
    .map_err(|error| match error {
        oauth::TransportError::DeadlineReached => XaiAuthError::AuthorizationTimedOut,
        oauth::TransportError::Failed => XaiAuthError::DeviceRequestFailed,
    })?;
    decode_poll(&response)
}

fn refresh(store: &CredentialStore, refresh_token: &str) -> Result<TokenSet, XaiAuthError> {
    let response = post(
        store,
        TOKEN_ENDPOINT,
        vec![
            ("grant_type", "refresh_token".to_owned()),
            ("client_id", CLIENT_ID.to_owned()),
            ("refresh_token", refresh_token.to_owned()),
        ],
        &[],
        None,
    )
    .map_err(|_| XaiAuthError::RefreshRequestFailed)?;
    if !response.is_success() {
        return Err(XaiAuthError::RefreshRejected);
    }
    decode_token(&response)
}

fn decode_poll(response: &oauth::OAuthResponse) -> Result<DevicePoll, XaiAuthError> {
    if response.is_success() {
        return decode_token(response).map(DevicePoll::Complete);
    }
    #[derive(Deserialize)]
    struct ErrorBody {
        error: String,
    }
    let body: ErrorBody = response.json().ok_or(XaiAuthError::TokenResponseInvalid)?;
    match body.error.as_str() {
        "authorization_pending" => Ok(DevicePoll::Pending),
        "slow_down" => Ok(DevicePoll::SlowDown),
        "access_denied" => Ok(DevicePoll::Denied),
        "expired_token" => Ok(DevicePoll::Expired),
        _ => Err(XaiAuthError::TokenResponseInvalid),
    }
}

fn decode_token(response: &oauth::OAuthResponse) -> Result<TokenSet, XaiAuthError> {
    #[derive(Deserialize)]
    struct TokenBody {
        access_token: String,
        refresh_token: Option<String>,
        expires_in: Option<u64>,
    }
    let body: TokenBody = response.json().ok_or(XaiAuthError::TokenResponseInvalid)?;
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

pub struct XaiLogin {
    authorization: DeviceAuthorization,
}

impl XaiLogin {
    pub fn start(store: &CredentialStore) -> Result<Self, AuthError> {
        Ok(Self {
            authorization: start_device(store)?,
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
        self.complete_with(
            store,
            profile,
            allow_file_fallback,
            &mut oauth::DevicePollRuntime {
                now: Instant::now,
                cancelled: || false,
                sleep: thread::sleep,
            },
        )
    }

    pub(super) fn complete_with<N, C, S>(
        self,
        store: &CredentialStore,
        profile: &str,
        allow_file_fallback: bool,
        runtime: &mut oauth::DevicePollRuntime<N, C, S>,
    ) -> Result<CredentialBackend, AuthError>
    where
        N: FnMut() -> Instant,
        C: FnMut() -> bool,
        S: FnMut(Duration),
    {
        let name = credential_name(profile)?;
        let deadline = (runtime.now)()
            .checked_add(Duration::from_secs(self.authorization.expires_in))
            .ok_or(XaiAuthError::DeviceResponseInvalid)?;
        let errors = oauth::DevicePollErrors {
            timed_out: || XaiAuthError::AuthorizationTimedOut.into(),
            cancelled: || XaiAuthError::AuthorizationTimedOut.into(),
            denied: || XaiAuthError::AuthorizationDenied.into(),
            expired: || XaiAuthError::AuthorizationExpired.into(),
        };
        let tokens = oauth::poll_device(
            deadline,
            Duration::from_secs(self.authorization.interval),
            Duration::from_secs(MAX_POLL_INTERVAL),
            runtime,
            &errors,
            |remaining| {
                Ok::<_, AuthError>(
                    match poll_device(store, &self.authorization.device_code, remaining)? {
                        DevicePoll::Pending => oauth::DeviceStep::Pending,
                        DevicePoll::SlowDown => oauth::DeviceStep::SlowDown,
                        DevicePoll::Denied => oauth::DeviceStep::Denied,
                        DevicePoll::Expired => oauth::DeviceStep::Expired,
                        DevicePoll::Complete(tokens) => oauth::DeviceStep::Complete(tokens),
                    },
                )
            },
        )?;
        let credential = StoredXaiCredential::from_tokens(tokens, None, unix_time()?)?;
        store.set_with_metadata(
            &name,
            credential.encode()?,
            allow_file_fallback,
            Some("xai-oauth"),
            Some(XAI_CREDENTIAL_ENDPOINT),
        )
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

    fn into_resolution(self, epoch: CredentialEpoch) -> (Secret, Reuse) {
        let reuse = Reuse {
            epoch,
            refresh_after: self.expires_at,
        };
        (
            Secret::from_secret_bytes(self.access_token.into_bytes()),
            reuse,
        )
    }
}

impl CredentialStore {
    pub fn xai_request_credentials(
        &self,
        profile: &str,
        explicit: Option<SecretRef>,
    ) -> SharedRequestCredentialProvider {
        let remedies = MissingCredentialRemedies::new(
            "xai",
            profile,
            &["qq auth login xai --oauth", "qq auth login xai"],
            Some(XAI_API_KEY_VARIABLE),
        );
        SharedRequestCredentialProvider::new(CachedRequestCredentials::new(
            self,
            XaiSource {
                profile: profile.to_owned(),
                explicit,
                remedies,
            },
        ))
    }

    pub fn resolve_xai(
        &self,
        profile: &str,
        explicit: Option<&SecretRef>,
    ) -> Result<Secret, AuthError> {
        self.resolve_xai_with_reuse(profile, explicit, false)
            .map(|(secret, _)| secret)
    }

    pub fn check_xai_credential(
        &self,
        profile: &str,
        explicit: Option<&SecretRef>,
    ) -> Result<(), AuthError> {
        let name = credential_name(profile)?;
        if explicit.is_none()
            && self
                .status(&name)?
                .is_some_and(|metadata| metadata.kind.as_deref() == Some("xai-oauth"))
        {
            return self.load_xai_oauth(&name, false).map(|_| ());
        }
        self.resolve_xai_with_reuse(profile, explicit, false)
            .map(|_| ())
    }

    fn resolve_xai_with_reuse(
        &self,
        profile: &str,
        explicit: Option<&SecretRef>,
        reload: bool,
    ) -> Result<(Secret, Option<Reuse>), AuthError> {
        if let Some(reference) = explicit {
            return self
                .resolve_with_endpoint(reference, Some(XAI_CREDENTIAL_ENDPOINT))
                .map(|secret| (secret, None));
        }
        let name = credential_name(profile)?;
        let without_reuse = |secret| (secret, None);
        match self.status(&name)? {
            Some(metadata) if metadata.kind.as_deref() == Some("xai-oauth") => self
                .resolve_xai_oauth_with_epoch(profile, reload)
                .map(|(secret, reuse)| (secret, Some(reuse))),
            Some(metadata) if metadata.kind.as_deref() == Some("xai") => self
                .resolve_with_endpoint(&SecretRef::Stored(name), Some(XAI_CREDENTIAL_ENDPOINT))
                .map(without_reuse),
            Some(_) => Err(XaiAuthError::StoredCredentialInvalid.into()),
            None => resolve_provider_credential(
                self,
                None,
                &name,
                XAI_API_KEY_VARIABLE,
                Some(XAI_CREDENTIAL_ENDPOINT),
            )
            .map(without_reuse),
        }
    }

    #[cfg(test)]
    pub(super) fn resolve_xai_oauth(&self, profile: &str) -> Result<Secret, AuthError> {
        self.resolve_xai_oauth_with_epoch(profile, false)
            .map(|(secret, _)| secret)
    }

    fn resolve_xai_oauth_with_epoch(
        &self,
        profile: &str,
        reload: bool,
    ) -> Result<(Secret, Reuse), AuthError> {
        let name = credential_name(profile)?;
        let now = unix_time()?;
        let (credential, epoch) = self.load_xai_oauth(&name, reload)?;
        if !credential.needs_refresh(now) {
            return Ok(credential.into_resolution(epoch));
        }

        let _refresh = self.lock_oauth_operation(OAuthProvider::XAI)?;
        let (credential, epoch) = self.load_xai_oauth(&name, true)?;
        let now = unix_time()?;
        if !credential.needs_refresh(now) {
            return Ok(credential.into_resolution(epoch));
        }

        let previous_refresh_token = credential.refresh_token;
        let tokens = refresh(self, &previous_refresh_token)?;
        let credential =
            StoredXaiCredential::from_tokens(tokens, Some(previous_refresh_token), now)?;
        self.replace_with_metadata_normalized(
            &name,
            &credential.encode()?,
            "xai-oauth".to_owned(),
            XAI_CREDENTIAL_ENDPOINT.to_owned(),
        )?;
        let (credential, epoch) = self.load_xai_oauth(&name, true)?;
        if credential.needs_refresh(unix_time()?) {
            return Err(XaiAuthError::TokenResponseInvalid.into());
        }
        Ok(credential.into_resolution(epoch))
    }

    fn load_xai_oauth(
        &self,
        name: &str,
        fresh: bool,
    ) -> Result<(StoredXaiCredential, CredentialEpoch), AuthError> {
        let metadata =
            self.status(name)?
                .ok_or_else(|| AuthError::StoredCredentialNotRegistered {
                    name: name.to_owned(),
                })?;
        if metadata.kind.as_deref() != Some("xai-oauth") {
            return Err(XaiAuthError::StoredCredentialInvalid.into());
        }
        let reference = SecretRef::Stored(name.to_owned());
        let (secret, epoch) = if fresh {
            self.resolve_with_epoch_uncached(&reference, Some(XAI_CREDENTIAL_ENDPOINT))?
        } else {
            self.resolve_with_epoch(&reference, Some(XAI_CREDENTIAL_ENDPOINT))?
        };
        let credential = StoredXaiCredential::parse(secret.expose_secret_bytes())?;
        Ok((credential, epoch))
    }
}

#[cfg(test)]
pub(super) fn source_for_test(profile: &str, explicit: Option<SecretRef>) -> XaiSource {
    XaiSource {
        profile: profile.to_owned(),
        explicit,
        remedies: MissingCredentialRemedies::new(
            "xai",
            profile,
            &["qq auth login xai --oauth", "qq auth login xai"],
            Some(XAI_API_KEY_VARIABLE),
        ),
    }
}

#[cfg(test)]
pub(super) fn request_credentials_for_test(
    store: &CredentialStore,
    profile: &str,
) -> CachedRequestCredentials<XaiSource> {
    CachedRequestCredentials::new(
        store,
        XaiSource {
            profile: profile.to_owned(),
            explicit: None,
            remedies: MissingCredentialRemedies::new(
                "xai",
                profile,
                &["qq auth login xai --oauth", "qq auth login xai"],
                Some(XAI_API_KEY_VARIABLE),
            ),
        },
    )
}

pub(super) struct XaiSource {
    profile: String,
    explicit: Option<SecretRef>,
    remedies: MissingCredentialRemedies,
}

impl RequestCredentialSource for XaiSource {
    type Credential = Secret;

    fn resolve(
        &self,
        store: &CredentialStore,
        reload: bool,
    ) -> Result<Resolution<Secret>, AuthError> {
        let (credential, reuse) =
            store.resolve_xai_with_reuse(&self.profile, self.explicit.as_ref(), reload)?;
        Ok(Resolution { credential, reuse })
    }

    fn request_credential(secret: &Secret) -> Result<RequestCredential, RequestCredentialError> {
        RequestCredential::bearer(
            secret
                .expose_secret_str()
                .map_err(|_| RequestCredentialError::Invalid)?,
        )
    }

    fn map_error(&self, error: AuthError) -> RequestCredentialError {
        map_request_credential_error(error, &self.remedies)
    }

    fn clock_unavailable() -> AuthError {
        XaiAuthError::ClockUnavailable.into()
    }

    fn never_cacheable(&self) -> bool {
        self.explicit.is_some()
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
        AuthError::XAi(XaiAuthError::RefreshRejected) => RequestCredentialError::RefreshRejected,
        AuthError::XAi(XaiAuthError::RefreshRequestFailed | XaiAuthError::TokenResponseInvalid) => {
            RequestCredentialError::RefreshUnavailable
        }
        AuthError::XAi(
            XaiAuthError::StoredCredentialInvalid
            | XaiAuthError::UnsupportedStoredCredentialVersion { .. },
        )
        | AuthError::SecretNotUnicode => RequestCredentialError::Invalid,
        _ => RequestCredentialError::StorageUnavailable,
    }
}

fn credential_name(profile: &str) -> Result<String, AuthError> {
    oauth::profile_credential_name(OAuthProvider::XAI, profile)
}

fn unix_time() -> Result<u64, XaiAuthError> {
    oauth::unix_time().map_err(|_| XaiAuthError::ClockUnavailable)
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

    struct StalledTokenEndpoint {
        timeouts: std::sync::Mutex<Vec<Option<Duration>>>,
    }

    impl oauth::OAuthTransport for StalledTokenEndpoint {
        fn post(
            &self,
            request: oauth::OAuthRequest<'_>,
        ) -> Result<oauth::OAuthResponse, oauth::TransportError> {
            assert_eq!(request.url, TOKEN_ENDPOINT);
            self.timeouts.lock().unwrap().push(request.timeout);
            Err(match request.timeout {
                Some(timeout) if timeout < oauth::REQUEST_TIMEOUT => {
                    oauth::TransportError::DeadlineReached
                }
                _ => oauth::TransportError::Failed,
            })
        }
    }

    fn stalled_login(expires_in: u64) -> (AuthError, Vec<Option<Duration>>, std::path::PathBuf) {
        let directory = std::env::temp_dir().join(format!(
            "qq-auth-xai-poll-{}-{expires_in}-{:?}",
            std::process::id(),
            Instant::now()
        ));
        let mut store = CredentialStore::with_paths(crate::CredentialPaths::new(&directory));
        let endpoint = std::sync::Arc::new(StalledTokenEndpoint {
            timeouts: std::sync::Mutex::new(Vec::new()),
        });
        store.oauth = endpoint.clone();
        let login = XaiLogin {
            authorization: DeviceAuthorization {
                device_code: "device-code".to_owned(),
                user_code: "USER-CODE".to_owned(),
                verification_uri: "https://accounts.x.ai/activate".to_owned(),
                verification_uri_complete: None,
                expires_in,
                interval: 5,
            },
        };
        let start = Instant::now();
        let elapsed = std::cell::Cell::new(Duration::ZERO);
        let error = login
            .complete_with(
                &store,
                "work",
                false,
                &mut oauth::DevicePollRuntime {
                    now: || start + elapsed.get(),
                    cancelled: || false,
                    sleep: |duration| elapsed.set(elapsed.get() + duration),
                },
            )
            .unwrap_err();
        let timeouts = endpoint.timeouts.lock().unwrap().clone();
        (error, timeouts, directory)
    }

    #[test]
    fn xai_device_polls_with_a_long_lifetime_fail_at_the_transport_cap() {
        let (error, timeouts, directory) = stalled_login(1800);
        assert!(
            matches!(error, AuthError::XAi(XaiAuthError::DeviceRequestFailed)),
            "{error:?}"
        );
        assert_eq!(timeouts, [Some(Duration::from_secs(1795))]);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn xai_device_polls_are_bounded_by_the_remaining_lifetime() {
        let (error, timeouts, directory) = stalled_login(40);
        assert!(
            matches!(error, AuthError::XAi(XaiAuthError::AuthorizationTimedOut)),
            "{error:?}"
        );
        assert_eq!(timeouts, [Some(Duration::from_secs(35))]);
        let _ = std::fs::remove_dir_all(directory);
    }
}
