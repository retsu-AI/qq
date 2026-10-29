use qq_auth::{AuthError, CredentialStore, Secret, resolve_provider_credential_with_aliases};
use qq_config::{BuiltinAudience, BuiltinProvider, SecretRef};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuiltinAuth {
    ApiKey { audience: &'static str },
    OpenAiCodex,
    XAi,
}

impl BuiltinAuth {
    pub const fn of(provider: &BuiltinProvider) -> Self {
        match provider.audience {
            BuiltinAudience::Table(audience) => Self::ApiKey { audience },
            BuiltinAudience::OpenAiCodex => Self::OpenAiCodex,
            BuiltinAudience::XAi => Self::XAi,
        }
    }

    pub const fn audience(self) -> &'static str {
        match self {
            Self::ApiKey { audience } => audience,
            Self::OpenAiCodex => qq_auth::OPENAI_CODEX_CREDENTIAL_ENDPOINT,
            Self::XAi => qq_auth::XAI_CREDENTIAL_ENDPOINT,
        }
    }

    pub(crate) const fn is_profile_scoped(self) -> bool {
        matches!(self, Self::OpenAiCodex | Self::XAi)
    }

    pub(crate) const fn digest_marker(self) -> &'static [u8] {
        match self {
            Self::ApiKey { .. } => b"built-in-api-key",
            Self::OpenAiCodex => b"request-time-codex",
            Self::XAi => b"request-time-bearer",
        }
    }

    pub(crate) const fn cache_probe_reads_store(self, api_key: Option<&SecretRef>) -> bool {
        match self {
            Self::ApiKey { .. } => matches!(api_key, None | Some(SecretRef::Stored(_))),
            Self::OpenAiCodex | Self::XAi => true,
        }
    }

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

pub(crate) fn profile_name(profile: Option<&str>) -> &str {
    profile.unwrap_or("default")
}

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

    #[test]
    fn qq_auth_reads_the_variable_the_xai_row_names() {
        let xai = qq_config::builtin_provider("xai").unwrap();
        assert_eq!(xai.variables, [qq_auth::XAI_API_KEY_VARIABLE]);
    }
}
