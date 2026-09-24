//! Wires configuration-declared MCP servers into the core tool-host seam.
//!
//! The composition root translates `ConfigSnapshot` MCP declarations into
//! `qq-mcp` settings (resolving bearer secrets like every other credential;
//! one that does not resolve degrades only its server) and adapts the
//! resulting [`McpManager`] to `qq-core`'s
//! [`ExternalToolHost`] trait. Registries are cached by exact live declarations,
//! so every plan built from identical declarations shares one manager — and
//! therefore one client connection per server — for the whole server
//! process. Plans snapshot the manager's catalog at compile and revalidate
//! against its generation.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use qq_auth::{AuthError, CredentialStore};
use qq_config::{ConfigSnapshot, McpServerConfig, McpTransport};
use qq_core::{
    ExternalToolHost, HostCallError, HostCallFuture, HostCatalog, HostReadiness,
    HostShutdownFuture, HostTool, HostToolResult, RunCancellation, ToolHints,
    plan::{CredentialReference, McpServerDescriptor, McpTransportKind},
};
use qq_mcp::{McpBearer, McpCallFailure, McpManager, McpServerSettings, McpTransportSettings};
use qq_protocol::CredentialEpoch;
use qq_provider::SecretRef;

use crate::runtime::RuntimeBuildError;

const MAX_CACHED_REGISTRIES: usize = 8;
/// Bound on one catalog snapshot from a blocking compile thread: connect,
/// list, and namespace every declared server.
const CATALOG_SNAPSHOT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// Adapts the shared [`McpManager`] to the `qq-core` tool-host seam.
pub struct WiredMcpRegistry {
    manager: Arc<McpManager>,
}

impl ExternalToolHost for WiredMcpRegistry {
    fn name(&self) -> &str {
        "mcp"
    }

    fn catalog_blocking(&self) -> HostCatalog {
        // Compiles run on blocking threads inside a Tokio process; the
        // manager's connections live on the async runtime, so the fetch is
        // driven from there and awaited here under a hard bound.
        let manager = Arc::clone(&self.manager);
        let fetch =
            async move { tokio::time::timeout(CATALOG_SNAPSHOT_TIMEOUT, manager.catalog()).await };
        let fetched = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle.block_on(fetch),
            Err(_) => {
                return HostCatalog {
                    generation: self.manager.generation(),
                    tools: Vec::new(),
                    readiness: HostReadiness::Unavailable {
                        message: "no async runtime is available to reach MCP servers".to_owned(),
                    },
                };
            }
        };
        match fetched {
            Ok(catalog) => HostCatalog {
                generation: catalog.generation,
                tools: catalog
                    .tools
                    .into_iter()
                    .map(|tool| HostTool {
                        spec: tool.spec,
                        hints: ToolHints {
                            read_only: tool.hints.read_only,
                            destructive: tool.hints.destructive,
                            idempotent: tool.hints.idempotent,
                            open_world: tool.hints.open_world,
                        },
                    })
                    .collect(),
                readiness: if catalog.unavailable.is_empty() {
                    HostReadiness::Ready
                } else {
                    let mut message = String::from("unavailable MCP servers: ");
                    for (index, unavailable) in catalog.unavailable.iter().enumerate() {
                        if index > 0 {
                            message.push_str("; ");
                        }
                        message.push_str(&unavailable.server);
                        message.push_str(" (");
                        message.push_str(&unavailable.reason);
                        message.push(')');
                    }
                    HostReadiness::Degraded { message }
                },
            },
            Err(_) => HostCatalog {
                generation: self.manager.generation(),
                tools: Vec::new(),
                readiness: HostReadiness::Unavailable {
                    message: format!(
                        "MCP catalog snapshot timed out after {} s",
                        CATALOG_SNAPSHOT_TIMEOUT.as_secs()
                    ),
                },
            },
        }
    }

    fn catalog_is_current(&self, generation: u64) -> bool {
        self.manager.catalog_is_current(generation)
    }

    fn config_grants(&self) -> Vec<String> {
        self.manager.config_grants()
    }

    fn call(&self, name: String, arguments: String, cancelled: RunCancellation) -> HostCallFuture {
        let manager = Arc::clone(&self.manager);
        Box::pin(async move {
            // qq-mcp takes a plain future so it stays free of core's token
            // type; the token's wait is that future.
            let signal = Box::pin(async move { cancelled.cancelled().await });
            let outcome = manager.call(&name, &arguments, signal).await;
            match outcome.failure {
                None => Ok(HostToolResult {
                    content: outcome.content,
                    is_error: outcome.is_error,
                }),
                Some(McpCallFailure::Timeout) => Err(HostCallError::Timeout),
                Some(McpCallFailure::Cancelled) => Err(HostCallError::Cancelled),
                Some(McpCallFailure::Unavailable) => {
                    Err(HostCallError::Unavailable(outcome.content))
                }
                Some(McpCallFailure::InvalidArguments) => {
                    Err(HostCallError::Refused(outcome.content))
                }
                Some(McpCallFailure::UnknownTool) => Err(HostCallError::UnknownTool(name)),
                Some(McpCallFailure::ShutDown) => Err(HostCallError::ShutDown),
            }
        })
    }

    fn readiness(&self) -> HostReadiness {
        if self.manager.is_shut_down() {
            HostReadiness::ShutDown
        } else {
            HostReadiness::Ready
        }
    }

    fn shutdown(&self) -> HostShutdownFuture {
        let manager = Arc::clone(&self.manager);
        Box::pin(async move { manager.shutdown().await })
    }
}

/// A wired registry plus the secret-free descriptors of the servers it holds.
pub(crate) struct WiredMcp {
    pub(crate) registry: Arc<WiredMcpRegistry>,
    pub(crate) servers: Vec<McpServerDescriptor>,
}

/// Live-only equality includes inline bearers and full endpoints, which the
/// durable descriptors intentionally omit. Never hash, serialize, or format
/// this key; stored credential changes remain tracked by the separate epoch.
#[derive(PartialEq, Eq)]
struct RegistryKey {
    declarations: Vec<(String, McpServerConfig)>,
    epoch: CredentialEpoch,
}

/// Process-wide cache of wired registries with exact live binding equality.
pub(crate) struct McpRegistryCache {
    cache: Mutex<VecDeque<(RegistryKey, Arc<WiredMcpRegistry>)>>,
    #[cfg(test)]
    eager_publish_barrier: Option<Arc<std::sync::Barrier>>,
}

impl McpRegistryCache {
    pub(crate) fn new() -> Self {
        Self {
            cache: Mutex::new(VecDeque::new()),
            #[cfg(test)]
            eager_publish_barrier: None,
        }
    }

    /// Returns the shared registry for the snapshot's MCP declarations with
    /// their secret-free descriptors, or `None` when no servers are declared.
    /// `epoch` is the credential epoch the bearer secrets are resolved under.
    /// `subset` restricts the plan to the named servers (a pack profile's
    /// `mcp` list); `None` admits every declared server. The registry key
    /// covers the admitted set, so two profiles with different subsets hold
    /// different managers and never share a connection they were not given.
    pub(crate) fn registry_for_snapshot(
        &self,
        credentials: &CredentialStore,
        epoch: CredentialEpoch,
        snapshot: &ConfigSnapshot,
        subset: Option<&[String]>,
    ) -> Result<Option<WiredMcp>, RuntimeBuildError> {
        let admitted: Vec<(&String, &McpServerConfig)> = snapshot
            .mcp_servers()
            .iter()
            .filter(|(name, _)| subset.is_none_or(|names| names.contains(*name)))
            .collect();
        if admitted.is_empty() {
            return Ok(None);
        }
        let mut descriptors = Vec::with_capacity(admitted.len());
        for (name, server) in &admitted {
            descriptors.push(describe_server(name, server));
        }
        let key = RegistryKey {
            declarations: admitted
                .iter()
                .map(|(name, server)| ((*name).clone(), (*server).clone()))
                .collect(),
            epoch,
        };

        {
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| RuntimeBuildError::CacheUnavailable)?;
            if let Some(index) = cache.iter().position(|(cached, _)| *cached == key) {
                let (cached_key, registry) = cache
                    .remove(index)
                    .expect("a located registry cache entry must exist");
                cache.push_back((cached_key, Arc::clone(&registry)));
                return Ok(Some(WiredMcp {
                    registry,
                    servers: descriptors,
                }));
            }
        }

        // Secrets are resolved only on a miss, after the key is known.
        let mut settings = Vec::with_capacity(admitted.len());
        for (name, server) in &admitted {
            settings.push(resolve_server(name, server, credentials));
        }
        #[cfg(test)]
        if let Some(barrier) = &self.eager_publish_barrier {
            barrier.wait();
        }
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| RuntimeBuildError::CacheUnavailable)?;
        // Resolve outside the lock, but construct only after this recheck:
        // manager construction can spawn eager background connections.
        if let Some(index) = cache.iter().position(|(cached, _)| *cached == key) {
            let (cached_key, registry) = cache
                .remove(index)
                .expect("a located registry cache entry must exist");
            cache.push_back((cached_key, Arc::clone(&registry)));
            return Ok(Some(WiredMcp {
                registry,
                servers: descriptors,
            }));
        }
        let registry = Arc::new(WiredMcpRegistry {
            manager: Arc::new(McpManager::new(settings)?),
        });
        #[cfg(test)]
        if self.eager_publish_barrier.is_some() {
            // Finish eager startup before the fixture counts HTTP initializes.
            let catalog = registry.catalog_blocking();
            assert!(matches!(catalog.readiness, HostReadiness::Ready));
        }
        cache.push_back((key, Arc::clone(&registry)));
        while cache.len() > MAX_CACHED_REGISTRIES {
            cache.pop_front();
        }
        Ok(Some(WiredMcp {
            registry,
            servers: descriptors,
        }))
    }
}

fn describe_server(name: &str, server: &McpServerConfig) -> McpServerDescriptor {
    let (transport, target, args, env, credential) = match server.transport() {
        McpTransport::Stdio { command, args, env } => (
            McpTransportKind::Stdio,
            command.clone(),
            args.clone(),
            env.clone(),
            CredentialReference::None,
        ),
        McpTransport::Http { url, bearer } => (
            McpTransportKind::Http,
            crate::runtime::describe_endpoint(url),
            Vec::new(),
            Vec::new(),
            match bearer {
                None => CredentialReference::None,
                Some(SecretRef::Env(name)) => CredentialReference::Environment(name.clone()),
                Some(SecretRef::Stored(name)) => CredentialReference::Stored(name.clone()),
                Some(SecretRef::Value(_)) => CredentialReference::Inline,
            },
        ),
    };
    McpServerDescriptor {
        name: name.to_owned(),
        transport,
        target,
        args,
        env,
        credential,
        eager: server.eager(),
        allow: server.allow().to_vec(),
        call_timeout_seconds: server.call_timeout_seconds(),
        max_concurrent_calls: server.max_concurrent_calls(),
    }
}

fn resolve_server(
    name: &str,
    server: &McpServerConfig,
    credentials: &CredentialStore,
) -> McpServerSettings {
    let transport = match server.transport() {
        McpTransport::Stdio { command, args, env } => McpTransportSettings::Stdio {
            command: command.clone(),
            args: args.clone(),
            env: env.clone(),
        },
        McpTransport::Http { url, bearer } => {
            // A bearer that does not resolve is this server's problem alone:
            // it stays declared (grants, tool names) and never connects,
            // while the rest of the plan compiles. The reason is what the
            // readiness message and tool errors show.
            let bearer = match bearer {
                None => McpBearer::None,
                Some(reference) => match credentials.resolve_with_endpoint(reference, Some(url)) {
                    Ok(secret) => match secret.expose_secret_str() {
                        Ok(token) => McpBearer::Token(token.to_owned()),
                        Err(error) => McpBearer::Unavailable {
                            reason: BearerFailure::new(&error, reference, url).into_reason(),
                        },
                    },
                    Err(error) => McpBearer::Unavailable {
                        reason: BearerFailure::new(&error, reference, url).into_reason(),
                    },
                },
            };
            McpTransportSettings::Http {
                url: url.clone(),
                bearer,
            }
        }
    };

    let mut settings = McpServerSettings::new(name, transport);
    settings.eager = server.eager();
    settings.allow = server.allow().to_vec();
    settings.call_timeout = std::time::Duration::from_secs(server.call_timeout_seconds());
    settings.max_concurrent_calls = usize::try_from(server.max_concurrent_calls())
        .expect("the validated concurrency bound fits usize");
    settings
}

/// Why a declared HTTP bearer did not resolve on this machine, split into
/// the problem and its remedy so the readiness message and `qq doctor` word
/// the same failure the same way.
pub(crate) struct BearerFailure {
    pub(crate) problem: String,
    pub(crate) remedy: String,
}

impl BearerFailure {
    pub(crate) fn new(error: &AuthError, reference: &SecretRef, url: &str) -> Self {
        let (problem, remedy) = match error {
            AuthError::StoredCredentialNotRegistered { name } => (
                format!("credential `{name}` is not registered"),
                format!("run `qq auth set {name}`"),
            ),
            AuthError::StoredCredentialMissing { name, backend } => (
                format!("credential `{name}` is registered in {backend} but its secret is missing"),
                format!("run `qq auth logout {name}` then `qq auth set {name}`"),
            ),
            AuthError::EndpointMismatch { name } | AuthError::EndpointRequired { name } => (
                format!("credential `{name}` is bound to a different endpoint"),
                format!(
                    "store it again with `qq auth set {name} --endpoint {}`",
                    crate::runtime::describe_endpoint(url)
                ),
            ),
            // Only reached for an endpoint-bound record: the binding cannot be
            // checked against a URL carrying a query, fragment, or userinfo.
            AuthError::InvalidEndpoint => (
                match reference {
                    SecretRef::Stored(name) => {
                        format!("credential `{name}` is endpoint-bound but the server URL cannot be compared")
                    }
                    SecretRef::Env(_) | SecretRef::Value(_) => error.to_string(),
                },
                "drop the query, fragment, or userinfo from the URL, or store the credential without `--endpoint`".to_owned(),
            ),
            AuthError::KeyringUnavailable { name, .. } => (
                error.to_string(),
                format!("start the OS keyring or store it with `qq auth set {name} --allow-file`"),
            ),
            AuthError::EnvironmentMissing { variable } => (
                format!("environment variable `{variable}` is not set"),
                format!("export {variable} in this shell"),
            ),
            AuthError::EnvironmentEmpty { variable }
            | AuthError::EnvironmentNotUnicode { variable } => (
                error.to_string(),
                format!("export {variable} in this shell"),
            ),
            // Store-level failures (a corrupt index, an unreadable state
            // file, a keyring fault, a secret that is not text) already
            // describe themselves; the remedy follows the reference kind.
            other => (
                other.to_string(),
                match reference {
                    SecretRef::Stored(name) => format!("run `qq auth set {name}`"),
                    SecretRef::Env(variable) => format!("export {variable} in this shell"),
                    SecretRef::Value(_) => "replace the inline bearer value".to_owned(),
                },
            ),
        };
        Self { problem, remedy }
    }

    pub(crate) fn into_reason(self) -> String {
        format!("{}; {}", self.problem, self.remedy)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use qq_auth::{CredentialPaths, CredentialStore, KeyringBackend, KeyringError};
    use qq_config::{ConfigLoader, ConfigPaths, ConfigSnapshot, LoadRequest};
    use qq_core::hosts::conformance::{ConformanceFixture, check};
    use qq_core::{ExternalToolHost, HostCallError, HostReadiness, RunCancellation};
    use qq_mcp::{McpManager, McpServerSettings, McpTransportSettings};
    use qq_protocol::CredentialEpoch;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::{McpRegistryCache, WiredMcpRegistry};

    #[derive(Default)]
    struct MemoryKeyring(std::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>>);

    impl KeyringBackend for MemoryKeyring {
        fn get(&self, name: &str) -> Result<Vec<u8>, KeyringError> {
            self.0
                .lock()
                .unwrap()
                .get(name)
                .cloned()
                .ok_or(KeyringError::Missing)
        }

        fn set(&self, name: &str, secret: &[u8]) -> Result<(), KeyringError> {
            self.0
                .lock()
                .unwrap()
                .insert(name.to_owned(), secret.to_vec());
            Ok(())
        }

        fn remove(&self, name: &str) -> Result<(), KeyringError> {
            self.0.lock().unwrap().remove(name);
            Ok(())
        }
    }

    struct HttpFixture {
        url: String,
        initializations: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    impl HttpFixture {
        async fn new() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/mcp", listener.local_addr().unwrap());
            let initializations = Arc::new(AtomicUsize::new(0));
            let initialized = Arc::clone(&initializations);
            let task = tokio::spawn(async move {
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    let header_end = loop {
                        let mut chunk = [0_u8; 4096];
                        let count = socket.read(&mut chunk).await.unwrap();
                        assert_ne!(count, 0, "request ended before its headers");
                        request.extend_from_slice(&chunk[..count]);
                        assert!(request.len() <= 64 * 1024);
                        if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                            break end + 4;
                        }
                    };
                    let headers = String::from_utf8(request[..header_end].to_vec()).unwrap();
                    let header = |name: &str| {
                        headers.lines().find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case(name).then(|| value.trim())
                        })
                    };
                    let content_length =
                        header("content-length").map_or(0, |value| value.parse::<usize>().unwrap());
                    assert!(header_end + content_length <= 64 * 1024);
                    while request.len() < header_end + content_length {
                        let mut chunk = [0_u8; 4096];
                        let count = socket.read(&mut chunk).await.unwrap();
                        assert_ne!(count, 0, "request ended before its body");
                        request.extend_from_slice(&chunk[..count]);
                    }
                    let (status, body) = if headers.starts_with("POST ") {
                        let message: serde_json::Value = serde_json::from_slice(
                            &request[header_end..header_end + content_length],
                        )
                        .unwrap();
                        match message["method"].as_str().unwrap() {
                            "notifications/initialized" => (202, String::new()),
                            method => {
                                let result = match method {
                                    "initialize" => {
                                        initialized.fetch_add(1, Ordering::SeqCst);
                                        json!({
                                        "protocolVersion": message["params"]["protocolVersion"],
                                        "capabilities": {"tools": {}},
                                        "serverInfo": {"name": "authorization-fixture", "version": "1"},
                                        })
                                    }
                                    "tools/list" => json!({"tools": [{
                                        "name": "echo", "inputSchema": {"type": "object"},
                                    }]}),
                                    "tools/call" => json!({"content": [{
                                        "type": "text",
                                        "text": format!("{} {}", header("authorization").unwrap_or("none"), headers.lines().next().unwrap()),
                                    }]}),
                                    other => panic!("unexpected MCP method {other}"),
                                };
                                (200, json!({"jsonrpc": "2.0", "id": message["id"], "result": result}).to_string())
                            }
                        }
                    } else {
                        (405, String::new())
                    };
                    let response = format!(
                        "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len(),
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                }
            });
            Self {
                url,
                initializations,
                task,
            }
        }
    }

    impl Drop for HttpFixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    struct ConfigFixture {
        root: tempfile::TempDir,
        loader: ConfigLoader,
        credentials: CredentialStore,
    }

    impl ConfigFixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            // A caller may set TMPDIR inside a configured checkout. Bound
            // project discovery to this fixture regardless of its parent.
            fs::create_dir(root.path().join(".git")).unwrap();
            let paths = ConfigPaths::new(
                root.path().join("global"),
                root.path().join("data"),
                root.path().join("managed"),
            );
            let credentials = CredentialStore::with_backend(
                CredentialPaths::new(root.path().join("data")),
                Arc::new(MemoryKeyring::default()),
            );
            Self {
                root,
                loader: ConfigLoader::new(paths),
                credentials,
            }
        }

        fn snapshot(&self, workspace: &str, url: &str, bearer: &str) -> ConfigSnapshot {
            self.snapshot_with_eager(workspace, url, bearer, false)
        }

        fn snapshot_with_eager(
            &self,
            workspace: &str,
            url: &str,
            bearer: &str,
            eager: bool,
        ) -> ConfigSnapshot {
            self.snapshot_from_document(workspace, &format!(
                r#"(version: 1, model: "openai/gpt-5.6", mcp: {{"srv": Http(url: "{url}", bearer: Value("{bearer}"), eager: {eager}, allow: ["echo"], call_timeout_seconds: 5)}})"#,
            ))
        }

        fn snapshot_from_document(&self, workspace: &str, document: &str) -> ConfigSnapshot {
            let workspace = self.root.path().join(workspace);
            fs::create_dir_all(&workspace).unwrap();
            let path = workspace.join("config.ron");
            fs::write(&path, document).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            }
            self.loader
                .load(&LoadRequest::new(workspace).with_explicit_path(path))
                .unwrap()
        }
    }

    async fn echo(registry: &WiredMcpRegistry) -> String {
        tokio::time::timeout(
            Duration::from_secs(10),
            registry.call(
                "mcp__srv__echo".to_owned(),
                "{}".to_owned(),
                RunCancellation::new(),
            ),
        )
        .await
        .unwrap()
        .unwrap()
        .content
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn workspace_inline_bearer_rotation_preserves_each_live_authorization() {
        let server = HttpFixture::new().await;
        let fixture = ConfigFixture::new();
        let cache = McpRegistryCache::new();
        let first_snapshot = fixture.snapshot("first", &server.url, "first-secret");
        let first = cache
            .registry_for_snapshot(
                &fixture.credentials,
                CredentialEpoch::NONE,
                &first_snapshot,
                None,
            )
            .unwrap()
            .unwrap();
        assert!(
            echo(&first.registry)
                .await
                .starts_with("Bearer first-secret ")
        );

        let second_snapshot = fixture.snapshot("second", &server.url, "second-secret");
        let second = cache
            .registry_for_snapshot(
                &fixture.credentials,
                CredentialEpoch::NONE,
                &second_snapshot,
                None,
            )
            .unwrap()
            .unwrap();
        assert!(
            echo(&second.registry)
                .await
                .starts_with("Bearer second-secret ")
        );

        let rotated_snapshot = fixture.snapshot("first", &server.url, "rotated-secret");
        let rotated = cache
            .registry_for_snapshot(
                &fixture.credentials,
                CredentialEpoch::NONE,
                &rotated_snapshot,
                None,
            )
            .unwrap()
            .unwrap();
        assert!(
            echo(&rotated.registry)
                .await
                .starts_with("Bearer rotated-secret ")
        );
        assert!(
            echo(&first.registry)
                .await
                .starts_with("Bearer first-secret ")
        );
        assert!(
            echo(&second.registry)
                .await
                .starts_with("Bearer second-secret ")
        );
        assert_eq!(first.servers, second.servers);
        assert_eq!(first.servers, rotated.servers);
        let descriptor = serde_json::to_string(&rotated.servers).unwrap();
        for secret in ["first-secret", "second-secret", "rotated-secret"] {
            assert!(!descriptor.contains(secret));
        }
        for registry in [&first.registry, &second.registry, &rotated.registry] {
            registry.shutdown().await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn identical_declarations_share_a_connection_but_endpoint_queries_do_not() {
        let server = HttpFixture::new().await;
        let fixture = ConfigFixture::new();
        let cache = McpRegistryCache::new();
        let first_snapshot =
            fixture.snapshot("first", &format!("{}?key=first", server.url), "secret");
        let first = cache
            .registry_for_snapshot(
                &fixture.credentials,
                CredentialEpoch::NONE,
                &first_snapshot,
                None,
            )
            .unwrap()
            .unwrap();
        let same_snapshot =
            fixture.snapshot("second", &format!("{}?key=first", server.url), "secret");
        let same = cache
            .registry_for_snapshot(
                &fixture.credentials,
                CredentialEpoch::NONE,
                &same_snapshot,
                None,
            )
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&first.registry, &same.registry));

        let changed_snapshot =
            fixture.snapshot("first", &format!("{}?key=second", server.url), "secret");
        let changed = cache
            .registry_for_snapshot(
                &fixture.credentials,
                CredentialEpoch::NONE,
                &changed_snapshot,
                None,
            )
            .unwrap()
            .unwrap();
        assert!(
            echo(&changed.registry)
                .await
                .ends_with("POST /mcp?key=second HTTP/1.1")
        );
        assert!(
            echo(&first.registry)
                .await
                .ends_with("POST /mcp?key=first HTTP/1.1")
        );
        assert_eq!(first.servers, changed.servers);
        let descriptor = serde_json::to_string(&changed.servers).unwrap();
        assert!(!descriptor.contains("key="));
        assert!(!descriptor.contains("secret"));
        first.registry.shutdown().await;
        changed.registry.shutdown().await;
    }

    #[test]
    fn concurrent_equal_declarations_publish_one_live_registry() {
        let fixture = ConfigFixture::new();
        let cache = McpRegistryCache::new();
        let snapshot = fixture.snapshot("first", "http://127.0.0.1:1/mcp", "secret");
        let barrier = std::sync::Barrier::new(8);
        let registries = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        cache
                            .registry_for_snapshot(
                                &fixture.credentials,
                                CredentialEpoch::NONE,
                                &snapshot,
                                None,
                            )
                            .unwrap()
                            .unwrap()
                            .registry
                    })
                })
                .collect();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(
            registries
                .iter()
                .all(|registry| Arc::ptr_eq(&registries[0], registry))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_equal_eager_declarations_initialize_one_connection() {
        let server = HttpFixture::new().await;
        let fixture = ConfigFixture::new();
        let mut cache = McpRegistryCache::new();
        cache.eager_publish_barrier = Some(Arc::new(std::sync::Barrier::new(8)));
        let cache = Arc::new(cache);
        let snapshot = Arc::new(fixture.snapshot_with_eager("first", &server.url, "secret", true));
        let tasks = (0..8)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let snapshot = Arc::clone(&snapshot);
                let credentials = fixture.credentials.clone();
                tokio::task::spawn_blocking(move || {
                    cache
                        .registry_for_snapshot(&credentials, CredentialEpoch::NONE, &snapshot, None)
                        .unwrap()
                        .unwrap()
                        .registry
                })
            })
            .collect::<Vec<_>>();
        let registries = tokio::time::timeout(
            Duration::from_secs(10),
            futures_util::future::join_all(tasks),
        )
        .await
        .unwrap()
        .into_iter()
        .map(Result::unwrap)
        .collect::<Vec<_>>();
        assert_eq!(server.initializations.load(Ordering::SeqCst), 1);
        assert!(
            registries
                .iter()
                .all(|registry| Arc::ptr_eq(&registries[0], registry))
        );
        assert!(echo(&registries[0]).await.starts_with("Bearer secret "));
        registries[0].shutdown().await;
    }

    /// Regression for ENG-861: a `Stored` bearer that is not registered on
    /// this machine used to fail plan compilation with an authentication
    /// error for every run in the workspace. It now degrades only its
    /// server — declared, granted, never connected — while the sibling
    /// serves; and once the credential is stored, the next compile under the
    /// new credential epoch resolves it without restarting.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unregistered_stored_bearer_degrades_only_its_server_until_it_is_set() {
        let server = HttpFixture::new().await;
        let fixture = ConfigFixture::new();
        let cache = McpRegistryCache::new();
        let snapshot = fixture.snapshot_from_document(
            "first",
            &format!(
                r#"(version: 1, model: "openai/gpt-5.6", mcp: {{
                    "linear": Http(url: "{url}", bearer: Stored("linear/default"), allow: ["create_issue"], call_timeout_seconds: 5),
                    "srv": Http(url: "{url}", bearer: Value("inline-secret"), allow: ["echo"], call_timeout_seconds: 5),
                }})"#,
                url = server.url,
            ),
        );
        let epoch = fixture.credentials.epoch().unwrap();
        let wired = cache
            .registry_for_snapshot(&fixture.credentials, epoch, &snapshot, None)
            .unwrap()
            .expect("two servers are declared");
        assert_eq!(wired.servers.len(), 2);
        let mut grants = wired.registry.config_grants();
        grants.sort();
        assert_eq!(grants, ["mcp__linear__create_issue", "mcp__srv__echo"]);

        let registry = Arc::clone(&wired.registry);
        let catalog = tokio::task::spawn_blocking(move || registry.catalog_blocking())
            .await
            .unwrap();
        let HostReadiness::Degraded { message } = &catalog.readiness else {
            panic!("expected Degraded, got {:?}", catalog.readiness);
        };
        assert_eq!(
            message,
            "unavailable MCP servers: linear (credential `linear/default` is not registered; run `qq auth set linear/default`)"
        );
        assert_eq!(
            catalog
                .tools
                .iter()
                .map(|tool| tool.spec.name())
                .collect::<Vec<_>>(),
            ["mcp__srv__echo"]
        );
        assert!(wired.registry.catalog_is_current(catalog.generation));
        assert_eq!(server.initializations.load(Ordering::SeqCst), 1);

        let degraded = tokio::time::timeout(
            Duration::from_secs(10),
            wired.registry.call(
                "mcp__linear__create_issue".to_owned(),
                "{}".to_owned(),
                RunCancellation::new(),
            ),
        )
        .await
        .unwrap();
        match degraded {
            Err(HostCallError::Unavailable(reason)) => {
                assert!(reason.contains("linear/default"), "{reason}");
                assert!(reason.contains("qq auth set linear/default"), "{reason}");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
        assert!(
            echo(&wired.registry)
                .await
                .starts_with("Bearer inline-secret ")
        );

        // `qq auth set linear/default` on this machine: the index revision
        // advances, so the epoch the next compile reads differs and the
        // registry key misses; the fresh manager resolves the credential.
        fixture
            .credentials
            .set_with_metadata("linear/default", b"stored-secret", false, None, None)
            .unwrap();
        let rotated_epoch = fixture.credentials.epoch().unwrap();
        assert!(rotated_epoch > epoch);
        let stale = cache
            .registry_for_snapshot(&fixture.credentials, epoch, &snapshot, None)
            .unwrap()
            .unwrap();
        assert!(
            Arc::ptr_eq(&stale.registry, &wired.registry),
            "the old epoch still keys the degraded registry"
        );
        let fresh = cache
            .registry_for_snapshot(&fixture.credentials, rotated_epoch, &snapshot, None)
            .unwrap()
            .unwrap();
        assert!(!Arc::ptr_eq(&fresh.registry, &wired.registry));
        assert_eq!(fresh.servers, wired.servers);
        let registry = Arc::clone(&fresh.registry);
        let catalog = tokio::task::spawn_blocking(move || registry.catalog_blocking())
            .await
            .unwrap();
        assert_eq!(catalog.readiness, HostReadiness::Ready, "{catalog:?}");
        let mut names = catalog
            .tools
            .iter()
            .map(|tool| tool.spec.name())
            .collect::<Vec<_>>();
        names.sort_unstable();
        assert_eq!(names, ["mcp__linear__echo", "mcp__srv__echo"]);
        let resolved = tokio::time::timeout(
            Duration::from_secs(10),
            fresh.registry.call(
                "mcp__linear__echo".to_owned(),
                "{}".to_owned(),
                RunCancellation::new(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            resolved.content.starts_with("Bearer stored-secret "),
            "{}",
            resolved.content
        );
        wired.registry.shutdown().await;
        fresh.registry.shutdown().await;
    }

    /// The shared host suite over a real manager whose HTTP bearer did not
    /// resolve: the snapshot is not Ready, every call is a typed
    /// `Unavailable`, shutdown is bounded and invalidates the generation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wired_mcp_adapter_passes_the_availability_subset_for_an_unresolved_bearer() {
        let fixture = ConfigFixture::new();
        let snapshot = fixture.snapshot_from_document(
            "first",
            r#"(version: 1, model: "openai/gpt-5.6", mcp: {"srv": Http(url: "http://127.0.0.1:1/mcp", bearer: Stored("srv/default"), allow: ["echo"], call_timeout_seconds: 5)})"#,
        );
        let wired = McpRegistryCache::new()
            .registry_for_snapshot(&fixture.credentials, CredentialEpoch::NONE, &snapshot, None)
            .unwrap()
            .unwrap();
        let host: Arc<dyn qq_core::ExternalToolHost> = wired.registry;
        assert_eq!(host.config_grants(), ["mcp__srv__echo"]);
        check(
            host,
            ConformanceFixture {
                succeeds: Some(("mcp__srv__echo".to_owned(), String::new())),
                tool_error: None,
                hangs: None,
                oversized: None,
                unknown: "mcp__other__nope".to_owned(),
                concurrency: None,
                backend_unavailable: true,
            },
        )
        .await
        .unwrap();
    }

    /// The adapter runs the shared host suite against a real manager whose
    /// stdio server cannot start: the snapshot reports the backend as
    /// unavailable rather than Ready, every call is a typed `Unavailable`
    /// (never a success or a panic), shutdown is bounded, and a shut-down
    /// manager invalidates the catalog generation a plan was compiled from.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wired_mcp_adapter_passes_the_availability_subset_over_the_real_transport() {
        let mut settings = McpServerSettings::new(
            "srv",
            McpTransportSettings::Stdio {
                command: "qq-mcp-test-no-such-binary".to_owned(),
                args: Vec::new(),
                env: Vec::new(),
            },
        );
        settings.call_timeout = std::time::Duration::from_secs(5);
        settings.allow = vec!["echo".to_owned()];
        let host: Arc<dyn qq_core::ExternalToolHost> = Arc::new(WiredMcpRegistry {
            manager: Arc::new(McpManager::new(vec![settings]).unwrap()),
        });
        assert_eq!(host.config_grants(), ["mcp__srv__echo"]);
        check(
            host,
            ConformanceFixture {
                succeeds: Some(("mcp__srv__echo".to_owned(), String::new())),
                tool_error: None,
                hangs: None,
                oversized: None,
                unknown: "mcp__other__nope".to_owned(),
                concurrency: None,
                backend_unavailable: true,
            },
        )
        .await
        .unwrap();
    }
}
