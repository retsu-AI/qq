//! Native OpenAI Codex OAuth credentials.

use std::{
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{Url, blocking::Response};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::{
    AuthError, CredentialBackend, CredentialStore, MissingCredentialRemedies, Secret,
    validate_credential_name,
};
use qq_protocol::CredentialEpoch;
use qq_provider::SecretRef;
use qq_provider::{
    RequestCredential, RequestCredentialError, RequestCredentialFuture, RequestCredentialProvider,
    SharedRequestCredentialProvider,
};

pub(super) const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub(crate) const CREDENTIAL_ENDPOINT: &str = "https://chatgpt.com";

const ISSUER: &str = "https://auth.openai.com";
const TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
const DEFAULT_CALLBACK_PORT: u16 = 1455;
const FALLBACK_CALLBACK_PORT: u16 = 1457;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const CALLBACK_POLL_INTERVAL: Duration = Duration::from_millis(25);
const CALLBACK_IO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CALLBACK_BYTES: usize = 16 * 1024;
const MAX_TOKEN_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_TOKEN_BYTES: usize = 256 * 1024;
const MAX_ACCOUNT_ID_BYTES: usize = 4 * 1024;
const REFRESH_WINDOW_SECONDS: u64 = 5 * 60;
const REFRESH_FALLBACK_SECONDS: u64 = 8 * 24 * 60 * 60;
const STORED_CREDENTIAL_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum CodexAuthError {
    #[error("secure randomness is unavailable for OpenAI Codex login")]
    RandomnessUnavailable,
    #[error("the OpenAI Codex login callback ports are unavailable")]
    CallbackUnavailable,
    #[error("the OpenAI Codex login callback failed")]
    CallbackFailed,
    #[error("OpenAI Codex login timed out")]
    CallbackTimedOut,
    #[error("OpenAI Codex authorization was denied: {code}")]
    AuthorizationDenied { code: String },
    #[error("OpenAI Codex authorization did not return a code")]
    AuthorizationCodeMissing,
    #[error("the OpenAI Codex token {operation} request failed")]
    TokenRequestFailed { operation: &'static str },
    #[error("the OpenAI Codex token {operation} request returned HTTP {status}")]
    TokenRequestRejected {
        operation: &'static str,
        status: u16,
    },
    #[error("the OpenAI Codex token {operation} response was invalid")]
    TokenResponseInvalid { operation: &'static str },
    #[error("the OpenAI Codex ID token was invalid")]
    IdTokenInvalid,
    #[error("the OpenAI Codex ID token did not contain a ChatGPT account ID")]
    AccountIdMissing,
    #[error("stored OpenAI Codex credentials are invalid")]
    StoredCredentialInvalid,
    #[error("stored OpenAI Codex credentials use unsupported version {version}")]
    UnsupportedStoredCredentialVersion { version: u32 },
    #[error("the OpenAI Codex credential refresh lock is unavailable")]
    RefreshLockUnavailable,
    #[error("the system clock is unavailable")]
    ClockUnavailable,
}

#[derive(Clone)]
pub(super) struct ExchangedTokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Clone, Default)]
pub(super) struct RefreshedTokens {
    pub id_token: Option<String>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
}

pub(super) trait CodexTokenClient: Send + Sync {
    fn exchange(
        &self,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> Result<ExchangedTokens, CodexAuthError>;

    fn refresh(&self, refresh_token: &str) -> Result<RefreshedTokens, CodexAuthError>;
}

pub(super) struct SystemCodexTokenClient;

impl CodexTokenClient for SystemCodexTokenClient {
    fn exchange(
        &self,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> Result<ExchangedTokens, CodexAuthError> {
        #[derive(Deserialize)]
        struct TokenResponse {
            id_token: String,
            access_token: String,
            refresh_token: String,
        }

        let client = token_client("exchange")?;
        let response = client
            .post(TOKEN_ENDPOINT)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("client_id", CLIENT_ID),
                ("code_verifier", code_verifier),
            ])
            .send()
            .map_err(|_| CodexAuthError::TokenRequestFailed {
                operation: "exchange",
            })?;
        let tokens: TokenResponse = decode_token_response(response, "exchange")?;
        Ok(ExchangedTokens {
            id_token: tokens.id_token,
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
        })
    }

    fn refresh(&self, refresh_token: &str) -> Result<RefreshedTokens, CodexAuthError> {
        #[derive(Serialize)]
        struct RefreshRequest<'a> {
            client_id: &'static str,
            grant_type: &'static str,
            refresh_token: &'a str,
        }

        #[derive(Deserialize)]
        struct RefreshResponse {
            id_token: Option<String>,
            access_token: Option<String>,
            refresh_token: Option<String>,
        }

        let client = token_client("refresh")?;
        let response = client
            .post(TOKEN_ENDPOINT)
            .json(&RefreshRequest {
                client_id: CLIENT_ID,
                grant_type: "refresh_token",
                refresh_token,
            })
            .send()
            .map_err(|_| CodexAuthError::TokenRequestFailed {
                operation: "refresh",
            })?;
        let tokens: RefreshResponse = decode_token_response(response, "refresh")?;
        if tokens.id_token.is_none()
            && tokens.access_token.is_none()
            && tokens.refresh_token.is_none()
        {
            return Err(CodexAuthError::TokenResponseInvalid {
                operation: "refresh",
            });
        }
        Ok(RefreshedTokens {
            id_token: tokens.id_token,
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
        })
    }
}

fn token_client(operation: &'static str) -> Result<reqwest::blocking::Client, CodexAuthError> {
    reqwest::blocking::Client::builder()
        .use_rustls_tls()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("qq/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| CodexAuthError::TokenRequestFailed { operation })
}

fn decode_token_response<T: DeserializeOwned>(
    mut response: Response,
    operation: &'static str,
) -> Result<T, CodexAuthError> {
    let status = response.status();
    if !status.is_success() {
        return Err(CodexAuthError::TokenRequestRejected {
            operation,
            status: status.as_u16(),
        });
    }

    let mut bytes = Vec::new();
    response
        .by_ref()
        .take((MAX_TOKEN_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| CodexAuthError::TokenResponseInvalid { operation })?;
    if bytes.len() > MAX_TOKEN_RESPONSE_BYTES {
        return Err(CodexAuthError::TokenResponseInvalid { operation });
    }
    serde_json::from_slice(&bytes).map_err(|_| CodexAuthError::TokenResponseInvalid { operation })
}

/// A short-lived loopback OAuth login awaiting the browser callback.
pub struct CodexLogin {
    listener: TcpListener,
    authorization_url: String,
    redirect_uri: String,
    state: String,
    code_verifier: String,
    timeout: Duration,
}

impl CodexLogin {
    pub fn start() -> Result<Self, AuthError> {
        let state = random_base64(32)?;
        let code_verifier = random_base64(64)?;
        Self::start_with(
            DEFAULT_CALLBACK_PORT,
            true,
            state,
            code_verifier,
            LOGIN_TIMEOUT,
        )
        .map_err(Into::into)
    }

    #[cfg(test)]
    pub(super) fn start_for_test(
        port: u16,
        state: &str,
        code_verifier: &str,
        timeout: Duration,
    ) -> Result<Self, AuthError> {
        Self::start_with(
            port,
            false,
            state.to_owned(),
            code_verifier.to_owned(),
            timeout,
        )
        .map_err(Into::into)
    }

    fn start_with(
        port: u16,
        allow_fallback: bool,
        state: String,
        code_verifier: String,
        timeout: Duration,
    ) -> Result<Self, CodexAuthError> {
        let listener = bind_callback(port, allow_fallback)?;
        listener
            .set_nonblocking(true)
            .map_err(|_| CodexAuthError::CallbackUnavailable)?;
        let actual_port = listener
            .local_addr()
            .map_err(|_| CodexAuthError::CallbackUnavailable)?
            .port();
        let redirect_uri = format!("http://localhost:{actual_port}/auth/callback");
        let authorization_url = authorization_url(&redirect_uri, &state, &code_verifier)?;
        Ok(Self {
            listener,
            authorization_url,
            redirect_uri,
            state,
            code_verifier,
            timeout,
        })
    }

    #[must_use]
    pub fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    pub fn complete(
        self,
        store: &CredentialStore,
        profile: &str,
        allow_file_fallback: bool,
    ) -> Result<CredentialBackend, AuthError> {
        let name = credential_name(profile)?;
        let deadline = Instant::now() + self.timeout;

        loop {
            if Instant::now() >= deadline {
                return Err(CodexAuthError::CallbackTimedOut.into());
            }
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(CALLBACK_IO_TIMEOUT))
                        .map_err(|_| CodexAuthError::CallbackFailed)?;
                    stream
                        .set_write_timeout(Some(CALLBACK_IO_TIMEOUT))
                        .map_err(|_| CodexAuthError::CallbackFailed)?;
                    match read_callback(&mut stream, &self.state, deadline) {
                        Callback::Ignore { status, body } => {
                            let _ = respond(&mut stream, status, body);
                        }
                        Callback::Denied(code) => {
                            let _ =
                                respond(&mut stream, 400, "OpenAI Codex login was not completed.");
                            return Err(CodexAuthError::AuthorizationDenied { code }.into());
                        }
                        Callback::MissingCode => {
                            let _ = respond(&mut stream, 400, "Authorization code missing.");
                            return Err(CodexAuthError::AuthorizationCodeMissing.into());
                        }
                        Callback::Code(code) => {
                            let result =
                                self.finish_login(store, &name, &code, allow_file_fallback);
                            match result {
                                Ok(backend) => {
                                    let _ = respond(
                                        &mut stream,
                                        200,
                                        "OpenAI Codex login complete. You may close this window.",
                                    );
                                    return Ok(backend);
                                }
                                Err(error) => {
                                    let _ = respond(
                                        &mut stream,
                                        500,
                                        "OpenAI Codex login could not be completed.",
                                    );
                                    return Err(error);
                                }
                            }
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(CodexAuthError::CallbackTimedOut.into());
                    }
                    thread::sleep(CALLBACK_POLL_INTERVAL);
                }
                Err(_) => return Err(CodexAuthError::CallbackFailed.into()),
            }
        }
    }

    fn finish_login(
        &self,
        store: &CredentialStore,
        name: &str,
        code: &str,
        allow_file_fallback: bool,
    ) -> Result<CredentialBackend, AuthError> {
        let tokens = store
            .codex_client
            .exchange(code, &self.redirect_uri, &self.code_verifier)?;
        let credential = StoredCodexCredential::from_exchange(tokens, unix_time()?)?;
        let encoded = credential.encode()?;
        store.set_with_metadata(
            name,
            encoded,
            allow_file_fallback,
            Some("openai-codex"),
            Some(CREDENTIAL_ENDPOINT),
        )
    }
}

fn random_base64(byte_count: usize) -> Result<String, CodexAuthError> {
    let mut bytes = vec![0_u8; byte_count];
    getrandom::fill(&mut bytes).map_err(|_| CodexAuthError::RandomnessUnavailable)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn bind_callback(port: u16, allow_fallback: bool) -> Result<TcpListener, CodexAuthError> {
    match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => Ok(listener),
        Err(error) if allow_fallback && error.kind() == io::ErrorKind::AddrInUse => {
            TcpListener::bind(("127.0.0.1", FALLBACK_CALLBACK_PORT))
                .map_err(|_| CodexAuthError::CallbackUnavailable)
        }
        Err(_) => Err(CodexAuthError::CallbackUnavailable),
    }
}

fn authorization_url(
    redirect_uri: &str,
    state: &str,
    code_verifier: &str,
) -> Result<String, CodexAuthError> {
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));
    let mut url = Url::parse(&format!("{ISSUER}/oauth/authorize"))
        .map_err(|_| CodexAuthError::CallbackFailed)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair(
            "scope",
            "openid profile email offline_access api.connectors.read api.connectors.invoke",
        )
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", state)
        .append_pair("originator", "qq");
    Ok(url.into())
}

enum Callback {
    Ignore { status: u16, body: &'static str },
    Denied(String),
    MissingCode,
    Code(String),
}

fn read_callback(stream: &mut TcpStream, expected_state: &str, deadline: Instant) -> Callback {
    let Some(request) = read_http_head(stream, deadline) else {
        return Callback::Ignore {
            status: 400,
            body: "Bad request.",
        };
    };
    let Some(request_line) = request.lines().next() else {
        return Callback::Ignore {
            status: 400,
            body: "Bad request.",
        };
    };
    let mut parts = request_line.split_whitespace();
    let (Some("GET"), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Callback::Ignore {
            status: 400,
            body: "Bad request.",
        };
    };
    if !version.starts_with("HTTP/1.") || !target.starts_with('/') {
        return Callback::Ignore {
            status: 400,
            body: "Bad request.",
        };
    }
    let Ok(url) = Url::parse(&format!("http://localhost{target}")) else {
        return Callback::Ignore {
            status: 400,
            body: "Bad request.",
        };
    };
    if url.path() != "/auth/callback" {
        return Callback::Ignore {
            status: 404,
            body: "Not found.",
        };
    }

    let mut state = None;
    let mut code = None;
    let mut error = None;
    for (name, value) in url.query_pairs() {
        let destination = match name.as_ref() {
            "state" => &mut state,
            "code" => &mut code,
            "error" => &mut error,
            _ => continue,
        };
        if destination.replace(value.into_owned()).is_some() {
            return Callback::Ignore {
                status: 400,
                body: "Bad request.",
            };
        }
    }
    if state.as_deref() != Some(expected_state) {
        return Callback::Ignore {
            status: 400,
            body: "State mismatch.",
        };
    }
    if let Some(error) = error {
        return Callback::Denied(sanitize_error_code(&error));
    }
    match code {
        Some(code) if !code.is_empty() => Callback::Code(code),
        _ => Callback::MissingCode,
    }
}

fn read_http_head(stream: &mut TcpStream, deadline: Instant) -> Option<String> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        stream
            .set_read_timeout(Some(remaining.min(CALLBACK_IO_TIMEOUT)))
            .ok()?;
        let read = stream.read(&mut buffer).ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() > MAX_CALLBACK_BYTES {
            return None;
        }
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            return String::from_utf8(bytes).ok();
        }
    }
}

fn respond(stream: &mut TcpStream, status: u16, body: &str) -> Result<(), AuthError> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .map_err(|_| CodexAuthError::CallbackFailed)?;
    stream.flush().map_err(|_| CodexAuthError::CallbackFailed)?;
    Ok(())
}

fn sanitize_error_code(value: &str) -> String {
    let sanitized = value
        .chars()
        .take(64)
        .filter(|character| character.is_ascii_alphanumeric() || "._-".contains(*character))
        .collect::<String>();
    if sanitized.is_empty() {
        "authorization_error".to_owned()
    } else {
        sanitized
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCodexCredential {
    version: u32,
    id_token: String,
    access_token: String,
    refresh_token: String,
    account_id: String,
    is_fedramp: bool,
    refreshed_at: u64,
}

impl StoredCodexCredential {
    fn from_exchange(tokens: ExchangedTokens, refreshed_at: u64) -> Result<Self, CodexAuthError> {
        let claims = id_token_claims(&tokens.id_token)?;
        let account_id = claims.account_id.ok_or(CodexAuthError::AccountIdMissing)?;
        let credential = Self {
            version: STORED_CREDENTIAL_VERSION,
            id_token: tokens.id_token,
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            account_id,
            is_fedramp: claims.is_fedramp,
            refreshed_at,
        };
        credential.validate()?;
        Ok(credential)
    }

    fn parse(bytes: &[u8]) -> Result<Self, CodexAuthError> {
        let credential: Self =
            serde_json::from_slice(bytes).map_err(|_| CodexAuthError::StoredCredentialInvalid)?;
        if credential.version != STORED_CREDENTIAL_VERSION {
            return Err(CodexAuthError::UnsupportedStoredCredentialVersion {
                version: credential.version,
            });
        }
        credential.validate()?;
        Ok(credential)
    }

    fn encode(&self) -> Result<Vec<u8>, CodexAuthError> {
        serde_json::to_vec(self).map_err(|_| CodexAuthError::StoredCredentialInvalid)
    }

    fn validate(&self) -> Result<(), CodexAuthError> {
        if self.id_token.is_empty()
            || self.access_token.is_empty()
            || self.refresh_token.is_empty()
            || self.id_token.len() > MAX_TOKEN_BYTES
            || self.access_token.len() > MAX_TOKEN_BYTES
            || self.refresh_token.len() > MAX_TOKEN_BYTES
            || !valid_account_id(&self.account_id)
        {
            return Err(CodexAuthError::StoredCredentialInvalid);
        }
        let claims = id_token_claims(&self.id_token)?;
        if claims.account_id.as_deref() != Some(&self.account_id)
            || claims.is_fedramp != self.is_fedramp
        {
            return Err(CodexAuthError::StoredCredentialInvalid);
        }
        Ok(())
    }

    fn needs_refresh(&self, now: u64) -> bool {
        match jwt_expiration(&self.access_token) {
            Some(expiration) => {
                expiration
                    <= i64::try_from(now.saturating_add(REFRESH_WINDOW_SECONDS)).unwrap_or(i64::MAX)
            }
            None => self.refreshed_at <= now.saturating_sub(REFRESH_FALLBACK_SECONDS),
        }
    }

    fn runtime_credential(&self) -> CodexCredential {
        let refresh_after = match jwt_expiration(&self.access_token)
            .and_then(|expiration| u64::try_from(expiration).ok())
        {
            Some(expiration) => expiration.saturating_sub(REFRESH_WINDOW_SECONDS),
            None => self.refreshed_at.saturating_add(REFRESH_FALLBACK_SECONDS),
        };
        CodexCredential {
            access_token: Secret::from_secret_bytes(self.access_token.as_bytes().to_vec()),
            account_id: self.account_id.clone(),
            is_fedramp: self.is_fedramp,
            refresh_after,
        }
    }
}

fn valid_account_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ACCOUNT_ID_BYTES
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

struct IdTokenClaims {
    account_id: Option<String>,
    is_fedramp: bool,
}

fn id_token_claims(token: &str) -> Result<IdTokenClaims, CodexAuthError> {
    #[derive(Deserialize)]
    struct Claims {
        #[serde(rename = "https://api.openai.com/auth")]
        auth: Option<AuthClaims>,
    }

    #[derive(Deserialize)]
    struct AuthClaims {
        chatgpt_account_id: Option<String>,
        #[serde(default)]
        chatgpt_account_is_fedramp: bool,
    }

    // These tokens come directly from the fixed HTTPS token endpoint. Like the
    // upstream Codex client, QQ decodes claims here rather than acting as an OIDC verifier.
    let claims: Claims = decode_jwt_payload(token).ok_or(CodexAuthError::IdTokenInvalid)?;
    let auth = claims.auth.ok_or(CodexAuthError::AccountIdMissing)?;
    Ok(IdTokenClaims {
        account_id: auth.chatgpt_account_id,
        is_fedramp: auth.chatgpt_account_is_fedramp,
    })
}

fn jwt_expiration(token: &str) -> Option<i64> {
    #[derive(Deserialize)]
    struct Claims {
        exp: Option<i64>,
    }

    decode_jwt_payload::<Claims>(token)?.exp
}

fn decode_jwt_payload<T: DeserializeOwned>(token: &str) -> Option<T> {
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    if header.is_empty() || payload.is_empty() || signature.is_empty() {
        return None;
    }
    let payload = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&payload).ok()
}

/// Runtime-only Codex bearer material.
#[doc(hidden)]
#[derive(Clone)]
pub struct CodexCredential {
    access_token: Secret,
    account_id: String,
    is_fedramp: bool,
    refresh_after: u64,
}

impl CodexCredential {
    pub const fn access_token(&self) -> &Secret {
        &self.access_token
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    pub const fn is_fedramp(&self) -> bool {
        self.is_fedramp
    }

    const fn refresh_after(&self) -> u64 {
        self.refresh_after
    }
}

impl CredentialStore {
    pub fn codex_request_credentials(&self, profile: &str) -> SharedRequestCredentialProvider {
        SharedRequestCredentialProvider::new(CodexRequestCredentials::new(self, profile))
    }

    pub fn resolve_codex(&self, profile: &str) -> Result<CodexCredential, AuthError> {
        self.resolve_codex_with_epoch(profile)
            .map(|(credential, _epoch)| credential)
    }

    fn resolve_codex_with_epoch(
        &self,
        profile: &str,
    ) -> Result<(CodexCredential, CredentialEpoch), AuthError> {
        let name = credential_name(profile)?;
        let now = unix_time()?;
        let (credential, epoch) = self.load_codex_with_epoch(&name)?;
        if !credential.needs_refresh(now) {
            return Ok((credential.runtime_credential(), epoch));
        }

        let _refresh = self
            .lock_codex_operation(&name)?
            .expect("OpenAI Codex credential names always require the Codex lock");
        let (mut credential, epoch) = self.load_codex_with_epoch(&name)?;
        let now = unix_time()?;
        if !credential.needs_refresh(now) {
            return Ok((credential.runtime_credential(), epoch));
        }

        let refreshed = self.codex_client.refresh(&credential.refresh_token)?;
        if let Some(id_token) = refreshed.id_token {
            let claims = id_token_claims(&id_token)?;
            if claims.account_id.as_deref() != Some(&credential.account_id) {
                return Err(CodexAuthError::StoredCredentialInvalid.into());
            }
            credential.is_fedramp = claims.is_fedramp;
            credential.id_token = id_token;
        }
        if let Some(access_token) = refreshed.access_token {
            credential.access_token = access_token;
        }
        if let Some(refresh_token) = refreshed.refresh_token {
            credential.refresh_token = refresh_token;
        }
        credential.refreshed_at = now;
        credential.validate()?;
        if credential.needs_refresh(now) {
            return Err(CodexAuthError::TokenResponseInvalid {
                operation: "refresh",
            }
            .into());
        }

        self.replace_with_metadata_normalized(
            &name,
            &credential.encode()?,
            "openai-codex".to_owned(),
            CREDENTIAL_ENDPOINT.to_owned(),
        )?;
        // Bind the cache candidate to the exact durable value and epoch read
        // under one state lock. A separate process may rotate or remove it
        // immediately after our refresh write.
        let (credential, epoch) = self.load_codex_with_epoch(&name)?;
        let now = unix_time()?;
        if credential.needs_refresh(now) {
            return Err(CodexAuthError::TokenResponseInvalid {
                operation: "refresh",
            }
            .into());
        }
        Ok((credential.runtime_credential(), epoch))
    }

    fn load_codex_with_epoch(
        &self,
        name: &str,
    ) -> Result<(StoredCodexCredential, CredentialEpoch), AuthError> {
        let (secret, epoch) = self.resolve_with_epoch(
            &SecretRef::Stored(name.to_owned()),
            Some(CREDENTIAL_ENDPOINT),
        )?;
        let credential =
            StoredCodexCredential::parse(secret.expose_secret_bytes()).map_err(AuthError::from)?;
        Ok((credential, epoch))
    }
}

pub(super) struct CodexRequestCredentials {
    pub(super) store: CredentialStore,
    pub(super) profile: String,
    pub(super) cache: tokio::sync::Mutex<Option<CachedCodexCredential>>,
    remedies: MissingCredentialRemedies,
}

impl CodexRequestCredentials {
    pub(super) fn new(store: &CredentialStore, profile: &str) -> Self {
        // Remedy text is authored here, once, so the request path never
        // formats provider-specific guidance. Codex has no API-key variable.
        Self {
            store: store.clone(),
            profile: profile.to_owned(),
            cache: tokio::sync::Mutex::new(None),
            remedies: MissingCredentialRemedies::new(
                "openai-codex",
                profile,
                &["qq auth login openai-codex"],
                None,
            ),
        }
    }
}

#[derive(Clone)]
pub(super) struct CachedCodexCredential {
    pub(super) credential: CodexCredential,
    pub(super) epoch: CredentialEpoch,
    pub(super) refresh_after: u64,
}

impl RequestCredentialProvider for CodexRequestCredentials {
    fn credential(&self) -> RequestCredentialFuture<'_> {
        Box::pin(async move {
            let mut cache = self.cache.lock().await;
            let cached_state = cache
                .as_ref()
                .map(|cached| (cached.epoch, cached.refresh_after));
            let profile = self.profile.clone();
            let resolved = self
                .store
                .load_request_credential(move |store| {
                    let now = unix_time()?;
                    let epoch = store.epoch()?;
                    if cached_state.is_some_and(|(cached_epoch, refresh_after)| {
                        cached_epoch == epoch && now < refresh_after
                    }) {
                        return Ok(None);
                    }
                    store.resolve_codex_with_epoch(&profile).map(Some)
                })
                .await?
                .map_err(|error| map_request_credential_error(error, &self.remedies))?;
            let Some((credential, epoch)) = resolved else {
                let credential = &cache
                    .as_ref()
                    .expect("a cache hit requires a cached Codex credential")
                    .credential;
                return request_credential(credential);
            };
            let refresh_after = credential.refresh_after();
            let request_credential = request_credential(&credential)?;
            *cache = Some(CachedCodexCredential {
                credential,
                epoch,
                refresh_after,
            });
            Ok(request_credential)
        })
    }
}

fn request_credential(
    credential: &CodexCredential,
) -> Result<RequestCredential, RequestCredentialError> {
    RequestCredential::codex(
        credential
            .access_token()
            .expose_secret_str()
            .map_err(|_| RequestCredentialError::Invalid)?,
        credential.account_id(),
        credential.is_fedramp(),
    )
}

fn map_request_credential_error(
    error: AuthError,
    remedies: &MissingCredentialRemedies,
) -> RequestCredentialError {
    match error {
        AuthError::Codex(
            CodexAuthError::TokenRequestRejected { .. }
            | CodexAuthError::AuthorizationDenied { .. },
        ) => RequestCredentialError::RefreshRejected,
        AuthError::Codex(
            CodexAuthError::TokenRequestFailed { .. }
            | CodexAuthError::TokenResponseInvalid { .. }
            | CodexAuthError::CallbackTimedOut,
        ) => RequestCredentialError::RefreshUnavailable,
        AuthError::StoredCredentialNotRegistered { .. }
        | AuthError::StoredCredentialMissing { .. } => remedies.missing(&error),
        AuthError::Codex(CodexAuthError::StoredCredentialInvalid)
        | AuthError::Codex(CodexAuthError::UnsupportedStoredCredentialVersion { .. })
        | AuthError::SecretNotUnicode => RequestCredentialError::Invalid,
        _ => RequestCredentialError::StorageUnavailable,
    }
}

fn credential_name(profile: &str) -> Result<String, AuthError> {
    let name = format!("openai-codex/{profile}");
    validate_credential_name(&name)?;
    Ok(name)
}

fn unix_time() -> Result<u64, CodexAuthError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| CodexAuthError::ClockUnavailable)
}
