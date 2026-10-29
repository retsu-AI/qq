use std::{
    collections::{BTreeMap, VecDeque},
    io::Read,
    net::IpAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use hmac::{Hmac, Mac};
use qq_auth::{CredentialStore, Secret};
use qq_config::{
    EndpointMode, HttpAccess, HttpCredential, ModelDiscoveryShape, ProviderApi, ProviderAuth,
    ProviderConfig, ProviderKind, SecretRef,
};
use qq_provider::RequestCredential;

use crate::builtin_auth::{self, BuiltinAuth};
use reqwest::{Url, blocking::RequestBuilder};
use sha2::Sha256;
use thiserror::Error;

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_DISCOVERED_MODELS: usize = 4_096;
const MAX_CACHE_ENTRIES: usize = 32;
const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const FAILURE_CACHE_TTL: Duration = Duration::from_secs(5);
const CODEX_MODELS_CLIENT_VERSION: &str = "0.159.0";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiscoveredModel {
    pub(crate) id: String,
    pub(crate) name: Option<String>,
    pub(crate) efforts: Option<Vec<qq_provider::ReasoningEffort>>,
}

pub(crate) struct ModelDiscovery {
    cache: Mutex<VecDeque<CacheEntry>>,
    fetch_gates: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    cache_key: [u8; 32],
}

#[derive(PartialEq, Eq)]
struct CacheProbe {
    provider_id: String,
    kind: ProviderKind,
    access: HttpAccess,
    credential_index: PathBuf,
    credential_epoch: qq_protocol::CredentialEpoch,
    environment: [u8; 32],
}

struct CacheEntry {
    key: [u8; 32],
    probe: Option<CacheProbe>,
    expires_at: Instant,
    models: Option<Vec<DiscoveredModel>>,
}

#[derive(Debug, Error)]
pub enum ModelDiscoveryError {
    #[error("model-discovery cache key generation failed")]
    Random,
}

impl ModelDiscovery {
    pub(crate) fn new() -> Result<Self, ModelDiscoveryError> {
        let mut cache_key = [0_u8; 32];
        getrandom::fill(&mut cache_key).map_err(|_| ModelDiscoveryError::Random)?;
        Ok(Self {
            cache: Mutex::new(VecDeque::new()),
            fetch_gates: Mutex::new(BTreeMap::new()),
            cache_key,
        })
    }

    pub(crate) fn discover(
        &self,
        provider_id: &str,
        provider: &ProviderConfig,
        credentials: &CredentialStore,
    ) -> Option<Vec<DiscoveredModel>> {
        let access = match provider.access()? {
            qq_config::ProviderAccess::Http(access) => access,
            qq_config::ProviderAccess::AmazonBedrock { .. }
            | qq_config::ProviderAccess::AmazonBedrockMantle { .. } => return None,
        };
        if let Some(models) = self.cached(provider_id, provider, credentials) {
            return Some(models);
        }
        let gate = Arc::clone(
            self.fetch_gates
                .lock()
                .ok()?
                .entry(provider_id.to_owned())
                .or_default(),
        );
        let _fetch = gate.lock().ok()?;
        if let Some(models) = self.cached(provider_id, provider, credentials) {
            return Some(models);
        }
        let probe = self.cache_probe(provider_id, provider.kind(), access, credentials);
        let auth = resolve_auth(access, credentials)?;
        let Some(key) = cache_key(
            &self.cache_key,
            provider_id,
            provider.kind(),
            access,
            auth.as_ref(),
        ) else {
            return self.fetch(provider.kind(), access, auth.as_ref());
        };
        let now = Instant::now();
        if let Ok(mut cache) = self.cache.lock() {
            cache.retain(|entry| entry.expires_at > now);
            if let Some(position) = cache.iter().position(|entry| entry.key == key) {
                let mut entry = cache.remove(position)?;
                entry.probe = probe;
                let models = entry.models.clone();
                cache.push_back(entry);
                return models;
            }
        }

        let models = self.fetch(provider.kind(), access, auth.as_ref());
        if let Ok(mut cache) = self.cache.lock() {
            cache.push_back(CacheEntry {
                key,
                probe,
                expires_at: now
                    + if models.is_some() {
                        CACHE_TTL
                    } else {
                        FAILURE_CACHE_TTL
                    },
                models: models.clone(),
            });
            while cache.len() > MAX_CACHE_ENTRIES {
                cache.pop_front();
            }
        }
        models
    }

    pub(crate) fn cached(
        &self,
        provider_id: &str,
        provider: &ProviderConfig,
        credentials: &CredentialStore,
    ) -> Option<Vec<DiscoveredModel>> {
        let qq_config::ProviderAccess::Http(access) = provider.access()? else {
            return None;
        };
        if cache_probe_requires_secure_store(access.auth()) {
            if !self.cache.lock().ok()?.iter().any(|entry| {
                entry.expires_at > Instant::now()
                    && entry.probe.as_ref().is_some_and(|probe| {
                        probe.provider_id == provider_id
                            && probe.kind == provider.kind()
                            && probe.access == *access
                    })
            }) {
                return None;
            }

            let probe = self.cache_probe(provider_id, provider.kind(), access, credentials)?;
            return self
                .cache
                .lock()
                .ok()?
                .iter()
                .rev()
                .find(|entry| {
                    entry.probe.as_ref() == Some(&probe) && entry.expires_at > Instant::now()
                })
                .and_then(|entry| entry.models.clone());
        }
        let auth = resolve_auth(access, credentials)?;
        let key = cache_key(
            &self.cache_key,
            provider_id,
            provider.kind(),
            access,
            auth.as_ref(),
        )?;
        self.cache
            .lock()
            .ok()?
            .iter()
            .find(|entry| entry.key == key && entry.expires_at > Instant::now())
            .and_then(|entry| entry.models.clone())
    }

    fn cache_probe(
        &self,
        provider_id: &str,
        kind: ProviderKind,
        access: &HttpAccess,
        credentials: &CredentialStore,
    ) -> Option<CacheProbe> {
        if !cache_probe_requires_secure_store(access.auth()) {
            return None;
        }
        let mut environment = Hmac::<Sha256>::new_from_slice(&self.cache_key).ok()?;
        let mut variable = |name: &str| {
            update_digest(&mut environment, name.as_bytes());
            match std::env::var_os(name) {
                Some(value) => {
                    environment.update(&[1]);
                    update_digest(&mut environment, value.as_encoded_bytes());
                }
                None => environment.update(&[0]),
            }
        };
        match access.auth() {
            HttpCredential::Builtin {
                provider, api_key, ..
            } => {
                for name in provider.variables {
                    variable(name);
                }
                if let Some(SecretRef::Env(name)) = api_key {
                    variable(name);
                }
            }
            HttpCredential::Configured(_) => {}
        }
        Some(CacheProbe {
            provider_id: provider_id.to_owned(),
            kind,
            access: access.clone(),
            credential_index: credentials.paths().index_file().to_owned(),
            credential_epoch: credentials.epoch().ok()?,
            environment: environment.finalize().into_bytes().into(),
        })
    }

    fn fetch(
        &self,
        kind: ProviderKind,
        access: &HttpAccess,
        auth: Option<&RequestCredential>,
    ) -> Option<Vec<DiscoveredModel>> {
        let (mut endpoint, direct) = models_endpoint(access)?;
        let shape = kind.discovery();
        match (shape, access.api()) {
            (_, ProviderApi::AnthropicMessages) => {
                endpoint.query_pairs_mut().append_pair("limit", "1000");
            }
            (_, ProviderApi::GoogleGenerateContent) => {
                endpoint.query_pairs_mut().append_pair("pageSize", "1000");
            }
            (ModelDiscoveryShape::Codex, _) => {
                endpoint
                    .query_pairs_mut()
                    .append_pair("client_version", CODEX_MODELS_CLIENT_VERSION);
            }
            _ => {}
        }
        let client = discovery_client(direct)?;
        let mut models = Vec::new();
        let mut cursor: Option<String> = None;
        let mut total_bytes = 0_usize;
        let deadline = Instant::now() + Duration::from_secs(10);
        for _ in 0..16 {
            let mut url = endpoint.clone();
            if let Some(cursor) = &cursor {
                url.query_pairs_mut().append_pair("after_id", cursor);
            }
            let mut request = apply_static_headers(client.get(url), access)
                .timeout(deadline.checked_duration_since(Instant::now())?);
            if access.api() == ProviderApi::AnthropicMessages {
                request = request.header("anthropic-version", "2023-06-01");
            }
            if let Some(auth) = auth {
                request = request.headers(auth.headers());
            }
            let response = request.send().ok()?;
            if !response.status().is_success() {
                return None;
            }
            let mut bytes = Vec::new();
            response
                .take((MAX_RESPONSE_BYTES.saturating_sub(total_bytes) + 1) as u64)
                .read_to_end(&mut bytes)
                .ok()?;
            total_bytes = total_bytes.checked_add(bytes.len())?;
            if total_bytes > MAX_RESPONSE_BYTES {
                return None;
            }
            let body: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
            models.extend(parse_models(&body, shape, access.api())?);
            if models.len() > MAX_DISCOVERED_MODELS {
                return None;
            }
            if access.api() != ProviderApi::AnthropicMessages
                || body.get("has_more").and_then(serde_json::Value::as_bool) != Some(true)
            {
                models.sort_by(|a, b| a.id.cmp(&b.id));
                models.dedup_by(|a, b| a.id == b.id);
                return Some(models);
            }
            let next = body.get("last_id")?.as_str()?;
            if !valid_model_id(next) || cursor.as_deref() == Some(next) {
                return None;
            }
            cursor = Some(next.to_owned());
        }
        None
    }
}

fn discovery_client(direct: bool) -> Option<reqwest::blocking::Client> {
    let mut client = reqwest::blocking::Client::builder()
        .use_rustls_tls()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(2))
        .user_agent(concat!("qq/", env!("CARGO_PKG_VERSION")));
    if direct {
        client = client.no_proxy();
    }
    client.build().ok()
}

fn cache_key(
    key: &[u8; 32],
    provider_id: &str,
    kind: ProviderKind,
    access: &HttpAccess,
    auth: Option<&RequestCredential>,
) -> Option<[u8; 32]> {
    let mut digest = Hmac::<Sha256>::new_from_slice(key).ok()?;
    update_digest(&mut digest, provider_id.as_bytes());
    update_digest(&mut digest, kind.name().as_bytes());
    digest.update(&[access.api() as u8, access.endpoint_mode() as u8]);
    update_digest(&mut digest, access.endpoint().as_bytes());
    for (name, value) in access.headers() {
        update_digest(&mut digest, name.as_bytes());
        update_digest(&mut digest, value.expose_value().as_bytes());
    }
    match auth {
        None => update_digest(&mut digest, b"no-auth"),
        Some(auth) => {
            for (name, value) in &auth.headers() {
                update_digest(&mut digest, name.as_str().as_bytes());
                update_digest(&mut digest, value.as_bytes());
            }
        }
    }
    Some(digest.finalize().into_bytes().into())
}

fn update_digest(digest: &mut Hmac<Sha256>, value: &[u8]) {
    digest.update(&(value.len() as u64).to_le_bytes());
    digest.update(value);
}

fn models_endpoint(access: &HttpAccess) -> Option<(Url, bool)> {
    let mut endpoint = validate_endpoint(access.endpoint())?;
    let direct = endpoint.scheme() == "http";
    endpoint.set_query(None);
    endpoint.set_fragment(None);
    {
        let mut segments = endpoint.path_segments_mut().ok()?;
        segments.pop_if_empty();
        if access.endpoint_mode() == EndpointMode::Exact {
            segments.pop();
        }
        segments.push("models");
    }
    Some((endpoint, direct))
}

fn validate_endpoint(endpoint: &str) -> Option<Url> {
    let url = Url::parse(endpoint).ok()?;
    if url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
    {
        return None;
    }
    match url.scheme() {
        "https" => Some(url),
        "http" if is_loopback_host(&url) => Some(url),
        _ => None,
    }
}

fn is_loopback_host(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let address = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    address
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

fn apply_static_headers(mut request: RequestBuilder, access: &HttpAccess) -> RequestBuilder {
    for (name, value) in access.headers() {
        request = request.header(name, value.expose_value());
    }
    request
}

fn cache_probe_requires_secure_store(auth: &HttpCredential) -> bool {
    match auth {
        HttpCredential::Configured(ProviderAuth::NoAuth) => false,
        HttpCredential::Configured(
            ProviderAuth::ApiKey(reference)
            | ProviderAuth::Bearer(reference)
            | ProviderAuth::Header(_, reference),
        ) => matches!(reference, SecretRef::Stored(_)),
        HttpCredential::Builtin {
            provider, api_key, ..
        } => BuiltinAuth::of(provider).cache_probe_reads_store(api_key.as_ref()),
    }
}

fn resolve_auth(
    access: &HttpAccess,
    credentials: &CredentialStore,
) -> Option<Option<RequestCredential>> {
    let configured = |reference| {
        credentials
            .resolve_with_endpoint(reference, Some(access.endpoint()))
            .ok()
    };
    let credential = match access.auth() {
        HttpCredential::Configured(ProviderAuth::NoAuth) => return Some(None),
        HttpCredential::Configured(ProviderAuth::ApiKey(reference)) => {
            api_key(access.api(), &configured(reference)?)
        }
        HttpCredential::Configured(ProviderAuth::Bearer(reference)) => {
            RequestCredential::bearer(configured(reference)?.expose_secret_str().ok()?)
        }
        HttpCredential::Configured(ProviderAuth::Header(name, reference)) => {
            RequestCredential::header(name, configured(reference)?.expose_secret_str().ok()?)
        }
        HttpCredential::Builtin {
            provider,
            api_key: key,
            profile,
        } => {
            let profile = profile.as_deref();
            match BuiltinAuth::of(provider) {
                BuiltinAuth::ApiKey { .. } => api_key(
                    access.api(),
                    &builtin_auth::resolve_api_key(credentials, provider, key.as_ref(), profile)
                        .ok()?,
                ),
                BuiltinAuth::OpenAiCodex => {
                    let credential = credentials
                        .resolve_codex(builtin_auth::profile_name(profile))
                        .ok()?;
                    RequestCredential::codex(
                        credential.access_token().expose_secret_str().ok()?,
                        credential.account_id(),
                        credential.is_fedramp(),
                    )
                }
                BuiltinAuth::XAi => RequestCredential::bearer(
                    credentials
                        .resolve_xai(builtin_auth::profile_name(profile), key.as_ref())
                        .ok()?
                        .expose_secret_str()
                        .ok()?,
                ),
            }
        }
    };
    Some(Some(credential.ok()?))
}

fn api_key(
    api: ProviderApi,
    secret: &Secret,
) -> Result<RequestCredential, qq_provider::RequestCredentialError> {
    let secret = secret
        .expose_secret_str()
        .map_err(|_| qq_provider::RequestCredentialError::Invalid)?;
    match api {
        ProviderApi::AnthropicMessages => RequestCredential::header("x-api-key", secret),
        ProviderApi::GoogleGenerateContent => RequestCredential::header("x-goog-api-key", secret),
        ProviderApi::OpenAiResponses
        | ProviderApi::OpenAiChatCompletions
        | ProviderApi::BedrockConverse => RequestCredential::bearer(secret),
    }
}

fn parse_models(
    body: &serde_json::Value,
    shape: ModelDiscoveryShape,
    api: ProviderApi,
) -> Option<Vec<DiscoveredModel>> {
    let codex = shape == ModelDiscoveryShape::Codex;
    let entries = if api == ProviderApi::GoogleGenerateContent || codex {
        body.get("models")?.as_array()?
    } else {
        body.get("data")?.as_array()?
    };
    if shape.is_authoritative() && entries.len() > MAX_DISCOVERED_MODELS {
        return None;
    }
    let mut models = Vec::with_capacity(entries.len().min(MAX_DISCOVERED_MODELS));
    for entry in entries.iter().take(MAX_DISCOVERED_MODELS) {
        if codex {
            match entry.get("visibility") {
                None => {}
                Some(value) if value.as_str() == Some("list") => {}
                Some(value) if value.as_str() == Some("hide") => continue,
                Some(_) => return None,
            }
        }
        if api == ProviderApi::GoogleGenerateContent
            && entry
                .get("supportedGenerationMethods")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|methods| {
                    !methods
                        .iter()
                        .any(|method| method.as_str() == Some("generateContent"))
                })
        {
            continue;
        }
        let Some(id) = entry
            .get("id")
            .or_else(|| entry.get("slug"))
            .or_else(|| entry.get("name"))
            .and_then(serde_json::Value::as_str)
            .map(|id| id.strip_prefix("models/").unwrap_or(id))
            .filter(|id| valid_model_id(id))
        else {
            if shape.is_authoritative() {
                return None;
            }
            continue;
        };
        let name = entry
            .get("display_name")
            .or_else(|| entry.get("displayName"))
            .or_else(|| entry.get("title"))
            .and_then(serde_json::Value::as_str)
            .filter(|name| {
                !name.is_empty() && name.len() <= 512 && !name.chars().any(char::is_control)
            })
            .map(str::to_owned);
        let efforts = entry
            .get("supported_reasoning_levels")
            .and_then(serde_json::Value::as_array)
            .map(|levels| {
                let mut efforts = Vec::new();
                for level in levels {
                    if let Some(value) = level.get("effort").and_then(serde_json::Value::as_str)
                        && let Some(effort) = qq_provider::ReasoningEffort::ALL
                            .into_iter()
                            .find(|effort| effort.as_str() == value)
                        && !efforts.contains(&effort)
                    {
                        efforts.push(effort);
                    }
                }
                efforts
            });
        let efforts = if api == ProviderApi::AnthropicMessages {
            match entry.pointer("/capabilities/effort") {
                Some(capability) if !capability.is_null() => {
                    let supported = capability.get("supported")?.as_bool()?;
                    let mut levels = Vec::new();
                    if supported {
                        for level in [
                            qq_provider::ReasoningEffort::Low,
                            qq_provider::ReasoningEffort::Medium,
                            qq_provider::ReasoningEffort::High,
                            qq_provider::ReasoningEffort::Xhigh,
                            qq_provider::ReasoningEffort::Max,
                        ] {
                            if let Some(value) =
                                capability.get(level.as_str()).filter(|v| !v.is_null())
                                && value.get("supported")?.as_bool()?
                            {
                                levels.push(level);
                            }
                        }
                    }
                    Some(levels)
                }
                _ => None,
            }
        } else {
            efforts
        };
        models.push(DiscoveredModel {
            efforts,
            id: id.to_owned(),
            name,
        });
    }
    models.sort_by(|left, right| left.id.cmp(&right.id));
    models.dedup_by(|left, right| left.id == right.id);
    Some(models)
}

fn valid_model_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 512 && !id.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        io::{Read as _, Write as _},
        net::TcpListener,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
    };

    use super::*;
    use qq_auth::{CredentialPaths, KeyringBackend, KeyringError};
    use qq_config::{ProviderAccess, SecretRef, UsageType};

    #[derive(Default)]
    struct ReadCountingKeyring {
        reads: AtomicUsize,
        values: Mutex<BTreeMap<String, Vec<u8>>>,
    }

    impl ReadCountingKeyring {
        fn reads(&self) -> usize {
            self.reads.load(Ordering::Relaxed)
        }
    }

    impl KeyringBackend for ReadCountingKeyring {
        fn get(&self, name: &str) -> Result<Vec<u8>, KeyringError> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.values
                .lock()
                .unwrap()
                .get(name)
                .cloned()
                .ok_or(KeyringError::Missing)
        }

        fn set(&self, name: &str, secret: &[u8]) -> Result<(), KeyringError> {
            self.values
                .lock()
                .unwrap()
                .insert(name.to_owned(), secret.to_vec());
            Ok(())
        }

        fn remove(&self, name: &str) -> Result<(), KeyringError> {
            self.values
                .lock()
                .unwrap()
                .remove(name)
                .map(|_| ())
                .ok_or(KeyringError::Missing)
        }
    }

    fn builtin_kind(id: &str) -> ProviderKind {
        ProviderKind::Builtin(qq_config::builtin_provider(id).unwrap())
    }

    fn builtin(id: &str) -> HttpCredential {
        HttpCredential::Builtin {
            provider: qq_config::builtin_provider(id).unwrap(),
            api_key: None,
            profile: None,
        }
    }

    fn test_provider(kind: ProviderKind, endpoint: String, auth: HttpCredential) -> ProviderConfig {
        ProviderConfig::new(
            kind,
            Some(ProviderAccess::Http(HttpAccess::new(
                endpoint,
                EndpointMode::Exact,
                ProviderApi::OpenAiResponses,
                auth,
                BTreeMap::new(),
            ))),
            UsageType::Unknown,
            BTreeMap::new(),
        )
    }

    #[test]
    fn malformed_codex_entries_do_not_replace_the_fallback_catalog() {
        for entry in [
            serde_json::json!({"slug": 123, "visibility": "list"}),
            serde_json::json!({"slug": "valid", "visibility": false}),
            serde_json::json!({"slug": "valid", "visibility": "unknown"}),
        ] {
            assert!(
                parse_models(
                    &serde_json::json!({"models": [entry]}),
                    builtin_kind("openai-codex").discovery(),
                    ProviderApi::OpenAiResponses
                )
                .is_none()
            );
        }
        assert_eq!(
            parse_models(
                &serde_json::json!({"models": []}),
                builtin_kind("openai-codex").discovery(),
                ProviderApi::OpenAiResponses
            ),
            Some(Vec::new())
        );
    }

    #[test]
    fn codex_discovery_filters_hidden_models_and_rejects_partial_catalogs() {
        let body = serde_json::json!({"models": [
            {"slug": "gpt-6-sol", "visibility": "list"},
            {"slug": "gpt-6-luna"},
            {"slug": "retired", "visibility": "hide"}
        ]});
        let models = parse_models(
            &body,
            builtin_kind("openai-codex").discovery(),
            ProviderApi::OpenAiResponses,
        )
        .unwrap();
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["gpt-6-luna", "gpt-6-sol"]
        );
        let oversized = serde_json::json!({"models": vec![serde_json::json!({"slug": "test"}); MAX_DISCOVERED_MODELS + 1]});
        assert!(
            parse_models(
                &oversized,
                builtin_kind("openai-codex").discovery(),
                ProviderApi::OpenAiResponses
            )
            .is_none()
        );
    }

    #[test]
    fn codex_discovery_sends_supported_client_version_and_returns_astra() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let length = stream.read(&mut request).unwrap();
            let request = std::str::from_utf8(&request[..length]).unwrap();
            assert!(request.starts_with("GET /v1/models?client_version=0.159.0 HTTP/1.1\r\n"));
            let headers = request.to_ascii_lowercase();
            assert!(headers.contains("authorization: bearer test-token\r\n"));
            assert!(headers.contains("chatgpt-account-id: test-account\r\n"));
            assert!(headers.contains("originator: qq\r\n"));
            let body = r#"{"models":[{"slug":"gpt-6-astra","display_name":"GPT-6 Astra"}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let access = HttpAccess::new(
            format!("http://{address}/v1/responses"),
            EndpointMode::Exact,
            ProviderApi::OpenAiResponses,
            builtin("openai-codex"),
            BTreeMap::new(),
        );
        let discovery = ModelDiscovery::new().unwrap();

        let models = discovery
            .fetch(
                builtin_kind("openai-codex"),
                &access,
                Some(&RequestCredential::codex("test-token", "test-account", false).unwrap()),
            )
            .unwrap();
        server.join().unwrap();

        assert_eq!(
            models,
            [DiscoveredModel {
                efforts: None,
                id: "gpt-6-astra".to_owned(),
                name: Some("GPT-6 Astra".to_owned()),
            }]
        );
    }

    #[test]
    fn discovers_authenticated_models_from_validated_loopback_endpoints() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let length = stream.read(&mut request).unwrap();
            let request = std::str::from_utf8(&request[..length]).unwrap();
            assert!(request.starts_with("GET /v1/models HTTP/1.1\r\n"));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer test-token\r\n")
            );
            let body = r#"{"data":[{"id":"live-model","display_name":"Live model"}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let keyring = Arc::new(ReadCountingKeyring::default());
        let credentials =
            CredentialStore::with_backend(CredentialPaths::new(directory.path()), keyring.clone());
        credentials
            .set("custom/stored", "test-token", false)
            .unwrap();
        let reference = SecretRef::Stored("custom/stored".to_owned());
        let provider = ProviderConfig::new(
            ProviderKind::Custom,
            Some(ProviderAccess::Http(HttpAccess::new(
                format!("http://{address}/v1/responses"),
                EndpointMode::Exact,
                ProviderApi::OpenAiResponses,
                HttpCredential::Configured(ProviderAuth::Bearer(reference.clone())),
                BTreeMap::new(),
            ))),
            qq_config::UsageType::Unknown,
            BTreeMap::new(),
        );
        let discovery = ModelDiscovery::new().unwrap();

        let first = discovery
            .discover("custom", &provider, &credentials)
            .unwrap();
        server.join().unwrap();

        let reads = keyring.reads();
        assert_eq!(
            discovery.cached("custom", &provider, &credentials),
            Some(first.clone())
        );
        assert_eq!(
            keyring.reads(),
            reads,
            "a warm cache probe read the keyring"
        );
        let other_directory = tempfile::tempdir().unwrap();
        let other_credentials = CredentialStore::with_backend(
            CredentialPaths::new(other_directory.path()),
            keyring.clone(),
        );
        other_credentials
            .set("custom/stored", "test-token", false)
            .unwrap();
        assert_eq!(
            discovery.cached("custom", &provider, &other_credentials),
            None
        );
        assert_eq!(
            discovery.cached("another-provider", &provider, &credentials),
            None
        );
        let changed_endpoint = test_provider(
            ProviderKind::Custom,
            format!("http://{address}/different/responses"),
            HttpCredential::Configured(ProviderAuth::Bearer(reference)),
        );
        assert_eq!(
            discovery.cached("custom", &changed_endpoint, &credentials),
            None
        );
        {
            let mut cache = discovery.cache.lock().unwrap();
            cache.back_mut().unwrap().expires_at = Instant::now() - Duration::from_secs(1);
        }
        assert_eq!(discovery.cached("custom", &provider, &credentials), None);
        discovery
            .cache
            .lock()
            .unwrap()
            .back_mut()
            .unwrap()
            .expires_at = Instant::now() + CACHE_TTL;
        credentials
            .set("custom/stored", "rotated-token", false)
            .unwrap();
        assert_eq!(discovery.cached("custom", &provider, &credentials), None);
        assert_eq!(keyring.reads(), reads, "invalidation read the keyring");

        assert_eq!(
            first,
            [DiscoveredModel {
                efforts: None,
                id: "live-model".to_owned(),
                name: Some("Live model".to_owned())
            }]
        );
    }

    #[test]
    fn anthropic_discovery_collects_pages_before_publishing() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for (suffix, body) in [
                (
                    "limit=1000",
                    r#"{"data":[{"id":"claude-opus-5-5"}],"has_more":true,"last_id":"claude-opus-5-5"}"#,
                ),
                (
                    "limit=1000&after_id=claude-opus-5-5",
                    r#"{"data":[{"id":"claude-sonnet-5"}],"has_more":false}"#,
                ),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 4096];
                let n = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..n]);
                assert!(request.starts_with(&format!("GET /v1/models?{suffix} HTTP/1.1")));
                assert!(request.contains("anthropic-version: 2023-06-01"));
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let access = HttpAccess::new(
            format!("http://{address}/v1"),
            EndpointMode::Base,
            ProviderApi::AnthropicMessages,
            HttpCredential::Configured(ProviderAuth::NoAuth),
            BTreeMap::new(),
        );
        let models = ModelDiscovery::new()
            .unwrap()
            .fetch(builtin_kind("anthropic"), &access, None)
            .unwrap();
        server.join().unwrap();
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["claude-opus-5-5", "claude-sonnet-5"]
        );
    }

    #[test]
    fn discovery_keeps_exact_supported_efforts_without_inventing_unknown_levels() {
        let body = serde_json::json!({"models": [
            {"slug":"gpt-6-sol","supported_reasoning_levels":[{"effort":"low"},{"effort":"max"},{"effort":"future"},{"effort":"low"}]},
            {"slug":"unknown"},
            {"slug":"unsupported","supported_reasoning_levels":[]}
        ]});
        let models = parse_models(
            &body,
            builtin_kind("openai-codex").discovery(),
            ProviderApi::OpenAiResponses,
        )
        .unwrap();
        assert_eq!(
            models[0].efforts,
            Some(vec![
                qq_provider::ReasoningEffort::Low,
                qq_provider::ReasoningEffort::Max
            ])
        );
        assert_eq!(models[1].efforts, None);
        assert_eq!(models[2].efforts, Some(vec![]));
    }

    #[test]
    fn cached_openai_codex_does_not_read_keyring() {
        let directory = tempfile::tempdir().unwrap();
        let keyring = Arc::new(ReadCountingKeyring::default());
        let credentials =
            CredentialStore::with_backend(CredentialPaths::new(directory.path()), keyring.clone());
        credentials
            .set_with_metadata(
                "openai-codex/default",
                b"registered-codex-credential",
                false,
                Some("openai-codex"),
                Some("https://chatgpt.com"),
            )
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/v1/responses", listener.local_addr().unwrap());
        let provider = test_provider(
            builtin_kind("openai-codex"),
            endpoint.clone(),
            builtin("openai-codex"),
        );
        let discovery = ModelDiscovery::new().unwrap();

        assert_eq!(
            discovery.cached("openai-codex", &provider, &credentials),
            None
        );
        assert_eq!(keyring.reads(), 0, "an empty cache probe read Keychain");

        let safe_provider = test_provider(
            ProviderKind::Custom,
            endpoint,
            HttpCredential::Configured(ProviderAuth::NoAuth),
        );
        let ProviderAccess::Http(safe_access) = safe_provider.access().unwrap() else {
            unreachable!()
        };
        let safe_key = cache_key(
            &discovery.cache_key,
            "safe",
            ProviderKind::Custom,
            safe_access,
            None,
        )
        .unwrap();
        discovery.cache.lock().unwrap().push_back(CacheEntry {
            key: safe_key,
            probe: None,
            expires_at: Instant::now() + CACHE_TTL,
            models: Some(vec![DiscoveredModel {
                id: "safe-model".to_owned(),
                name: None,
                efforts: None,
            }]),
        });

        assert_eq!(
            discovery.cached("openai-codex", &provider, &credentials),
            None
        );
        assert_eq!(keyring.reads(), 0, "an unrelated cache entry read Keychain");
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn cached_stored_reference_and_xai_do_not_read_keyring_but_no_auth_hits() {
        let directory = tempfile::tempdir().unwrap();
        let keyring = Arc::new(ReadCountingKeyring::default());
        let credentials =
            CredentialStore::with_backend(CredentialPaths::new(directory.path()), keyring.clone());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/v1/responses", listener.local_addr().unwrap());
        credentials
            .set_with_metadata(
                "custom/stored",
                b"stored-test-secret",
                false,
                Some("custom"),
                Some(&endpoint),
            )
            .unwrap();
        credentials
            .set_with_metadata(
                "xai/default",
                b"xai-test-secret",
                false,
                Some("xai"),
                Some("https://api.x.ai"),
            )
            .unwrap();
        let stored = test_provider(
            ProviderKind::Custom,
            endpoint.clone(),
            HttpCredential::Configured(ProviderAuth::Bearer(SecretRef::Stored(
                "custom/stored".to_owned(),
            ))),
        );
        let xai = test_provider(builtin_kind("xai"), endpoint.clone(), builtin("xai"));
        let builtin_fallback =
            test_provider(builtin_kind("openai"), endpoint.clone(), builtin("openai"));
        let safe = test_provider(
            ProviderKind::Custom,
            endpoint.clone(),
            HttpCredential::Configured(ProviderAuth::NoAuth),
        );
        let value_reference: SecretRef = ron::from_str(r#"Value("inline-safe")"#).unwrap();
        let value = test_provider(
            ProviderKind::Custom,
            endpoint,
            HttpCredential::Configured(ProviderAuth::Bearer(value_reference)),
        );
        let discovery = ModelDiscovery::new().unwrap();
        let ProviderAccess::Http(safe_access) = safe.access().unwrap() else {
            unreachable!()
        };
        let safe_key = cache_key(
            &discovery.cache_key,
            "safe",
            ProviderKind::Custom,
            safe_access,
            None,
        )
        .unwrap();
        let safe_models = vec![DiscoveredModel {
            id: "safe-model".to_owned(),
            name: Some("Safe model".to_owned()),
            efforts: None,
        }];
        discovery.cache.lock().unwrap().push_back(CacheEntry {
            key: safe_key,
            probe: None,
            expires_at: Instant::now() + CACHE_TTL,
            models: Some(safe_models.clone()),
        });
        let ProviderAccess::Http(value_access) = value.access().unwrap() else {
            unreachable!()
        };
        let value_auth = resolve_auth(value_access, &credentials).unwrap();
        let value_key = cache_key(
            &discovery.cache_key,
            "value",
            ProviderKind::Custom,
            value_access,
            value_auth.as_ref(),
        )
        .unwrap();
        let value_models = vec![DiscoveredModel {
            id: "value-model".to_owned(),
            name: Some("Value model".to_owned()),
            efforts: None,
        }];
        discovery.cache.lock().unwrap().push_back(CacheEntry {
            key: value_key,
            probe: None,
            expires_at: Instant::now() + CACHE_TTL,
            models: Some(value_models.clone()),
        });

        assert_eq!(discovery.cached("stored", &stored, &credentials), None);
        assert_eq!(discovery.cached("xai", &xai, &credentials), None);
        assert_eq!(
            discovery.cached("builtin", &builtin_fallback, &credentials),
            None
        );
        assert_eq!(
            discovery.cached("safe", &safe, &credentials),
            Some(safe_models)
        );
        assert_eq!(
            discovery.cached("value", &value, &credentials),
            Some(value_models)
        );
        assert_eq!(keyring.reads(), 0, "cache-only discovery read Keychain");
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn anthropic_capabilities_distinguish_absent_disabled_and_exact_levels() {
        let body = serde_json::json!({"data":[
            {"id":"a","capabilities":{"effort":{"supported":true,"low":{"supported":true},"high":{"supported":false},"xhigh":null,"max":{"supported":true}}}},
            {"id":"b","capabilities":{"effort":{"supported":false}}},
            {"id":"c","capabilities":null}
        ]});
        let models = parse_models(
            &body,
            builtin_kind("anthropic").discovery(),
            ProviderApi::AnthropicMessages,
        )
        .unwrap();
        assert_eq!(
            models[0].efforts,
            Some(vec![
                qq_provider::ReasoningEffort::Low,
                qq_provider::ReasoningEffort::Max
            ])
        );
        assert_eq!(models[1].efforts, Some(vec![]));
        assert_eq!(models[2].efforts, None);
        let malformed =
            serde_json::json!({"data":[{"id":"a","capabilities":{"effort":{"supported":"yes"}}}]});
        assert!(
            parse_models(
                &malformed,
                builtin_kind("anthropic").discovery(),
                ProviderApi::AnthropicMessages
            )
            .is_none()
        );
    }

    #[test]
    fn cache_identity_separates_effective_credentials() {
        let provider = |reference| {
            HttpAccess::new(
                "https://example.test/v1/responses",
                EndpointMode::Exact,
                ProviderApi::OpenAiResponses,
                HttpCredential::Configured(ProviderAuth::Bearer(reference)),
                BTreeMap::new(),
            )
        };
        let first: SecretRef = ron::from_str(r#"Value("tenant-a")"#).unwrap();
        let second: SecretRef = ron::from_str(r#"Value("tenant-b")"#).unwrap();
        let credentials = CredentialStore::with_paths(CredentialPaths::new(
            std::env::temp_dir().join("qq-catalog-cache-key-test"),
        ));
        let key = [7_u8; 32];
        let first = provider(first);
        let second = provider(second);
        let first_auth = resolve_auth(&first, &credentials).unwrap();
        let second_auth = resolve_auth(&second, &credentials).unwrap();

        assert_ne!(
            cache_key(
                &key,
                "custom",
                ProviderKind::Custom,
                &first,
                first_auth.as_ref()
            ),
            cache_key(
                &key,
                "custom",
                ProviderKind::Custom,
                &second,
                second_auth.as_ref()
            )
        );
        let shared = Secret::from_secret_bytes("shared");
        let anthropic = HttpAccess::new(
            "https://example.test/v1/messages",
            EndpointMode::Exact,
            ProviderApi::AnthropicMessages,
            HttpCredential::Configured(ProviderAuth::NoAuth),
            BTreeMap::new(),
        );
        assert_ne!(
            cache_key(
                &key,
                "custom",
                builtin_kind("anthropic"),
                &anthropic,
                Some(&api_key(ProviderApi::AnthropicMessages, &shared).unwrap())
            ),
            cache_key(
                &key,
                "custom",
                builtin_kind("anthropic"),
                &anthropic,
                Some(&RequestCredential::bearer("shared").unwrap())
            )
        );
    }

    #[test]
    fn a_slow_provider_does_not_hold_discovery_for_another() {
        let (b_requested, b_seen) = std::sync::mpsc::sync_channel(1);
        let serve = |listener: TcpListener,
                     id: &'static str,
                     before: Option<std::sync::mpsc::Receiver<()>>,
                     after: Option<std::sync::mpsc::SyncSender<()>>| {
            thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 4096];
                let _ = stream.read(&mut request).unwrap();
                if let Some(after) = after {
                    after.send(()).unwrap();
                }
                if let Some(before) = before {
                    before.recv().unwrap();
                }
                let body = format!(r#"{{"data":[{{"id":"{id}"}}]}}"#);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            })
        };
        let slow = TcpListener::bind("127.0.0.1:0").unwrap();
        let fast = TcpListener::bind("127.0.0.1:0").unwrap();
        let slow_provider = test_provider(
            ProviderKind::Custom,
            format!("http://{}/v1/responses", slow.local_addr().unwrap()),
            HttpCredential::Configured(ProviderAuth::NoAuth),
        );
        let fast_provider = test_provider(
            ProviderKind::Custom,
            format!("http://{}/v1/responses", fast.local_addr().unwrap()),
            HttpCredential::Configured(ProviderAuth::NoAuth),
        );
        let (slow_accepted, slow_waiting) = std::sync::mpsc::sync_channel(1);
        let slow_server = serve(slow, "slow-model", Some(b_seen), Some(slow_accepted));
        let fast_server = serve(fast, "fast-model", None, Some(b_requested));
        let discovery = Arc::new(ModelDiscovery::new().unwrap());
        let home = tempfile::tempdir().unwrap();
        let credentials = Arc::new(CredentialStore::with_paths(CredentialPaths::new(
            home.path().to_owned(),
        )));
        let slow_discovery = {
            let discovery = Arc::clone(&discovery);
            let credentials = Arc::clone(&credentials);
            thread::spawn(move || discovery.discover("slow", &slow_provider, &credentials))
        };
        slow_waiting.recv().unwrap();

        let fast_models = discovery.discover("fast", &fast_provider, &credentials);
        let slow_models = slow_discovery.join().unwrap();
        slow_server.join().unwrap();
        fast_server.join().unwrap();

        let ids = |models: Option<Vec<DiscoveredModel>>| {
            models.map(|models| models.into_iter().map(|model| model.id).collect::<Vec<_>>())
        };
        assert_eq!(ids(fast_models), Some(vec!["fast-model".to_owned()]));
        assert_eq!(ids(slow_models), Some(vec!["slow-model".to_owned()]));
    }

    #[test]
    fn rejects_non_loopback_http_discovery_endpoints() {
        let access = HttpAccess::new(
            "http://192.0.2.1/v1/responses",
            EndpointMode::Exact,
            ProviderApi::OpenAiResponses,
            HttpCredential::Configured(ProviderAuth::NoAuth),
            BTreeMap::new(),
        );

        assert!(models_endpoint(&access).is_none());
    }

    #[test]
    fn discards_terminal_controls_in_discovered_names() {
        let models = parse_models(
            &serde_json::json!({
                "data": [{"id": "safe-id", "display_name": "unsafe\u{1b}[2J"}]
            }),
            ModelDiscoveryShape::Supplementary,
            ProviderApi::OpenAiResponses,
        )
        .unwrap();

        assert_eq!(models[0].id, "safe-id");
        assert_eq!(models[0].name, None);
    }
}
