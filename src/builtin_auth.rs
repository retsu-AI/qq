//! What each built-in provider's credential is, in one place.
//!
//! The provider table (`qq-config`) names who owns a credential's audience;
//! `qq-auth` owns OAuth credentials. This module is where the two meet
//! (ADR-0064 §1, §6): every question the runtime, discovery and the doctor ask
//! about a built-in credential is answered here, by one match per question,
//! instead of by a match on the audience at each call site.

use qq_auth::{AuthError, CredentialStore, Secret, resolve_provider_credential_with_aliases};
use qq_config::{BuiltinAudience, BuiltinProvider, SecretRef};

/// How a built-in provider authenticates: a resolved-once API key bound to
/// the table's audience, or a credential `qq-auth` resolves per request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BuiltinAuth {
    /// An API key: explicit reference, then `<id>/<profile>`, then the row's
    /// variables, bound to this audience.
    ApiKey { audience: &'static str },
    /// The stored ChatGPT login for a profile, refreshed per request.
    OpenAiCodex,
    /// xAI: an explicit reference, a stored OAuth login or API key for a
    /// profile, then `XAI_API_KEY`; resolved per request.
    XAi,
}

impl BuiltinAuth {
    pub(crate) const fn of(provider: &BuiltinProvider) -> Self {
        match provider.audience {
            BuiltinAudience::Table(audience) => Self::ApiKey { audience },
            BuiltinAudience::OpenAiCodex => Self::OpenAiCodex,
            BuiltinAudience::XAi => Self::XAi,
        }
    }

    /// The audience a credential for this provider is bound to.
    pub(crate) const fn audience(self) -> &'static str {
        match self {
            Self::ApiKey { audience } => audience,
            Self::OpenAiCodex => qq_auth::OPENAI_CODEX_CREDENTIAL_ENDPOINT,
            Self::XAi => qq_auth::XAI_CREDENTIAL_ENDPOINT,
        }
    }

    /// Whether `qq-auth` resolves the credential per request under a
    /// profile, so the profile names the credential.
    pub(crate) const fn is_profile_scoped(self) -> bool {
        matches!(self, Self::OpenAiCodex | Self::XAi)
    }

    /// The marker the request-shape digest records. Each keeps the bytes its
    /// pre-ADR-0064 credential variant wrote, so every version 1 identity
    /// stands (ADR-0064 §4).
    pub(crate) const fn digest_marker(self) -> &'static [u8] {
        match self {
            Self::ApiKey { .. } => b"built-in-api-key",
            Self::OpenAiCodex => b"request-time-codex",
            Self::XAi => b"request-time-bearer",
        }
    }

    /// Whether a cached discovery result for this provider can be found only
    /// by reading the secure store.
    pub(crate) const fn cache_probe_reads_store(self, api_key: Option<&SecretRef>) -> bool {
        match self {
            Self::ApiKey { .. } => matches!(api_key, None | Some(SecretRef::Stored(_))),
            Self::OpenAiCodex | Self::XAi => true,
        }
    }

    /// Whether a request could authenticate now, by the same precedence and
    /// parsing the request path uses, without refreshing an OAuth login or
    /// any other network I/O.
    pub(crate) fn check(
        self,
        store: &CredentialStore,
        provider: &BuiltinProvider,
        api_key: Option<&SecretRef>,
        profile: Option<&str>,
    ) -> Result<(), AuthError> {
        match self {
            Self::ApiKey { .. } => resolve_api_key(store, provider, api_key, profile).map(|_| ()),
            Self::OpenAiCodex => store.check_codex_login(profile_name(profile)),
            Self::XAi => store.check_xai_credential(profile_name(profile), api_key),
        }
    }
}

/// The profile a profile-scoped credential is stored under.
pub(crate) fn profile_name(profile: Option<&str>) -> &str {
    profile.unwrap_or("default")
}

/// Resolves a built-in provider's API key in precedence order: the explicit
/// reference, the stored `<id>/<profile>` credential, then the row's
/// variables, all bound to the row's audience.
pub(crate) fn resolve_api_key(
    store: &CredentialStore,
    provider: &BuiltinProvider,
    api_key: Option<&SecretRef>,
    profile: Option<&str>,
) -> Result<Secret, AuthError> {
    let (variable, aliases) = provider
        .variables
        .split_first()
        .map_or(("", &[][..]), |(first, rest)| (*first, rest));
    resolve_provider_credential_with_aliases(
        store,
        api_key,
        &provider.stored_name(profile),
        variable,
        aliases,
        Some(BuiltinAuth::of(provider).audience()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The digest markers are a persisted contract: changing one changes
    /// every plan identity on that route (ADR-0064 §4).
    #[test]
    fn every_builtin_row_keeps_its_pinned_digest_marker_and_audience() {
        let expected: &[(&str, &[u8], &str, bool)] = &[
            (
                "openai",
                b"built-in-api-key",
                "https://api.openai.com",
                false,
            ),
            (
                "openai-codex",
                b"request-time-codex",
                "https://chatgpt.com",
                true,
            ),
            (
                "anthropic",
                b"built-in-api-key",
                "https://api.anthropic.com",
                false,
            ),
            (
                "google",
                b"built-in-api-key",
                "https://generativelanguage.googleapis.com",
                false,
            ),
            ("xai", b"request-time-bearer", "https://api.x.ai", true),
        ];
        let rows: Vec<_> = qq_config::builtin_providers().collect();
        assert_eq!(rows.len(), expected.len(), "a new row needs an entry here");
        for (id, marker, audience, profile_scoped) in expected {
            let provider = qq_config::builtin_provider(id).unwrap();
            let auth = BuiltinAuth::of(&provider);
            assert_eq!(auth.digest_marker(), *marker, "{id}");
            assert_eq!(auth.audience(), *audience, "{id}");
            assert_eq!(auth.is_profile_scoped(), *profile_scoped, "{id}");
        }
    }

    /// `qq-auth` cannot read the table (ADR-0064 §6), so the variable xAI
    /// falls back to is spelled there too; the two must agree.
    #[test]
    fn qq_auth_reads_the_variable_the_xai_row_names() {
        let xai = qq_config::builtin_provider("xai").unwrap();
        assert_eq!(xai.variables, [qq_auth::XAI_API_KEY_VARIABLE]);
    }
}
