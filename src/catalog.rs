//! Authenticated, best-effort model discovery.

use std::{
    collections::VecDeque,
    io::Read,
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

use hmac::{Hmac, Mac};
use qq_auth::{CredentialStore, Secret, resolve_provider_credential_with_aliases};
use qq_config::{
    EndpointMode, HttpAccess, HttpCredential, ProviderApi, ProviderAuth, ProviderConfig,
    ProviderKind,
};
use reqwest::{Url, blocking::RequestBuilder, header::AUTHORIZATION};
use sha2::Sha256;
use thiserror::Error;

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_DISCOVERED_MODELS: usize = 4_096;
const MAX_CACHE_ENTRIES: usize = 32;
const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const FAILURE_CACHE_TTL: Duration = Duration::from_secs(5);
// Codex gates `/models` on a supported Codex client version, not QQ's package
// version. Keep this at least as high as the newest listed model's
// `minimal_client_version`.
const CODEX_MODELS_CLIENT_VERSION: &str = "0.156.1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiscoveredModel {
    pub(crate) id: String,
    pub(crate) name: Option<String>,
    pub(crate) efforts: Option<Vec<qq_provider::ReasoningEffort>>,
}

pub(crate) struct ModelDiscovery {
    cache: Mutex<VecDeque<CacheEntry>>,
    fetch_gate: Mutex<()>,
    cache_key: [u8; 32],
}

struct CacheEntry {
    key: [u8; 32],
    expires_at: Instant,
    models: Option<Vec<DiscoveredModel>>,
}

enum DiscoveryAuth {
    NoAuth,
    ApiKey(Secret),
    Bearer(Secret),
    Header(String, Secret),
    Codex {
        access_token: Secret,
        account_id: String,
        is_fedramp: bool,
    },
}

#[derive(Debug, Error)]
pub(crate) enum ModelDiscoveryError {
    #[error("model-discovery cache key generation failed")]
    Random,
}

impl ModelDiscovery {
    pub(crate) fn new() -> Result<Self, ModelDiscoveryError> {
        let mut cache_key = [0_u8; 32];
        getrandom::fill(&mut cache_key).map_err(|_| ModelDiscoveryError::Random)?;
        Ok(Self {
            cache: Mutex::new(VecDeque::new()),
            fetch_gate: Mutex::new(()),
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
        let _fetch = self.fetch_gate.lock().ok()?;
        let auth = resolve_auth(access, credentials)?;
        let Some(key) = cache_key(&self.cache_key, provider_id, provider.kind(), access, &auth)
        else {
            return self.fetch(provider.kind(), access, &auth);
        };
        let now = Instant::now();
        if let Ok(mut cache) = self.cache.lock() {
            cache.retain(|entry| entry.expires_at > now);
            if let Some(position) = cache.iter().position(|entry| entry.key == key) {
                let entry = cache.remove(position)?;
                let models = entry.models.clone();
                cache.push_back(entry);
                return models;
            }
        }

        let models = self.fetch(provider.kind(), access, &auth);
        if let Ok(mut cache) = self.cache.lock() {
            cache.push_back(CacheEntry {
                key,
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
        let auth = resolve_auth(access, credentials)?;
        let key = cache_key(&self.cache_key, provider_id, provider.kind(), access, &auth)?;
        self.cache
            .lock()
            .ok()?
            .iter()
            .find(|entry| entry.key == key && entry.expires_at > Instant::now())
            .and_then(|entry| entry.models.clone())
    }

    fn fetch(
        &self,
        kind: ProviderKind,
        access: &HttpAccess,
        auth: &DiscoveryAuth,
    ) -> Option<Vec<DiscoveredModel>> {
        let (mut endpoint, direct) = models_endpoint(access)?;
        match (kind, access.api()) {
            (ProviderKind::Anthropic, _) | (_, ProviderApi::AnthropicMessages) => {
                endpoint.query_pairs_mut().append_pair("limit", "1000");
            }
            (ProviderKind::Google, _) | (_, ProviderApi::GoogleGenerateContent) => {
                endpoint.query_pairs_mut().append_pair("pageSize", "1000");
            }
            (ProviderKind::OpenAiCodex, _) => {
                endpoint
                    .query_pairs_mut()
                    .append_pair("client_version", CODEX_MODELS_CLIENT_VERSION);
            }
            _ => {}
        }
        // Blocking clients own an internal runtime, so keep their lifetime
        // inside the blocking discovery call rather than RuntimeFactory.
        let client = discovery_client(direct)?;
        let mut models = Vec::new();
        let mut cursor: Option<String> = None;
        let mut total_bytes = 0_usize;
        // Bound the entire pagination walk, not just each response.
        let deadline = Instant::now() + Duration::from_secs(10);
        for _ in 0..16 {
            let mut url = endpoint.clone();
            if let Some(cursor) = &cursor {
                url.query_pairs_mut().append_pair("after_id", cursor);
            }
            let mut request = apply_static_headers(client.get(url), access)
                .timeout(deadline.checked_duration_since(Instant::now())?);
            if kind == ProviderKind::Anthropic || access.api() == ProviderApi::AnthropicMessages {
                request = request.header("anthropic-version", "2023-06-01");
            }
            let response = apply_auth(request, kind, access.api(), auth)?.send().ok()?;
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
            models.extend(parse_models(&body, kind, access.api())?);
            if models.len() > MAX_DISCOVERED_MODELS {
                return None;
            }
            if kind != ProviderKind::Anthropic
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
    auth: &DiscoveryAuth,
) -> Option<[u8; 32]> {
    let mut digest = Hmac::<Sha256>::new_from_slice(key).ok()?;
    update_digest(&mut digest, provider_id.as_bytes());
    digest.update(&[kind as u8, access.api() as u8, access.endpoint_mode() as u8]);
    update_digest(&mut digest, access.endpoint().as_bytes());
    for (name, value) in access.headers() {
        update_digest(&mut digest, name.as_bytes());
        update_digest(&mut digest, value.expose_value().as_bytes());
    }
    update_auth_digest(&mut digest, kind, access.api(), auth);
    Some(digest.finalize().into_bytes().into())
}

fn update_auth_digest(
    digest: &mut Hmac<Sha256>,
    kind: ProviderKind,
    api: ProviderApi,
    auth: &DiscoveryAuth,
) {
    match auth {
        DiscoveryAuth::NoAuth => update_digest(digest, b"no-auth"),
        DiscoveryAuth::ApiKey(secret) => {
            let name = api_key_header(kind, api);
            update_digest(digest, name.as_bytes());
            if name == "authorization" {
                update_digest(digest, b"Bearer ");
            }
            update_digest(digest, secret.expose_secret_bytes());
        }
        DiscoveryAuth::Bearer(secret) => {
            update_digest(digest, b"authorization");
            update_digest(digest, b"Bearer ");
            update_digest(digest, secret.expose_secret_bytes());
        }
        DiscoveryAuth::Header(name, secret) => {
            update_digest(digest, name.to_ascii_lowercase().as_bytes());
            update_digest(digest, secret.expose_secret_bytes());
        }
        DiscoveryAuth::Codex {
            access_token,
            account_id,
            is_fedramp,
        } => {
            update_digest(digest, b"authorization");
            update_digest(digest, b"Bearer ");
            update_digest(digest, access_token.expose_secret_bytes());
            update_digest(digest, b"chatgpt-account-id");
            update_digest(digest, account_id.as_bytes());
            update_digest(digest, b"originator:qq");
            update_digest(digest, &[u8::from(*is_fedramp)]);
        }
    }
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

fn resolve_auth(access: &HttpAccess, credentials: &CredentialStore) -> Option<DiscoveryAuth> {
    match access.auth() {
        HttpCredential::Configured(ProviderAuth::NoAuth) => Some(DiscoveryAuth::NoAuth),
        HttpCredential::Configured(ProviderAuth::ApiKey(reference)) => Some(DiscoveryAuth::ApiKey(
            credentials
                .resolve_with_endpoint(reference, Some(access.endpoint()))
                .ok()?,
        )),
        HttpCredential::Configured(ProviderAuth::Bearer(reference)) => Some(DiscoveryAuth::Bearer(
            credentials
                .resolve_with_endpoint(reference, Some(access.endpoint()))
                .ok()?,
        )),
        HttpCredential::Configured(ProviderAuth::Header(name, reference)) => {
            Some(DiscoveryAuth::Header(
                name.clone(),
                credentials
                    .resolve_with_endpoint(reference, Some(access.endpoint()))
                    .ok()?,
            ))
        }
        HttpCredential::ApiKey {
            explicit,
            stored_name,
            environment_variable,
            alternate_variables,
            audience,
        } => Some(DiscoveryAuth::ApiKey(
            resolve_provider_credential_with_aliases(
                credentials,
                explicit.as_ref(),
                stored_name,
                environment_variable,
                alternate_variables,
                Some(audience),
            )
            .ok()?,
        )),
        HttpCredential::OpenAiCodex { profile } => {
            let credential = credentials
                .resolve_codex(profile.as_deref().unwrap_or("default"))
                .ok()?;
            Some(DiscoveryAuth::Codex {
                access_token: credential.access_token().clone(),
                account_id: credential.account_id().to_owned(),
                is_fedramp: credential.is_fedramp(),
            })
        }
        HttpCredential::XAi { api_key, profile } => Some(DiscoveryAuth::Bearer(
            credentials
                .resolve_xai(profile.as_deref().unwrap_or("default"), api_key.as_ref())
                .ok()?,
        )),
    }
}

fn apply_auth(
    request: RequestBuilder,
    kind: ProviderKind,
    api: ProviderApi,
    auth: &DiscoveryAuth,
) -> Option<RequestBuilder> {
    match auth {
        DiscoveryAuth::NoAuth => Some(request),
        DiscoveryAuth::ApiKey(secret) => apply_api_key(request, kind, api, secret),
        DiscoveryAuth::Bearer(secret) => bearer(request, secret),
        DiscoveryAuth::Header(name, secret) => {
            Some(request.header(name, secret.expose_secret_str().ok()?))
        }
        DiscoveryAuth::Codex {
            access_token,
            account_id,
            is_fedramp,
        } => {
            let request = bearer(request, access_token)?
                .header("chatgpt-account-id", account_id)
                .header("originator", "qq");
            Some(if *is_fedramp {
                request.header("x-openai-fedramp", "true")
            } else {
                request
            })
        }
    }
}

fn apply_api_key(
    request: RequestBuilder,
    kind: ProviderKind,
    api: ProviderApi,
    secret: &Secret,
) -> Option<RequestBuilder> {
    let value = secret.expose_secret_str().ok()?;
    match api_key_header(kind, api) {
        "authorization" => bearer(request, secret),
        name => Some(request.header(name, value)),
    }
}

fn api_key_header(kind: ProviderKind, api: ProviderApi) -> &'static str {
    match (kind, api) {
        (ProviderKind::Anthropic, _) | (_, ProviderApi::AnthropicMessages) => "x-api-key",
        (ProviderKind::Google, _) | (_, ProviderApi::GoogleGenerateContent) => "x-goog-api-key",
        _ => "authorization",
    }
}

fn bearer(request: RequestBuilder, secret: &Secret) -> Option<RequestBuilder> {
    Some(request.header(
        AUTHORIZATION,
        format!("Bearer {}", secret.expose_secret_str().ok()?),
    ))
}

fn parse_models(
    body: &serde_json::Value,
    kind: ProviderKind,
    api: ProviderApi,
) -> Option<Vec<DiscoveredModel>> {
    let entries = if kind == ProviderKind::Google
        || api == ProviderApi::GoogleGenerateContent
        || kind == ProviderKind::OpenAiCodex
    {
        body.get("models")?.as_array()?
    } else {
        body.get("data")?.as_array()?
    };
    // A partial response must not evict the bundled fallback catalog.
    if matches!(kind, ProviderKind::OpenAiCodex | ProviderKind::Anthropic)
        && entries.len() > MAX_DISCOVERED_MODELS
    {
        return None;
    }
    let mut models = Vec::with_capacity(entries.len().min(MAX_DISCOVERED_MODELS));
    for entry in entries.iter().take(MAX_DISCOVERED_MODELS) {
        if kind == ProviderKind::OpenAiCodex {
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
            if matches!(kind, ProviderKind::OpenAiCodex | ProviderKind::Anthropic) {
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
        sync::atomic::{AtomicU64, Ordering},
        thread,
    };

    use super::*;
    use qq_auth::CredentialPaths;
    use qq_config::{ProviderAccess, SecretRef};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

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
                    ProviderKind::OpenAiCodex,
                    ProviderApi::OpenAiResponses
                )
                .is_none()
            );
        }
        assert_eq!(
            parse_models(
                &serde_json::json!({"models": []}),
                ProviderKind::OpenAiCodex,
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
            ProviderKind::OpenAiCodex,
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
                ProviderKind::OpenAiCodex,
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
            assert!(request.starts_with("GET /v1/models?client_version=0.156.1 HTTP/1.1\r\n"));
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
            HttpCredential::OpenAiCodex { profile: None },
            BTreeMap::new(),
        );
        let discovery = ModelDiscovery::new().unwrap();

        let models = discovery
            .fetch(
                ProviderKind::OpenAiCodex,
                &access,
                &DiscoveryAuth::Codex {
                    access_token: Secret::from_secret_bytes("test-token"),
                    account_id: "test-account".to_owned(),
                    is_fedramp: false,
                },
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
        let reference: SecretRef = ron::from_str(r#"Value("test-token")"#).unwrap();
        let provider = ProviderConfig::new(
            ProviderKind::Custom,
            Some(ProviderAccess::Http(HttpAccess::new(
                format!("http://{address}/v1/responses"),
                EndpointMode::Exact,
                ProviderApi::OpenAiResponses,
                HttpCredential::Configured(ProviderAuth::Bearer(reference)),
                BTreeMap::new(),
            ))),
            qq_config::UsageType::Unknown,
            BTreeMap::new(),
        );
        let path = std::env::temp_dir().join(format!(
            "qq-catalog-test-{}-{}",
            std::process::id(),
            TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let credentials = CredentialStore::with_paths(CredentialPaths::new(path));
        let discovery = ModelDiscovery::new().unwrap();

        let first = discovery
            .discover("custom", &provider, &credentials)
            .unwrap();
        server.join().unwrap();

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
            .fetch(ProviderKind::Anthropic, &access, &DiscoveryAuth::NoAuth)
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
            ProviderKind::OpenAiCodex,
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
            cache_key(&key, "custom", ProviderKind::Custom, &first, &first_auth),
            cache_key(&key, "custom", ProviderKind::Custom, &second, &second_auth)
        );
        let shared = Secret::from_secret_bytes("shared");
        assert_ne!(
            cache_key(
                &key,
                "custom",
                ProviderKind::Anthropic,
                &first,
                &DiscoveryAuth::ApiKey(shared.clone())
            ),
            cache_key(
                &key,
                "custom",
                ProviderKind::Anthropic,
                &first,
                &DiscoveryAuth::Bearer(shared)
            )
        );
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
            ProviderKind::Custom,
            ProviderApi::OpenAiResponses,
        )
        .unwrap();

        assert_eq!(models[0].id, "safe-id");
        assert_eq!(models[0].name, None);
    }
}
