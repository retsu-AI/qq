//! Provider sign-in. The desktop app runs on the same machine as the server
//! it supervises, so it stores credentials exactly as `qq auth login` does:
//! the same `qq-auth` store, the same OS keyring entries. The server reads
//! credentials on every catalog request, so a stored key takes effect as
//! soon as each project re-reads its model catalog.

use std::{collections::HashSet, sync::Mutex};

use qq_auth::{AuthError, CodexLogin, CredentialStore, LOGIN_PROVIDERS, XaiLogin};
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
    pub recommended_model: &'static str,
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

/// Model routes chosen for a provider with nothing selected yet, from the
/// shipped catalog (the same picks `qq init` offers).
pub fn recommended_model(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" => Some("openai/gpt-5.6"),
        "anthropic" => Some("anthropic/claude-sonnet-5"),
        "google" => Some("google/gemini-2.5-flash"),
        "xai" => Some("xai/grok-4.6"),
        "openai-codex" => Some("openai-codex/gpt-5.6-luna"),
        _ => None,
    }
}

fn environment_variables(provider: &str) -> &'static [&'static str] {
    match provider {
        "openai" => &["OPENAI_API_KEY"],
        "anthropic" => &["ANTHROPIC_API_KEY"],
        "google" => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        "xai" => &["XAI_API_KEY"],
        _ => &[],
    }
}

fn known(provider: &str) -> Result<&'static str, SignInError> {
    LOGIN_PROVIDERS
        .iter()
        .copied()
        .find(|id| *id == provider)
        .ok_or_else(|| SignInError::UnknownProvider(provider.to_owned()))
}

/// Blocking: reads the credential index.
pub fn providers() -> Result<Vec<ProviderDto>, SignInError> {
    let store = CredentialStore::system()?;
    let stored = store.list()?;
    Ok(LOGIN_PROVIDERS
        .iter()
        .map(|&id| {
            let name = format!("{id}/{PROFILE}");
            ProviderDto {
                id,
                stored: stored
                    .iter()
                    .find(|metadata| metadata.name == name)
                    .map(|metadata| metadata.backend.to_string()),
                environment: environment_variables(id)
                    .iter()
                    .copied()
                    .find(|variable| std::env::var_os(variable).is_some_and(|v| !v.is_empty())),
                accepts_key: id != "openai-codex",
                browser_sign_in: matches!(id, "openai-codex" | "xai"),
                recommended_model: recommended_model(id).unwrap_or_default(),
            }
        })
        .collect())
}

/// Blocking: writes the OS keyring.
pub fn save_key(provider: &str, key: &str) -> Result<(), SignInError> {
    let provider = known(provider)?;
    if provider == "openai-codex" {
        return Err(SignInError::BrowserOnly(provider.to_owned()));
    }
    let key = key.trim();
    if key.is_empty() {
        return Err(SignInError::EmptyKey);
    }
    CredentialStore::system()?.login_with_api_key(provider, PROFILE, key, false)?;
    Ok(())
}

/// Blocking: removes the stored credential. A key supplied by an
/// environment variable is outside the store and stays in effect.
pub fn sign_out(provider: &str) -> Result<(), SignInError> {
    let provider = known(provider)?;
    CredentialStore::system()?.remove(&format!("{provider}/{PROFILE}"))?;
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
    let provider = known(provider)?;
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
