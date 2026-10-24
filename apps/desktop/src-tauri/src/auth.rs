//! Provider sign-in. The desktop app runs on the same machine as the server
//! it supervises, so it stores credentials exactly as `qq auth login` does:
//! the same `qq-auth` store, the same OS keyring entries. The server reads
//! credentials on every catalog request, so a stored key takes effect as
//! soon as each project re-reads its model catalog. Which providers exist,
//! how each signs in and where its key may be sent all come from the
//! engine's built-in provider table (ADR-0064), never a copy of it.

use std::{collections::HashSet, sync::Mutex};

use qq_auth::{
    AuthError, CodexLogin, CredentialStore, OPENAI_CODEX_CREDENTIAL_ENDPOINT,
    XAI_CREDENTIAL_ENDPOINT, XaiLogin,
};
use qq_config::{
    BuiltinAudience, BuiltinProvider, LoginMethod, builtin_provider, builtin_providers,
};
use serde::Serialize;

/// The profile every desktop sign-in uses, matching `qq auth login`'s default.
const PROFILE: &str = "default";

/// Emitted after any credential change so the UI re-reads provider status.
pub const AUTH_EVENT: &str = "auth-changed";

#[derive(Debug, thiserror::Error)]
pub enum SignInError {
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error("{0} is not a provider the desktop app can sign in to")]
    UnknownProvider(String),
    #[error("{0} uses browser sign-in, not an API key")]
    BrowserOnly(String),
    #[error("{0} does not offer browser sign-in; paste an API key")]
    KeyOnly(String),
    #[error("a sign-in for {0} is already waiting for the browser")]
    AlreadyPending(String),
    #[error("the API key is empty")]
    EmptyKey,
    #[error("the sign-in task stopped unexpectedly")]
    TaskFailed,
}

impl Serialize for SignInError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

/// One built-in provider and how the user can connect it.
#[derive(Debug, Serialize)]
pub struct ProviderDto {
    pub id: &'static str,
    /// Where the stored credential lives (`keyring`, `file`, …), if stored.
    pub stored: Option<String>,
    /// The environment variable that already supplies a key, if one does.
    pub environment: Option<&'static str>,
    pub accepts_key: bool,
    pub browser_sign_in: bool,
    /// The model the app selects first after this provider connects.
    pub recommended_model: String,
}

/// What the UI shows while a browser sign-in waits.
#[derive(Debug, Serialize)]
pub struct BrowserSignIn {
    pub url: String,
    /// The code to confirm on the provider's page (xAI device flow).
    pub user_code: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthChanged {
    pub provider: String,
    pub error: Option<String>,
}

/// The model route chosen for a provider with nothing selected yet: the
/// table's `qq init` pick.
pub fn recommended_model(provider: &str) -> Option<String> {
    builtin_provider(provider).map(|row| format!("{}/{}", row.id, row.init_model))
}

/// The endpoint a provider's stored key is bound to, mapped exactly as the
/// engine's composition root maps it, so a key is never sent elsewhere.
const fn audience(provider: &BuiltinProvider) -> &'static str {
    match provider.audience {
        BuiltinAudience::Table(audience) => audience,
        BuiltinAudience::OpenAiCodex => OPENAI_CODEX_CREDENTIAL_ENDPOINT,
        BuiltinAudience::XAi => XAI_CREDENTIAL_ENDPOINT,
    }
}

fn known(provider: &str) -> Result<BuiltinProvider, SignInError> {
    builtin_provider(provider).ok_or_else(|| SignInError::UnknownProvider(provider.to_owned()))
}

fn browser_sign_in(provider: &BuiltinProvider) -> bool {
    provider.login.iter().any(|method| {
        matches!(
            method,
            LoginMethod::OpenAiCodexBrowser | LoginMethod::XAiOAuth
        )
    })
}

/// Blocking: reads the credential index.
pub fn providers() -> Result<Vec<ProviderDto>, SignInError> {
    let store = CredentialStore::system()?;
    let stored = store.list()?;
    Ok(builtin_providers()
        .map(|provider| {
            let name = provider.stored_name(Some(PROFILE));
            ProviderDto {
                id: provider.id,
                stored: stored
                    .iter()
                    .find(|metadata| metadata.name == name)
                    .map(|metadata| metadata.backend.to_string()),
                environment: provider
                    .variables
                    .iter()
                    .copied()
                    .find(|variable| std::env::var_os(variable).is_some_and(|v| !v.is_empty())),
                accepts_key: provider.login.contains(&LoginMethod::ApiKey),
                browser_sign_in: browser_sign_in(&provider),
                recommended_model: format!("{}/{}", provider.id, provider.init_model),
            }
        })
        .collect())
}

/// Blocking: writes the OS keyring.
pub fn save_key(provider: &str, key: &str) -> Result<(), SignInError> {
    let provider = known(provider)?;
    if !provider.login.contains(&LoginMethod::ApiKey) {
        return Err(SignInError::BrowserOnly(provider.id.to_owned()));
    }
    let key = key.trim();
    if key.is_empty() {
        return Err(SignInError::EmptyKey);
    }
    // Stored exactly as `qq auth login`'s API-key path stores it.
    CredentialStore::system()?.set_with_metadata(
        &provider.stored_name(Some(PROFILE)),
        key,
        false,
        Some(provider.id),
        Some(audience(&provider)),
    )?;
    Ok(())
}

/// Blocking: removes the stored credential. A key supplied by an
/// environment variable is outside the store and stays in effect.
pub fn sign_out(provider: &str) -> Result<(), SignInError> {
    let provider = known(provider)?;
    CredentialStore::system()?.remove(&provider.stored_name(Some(PROFILE)))?;
    Ok(())
}

/// Browser sign-ins in flight, so a second click does not race the first
/// for the callback port or device code.
#[derive(Default)]
pub struct PendingSignIns(Mutex<HashSet<String>>);

impl PendingSignIns {
    fn claim(&self, provider: &str) -> Result<(), SignInError> {
        let mut pending = self.0.lock().map_err(|_| SignInError::TaskFailed)?;
        if !pending.insert(provider.to_owned()) {
            return Err(SignInError::AlreadyPending(provider.to_owned()));
        }
        Ok(())
    }

    pub fn release(&self, provider: &str) {
        if let Ok(mut pending) = self.0.lock() {
            pending.remove(provider);
        }
    }
}

/// A browser flow that has produced its URL and now waits for the user.
pub enum StartedSignIn {
    Codex(CodexLogin),
    Xai(XaiLogin),
}

/// Blocking: binds the OAuth callback (Codex) or requests a device code
/// (xAI). The caller opens the URL and then runs [`finish_browser`].
pub fn start_browser(
    pending: &PendingSignIns,
    provider: &str,
) -> Result<(StartedSignIn, BrowserSignIn), SignInError> {
    let provider = known(provider)?.id;
    pending.claim(provider)?;
    let started = match provider {
        "openai-codex" => CodexLogin::start().map(|login| {
            let shown = BrowserSignIn {
                url: login.authorization_url().to_owned(),
                user_code: None,
            };
            (StartedSignIn::Codex(login), shown)
        }),
        "xai" => CredentialStore::system()
            .and_then(|store| XaiLogin::start(&store))
            .map(|login| {
                let shown = BrowserSignIn {
                    url: login.verification_url().to_owned(),
                    user_code: Some(login.user_code().to_owned()),
                };
                (StartedSignIn::Xai(login), shown)
            }),
        other => {
            pending.release(other);
            return Err(SignInError::KeyOnly(other.to_owned()));
        }
    };
    if started.is_err() {
        pending.release(provider);
    }
    Ok(started?)
}

/// Blocking until the user finishes in the browser or the flow times out
/// (five minutes for Codex, the device code's lifetime for xAI).
pub fn finish_browser(started: StartedSignIn) -> Result<(), SignInError> {
    let store = CredentialStore::system()?;
    match started {
        StartedSignIn::Codex(login) => login.complete(&store, PROFILE, false)?,
        StartedSignIn::Xai(login) => login.complete(&store, PROFILE, false)?,
    };
    Ok(())
}
