//! Request-credential workloads (plan slice AP4b).
//!
//! Both metrics run against in-memory fakes and a loopback server, so they
//! measure QQ's credential path, not a keyring or a provider network:
//!
//! - resolving a stored, unexpired xAI OAuth credential through the
//!   `CredentialStore` request-credential provider, one request at a time
//!   and as a concurrent batch on one provider;
//! - one streamed HTTP request whose authorization is resolved at send
//!   time (`HttpAuth::RequestTimeBearer`), from `Provider::stream` to the
//!   terminal event.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use futures_util::{StreamExt, future::join_all};
use qq_auth::{CredentialPaths, CredentialStore, KeyringBackend, KeyringError, OfflineTokenCalls};
use qq_provider::{
    EndpointSpec, HttpAuth, HttpProtocol, HttpProviderRecipe, Message, ModelRequest,
    ProviderCompiler, ProviderEvent, ProviderRecipe, RequestCredentialProvider,
    SharedRequestCredentialProvider,
};
use tempfile::TempDir;

use super::{CorrectnessCheck, MetricResult, PerfError, elapsed_ns, fixture_error, with_timeout};

/// Requests issued together in the concurrent resolution case.
pub(super) const CONCURRENT_RESOLUTIONS: usize = 8;
const PROFILE: &str = "bench";
const ACCESS_TOKEN: &str = "perf-xai-access-token";
const SSE_BODY: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";

/// A keyring that never leaves the process.
#[derive(Default)]
struct MemoryKeyring {
    values: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl KeyringBackend for MemoryKeyring {
    fn get(&self, name: &str) -> Result<Vec<u8>, KeyringError> {
        self.values
            .lock()
            .map_err(|_| KeyringError::Failure)?
            .get(name)
            .cloned()
            .ok_or(KeyringError::Missing)
    }

    fn set(&self, name: &str, secret: &[u8]) -> Result<(), KeyringError> {
        self.values
            .lock()
            .map_err(|_| KeyringError::Failure)?
            .insert(name.to_owned(), secret.to_vec());
        Ok(())
    }

    fn remove(&self, name: &str) -> Result<(), KeyringError> {
        self.values
            .lock()
            .map_err(|_| KeyringError::Failure)?
            .remove(name);
        Ok(())
    }
}

/// A credential store holding one unexpired xAI OAuth credential for
/// `PROFILE`. The token is valid for a day, so no sample should refresh; the
/// store's token clients are offline, so a refresh fails instead of reaching
/// a real authorization server, and the returned counter records it.
fn xai_oauth_store(directory: &TempDir) -> Result<(CredentialStore, OfflineTokenCalls), PerfError> {
    let (store, token_calls) = CredentialStore::with_backend(
        CredentialPaths::new(directory.path()),
        Arc::new(MemoryKeyring::default()),
    )
    .offline();
    let expires_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(fixture_error("read the clock"))?
        .as_secs()
        + 24 * 60 * 60;
    let credential = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "access_token": ACCESS_TOKEN,
        "refresh_token": "perf-xai-refresh-token",
        "expires_at": expires_at,
    }))
    .map_err(fixture_error("encode the xAI credential"))?;
    store
        .set_with_metadata(
            &format!("xai/{PROFILE}"),
            credential,
            false,
            Some("xai-oauth"),
            Some(qq_auth::XAI_CREDENTIAL_ENDPOINT),
        )
        .map_err(fixture_error("store the xAI credential"))?;
    Ok((store, token_calls))
}

async fn resolve(provider: &SharedRequestCredentialProvider) -> Result<(), PerfError> {
    with_timeout("resolve request credentials", provider.credential())
        .await?
        .map(drop)
        .map_err(fixture_error("resolve request credentials"))
}

pub(super) async fn credential_resolution_workloads(
    samples: u16,
    warmups: u16,
) -> Result<(Vec<MetricResult>, Vec<CorrectnessCheck>), PerfError> {
    let directory = TempDir::new().map_err(fixture_error("create credential directory"))?;
    let (store, token_calls) = xai_oauth_store(&directory)?;
    let provider = store.xai_request_credentials(PROFILE, None);

    let total = u32::from(samples) + u32::from(warmups);
    let mut sequential = Vec::with_capacity(usize::from(samples));
    for iteration in 0..total {
        let started = Instant::now();
        resolve(&provider).await?;
        if iteration >= u32::from(warmups) {
            sequential.push(elapsed_ns(started));
        }
    }

    let mut concurrent = Vec::with_capacity(usize::from(samples));
    for iteration in 0..total {
        let started = Instant::now();
        let results = join_all((0..CONCURRENT_RESOLUTIONS).map(|_| resolve(&provider))).await;
        let elapsed = elapsed_ns(started);
        results
            .into_iter()
            .collect::<Result<Vec<()>, PerfError>>()?;
        if iteration >= u32::from(warmups) {
            concurrent.push(elapsed);
        }
    }

    Ok((
        vec![
            MetricResult::measured(
                "xai_request_credential_resolution_ns",
                "ns",
                "one RequestCredentialProvider::credential call for a stored, unexpired xAI OAuth credential through CredentialStore with an in-memory keyring",
                sequential,
            )?,
            MetricResult::measured(
                "xai_request_credential_concurrent_resolution_ns",
                "ns",
                "eight concurrent RequestCredentialProvider::credential calls on one xAI provider for a stored, unexpired OAuth credential, start to last completion",
                concurrent,
            )?,
        ],
        vec![CorrectnessCheck {
            name: "xai_request_credential_resolution".to_owned(),
            passed: token_calls.count() == 0,
            detail: format!(
                "{total} sequential resolutions and {total} batches of {CONCURRENT_RESOLUTIONS} concurrent resolutions returned a bearer credential; {} token-client calls (expected 0)",
                token_calls.count()
            ),
        }],
    ))
}

/// A keep-alive HTTP/1.1 loopback that answers every request with one small
/// Chat Completions SSE body and records each request's `Authorization`.
/// It runs as one task on the caller's runtime and stops, with every
/// connection it opened, when [`KeepAliveSse::shutdown`] runs or it drops.
struct KeepAliveSse {
    base_url: String,
    authorizations: Arc<Mutex<Vec<String>>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

/// Connections the loopback accepts; the workload reuses one.
const MAX_LOOPBACK_CONNECTIONS: usize = 4;

impl KeepAliveSse {
    async fn start() -> Result<Self, PerfError> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(fixture_error("bind loopback listener"))?;
        let base_url = format!(
            "http://{}",
            listener
                .local_addr()
                .map_err(fixture_error("read loopback address"))?
        );
        let authorizations = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&authorizations);
        let (shutdown, mut stop) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = &mut stop => break,
                    accepted = listener.accept(), if connections.len() < MAX_LOOPBACK_CONNECTIONS => {
                        let Ok((stream, _)) = accepted else { break };
                        connections.spawn(serve_connection(stream, Arc::clone(&recorded)));
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => {}
                }
            }
            connections.shutdown().await;
        });
        Ok(Self {
            base_url,
            authorizations,
            shutdown: Some(shutdown),
            task: Some(task),
        })
    }

    /// Stops accepting, closes every connection and waits for the task.
    async fn shutdown(mut self) -> Result<(), PerfError> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            with_timeout("stop loopback server", task)
                .await?
                .map_err(fixture_error("stop loopback server"))?;
        }
        Ok(())
    }
}

impl Drop for KeepAliveSse {
    fn drop(&mut self) {
        // Reached only on an early return; the normal path awaits `shutdown`.
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn serve_connection(stream: tokio::net::TcpStream, authorizations: Arc<Mutex<Vec<String>>>) {
    use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};

    let (reader, mut writer) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let mut content_length = 0_usize;
        let mut authorization = None;
        loop {
            line.clear();
            if reader.read_line(&mut line).await.is_err() {
                return;
            }
            let header = line.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse().unwrap_or(0);
                } else if name.eq_ignore_ascii_case("authorization") {
                    authorization = Some(value.trim().to_owned());
                }
            }
        }
        let mut body = vec![0_u8; content_length.min(1024 * 1024)];
        if reader.read_exact(&mut body).await.is_err() {
            return;
        }
        if let (Some(value), Ok(mut seen)) = (authorization, authorizations.lock()) {
            seen.push(value);
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{SSE_BODY}",
            SSE_BODY.len()
        );
        if writer.write_all(response.as_bytes()).await.is_err() || writer.flush().await.is_err() {
            return;
        }
    }
}

pub(super) async fn request_time_authorization_workloads(
    samples: u16,
    warmups: u16,
) -> Result<(Vec<MetricResult>, Vec<CorrectnessCheck>), PerfError> {
    let directory = TempDir::new().map_err(fixture_error("create credential directory"))?;
    let (store, token_calls) = xai_oauth_store(&directory)?;
    let server = KeepAliveSse::start().await?;
    let provider = ProviderCompiler::new()
        .map_err(fixture_error("create provider compiler"))?
        .compile_for_canary(ProviderRecipe::http(HttpProviderRecipe::new(
            EndpointSpec::base(format!("{}/v1", server.base_url), true),
            HttpProtocol::OpenAiChatCompletions,
            HttpAuth::RequestTimeBearer(store.xai_request_credentials(PROFILE, None)),
        )))
        .map_err(fixture_error("compile request-time provider"))?;

    let total = u32::from(samples) + u32::from(warmups);
    let mut measured = Vec::with_capacity(usize::from(samples));
    for iteration in 0..total {
        let started = Instant::now();
        let events = with_timeout(
            "stream request-time authorized response",
            provider
                .stream(ModelRequest::new(
                    "perf-model",
                    vec![Message::user("ping")],
                    16,
                ))
                .collect::<Vec<_>>(),
        )
        .await?;
        let elapsed = elapsed_ns(started);
        match events.last() {
            Some(Ok(ProviderEvent::Completed { .. })) => {}
            other => {
                return Err(PerfError::Fixture(format!(
                    "request-time authorized stream did not complete: {other:?}"
                )));
            }
        }
        if iteration >= u32::from(warmups) {
            measured.push(elapsed);
        }
    }

    let expected = format!("Bearer {ACCESS_TOKEN}");
    let authorizations = server
        .authorizations
        .lock()
        .map_err(|_| PerfError::Fixture("loopback request log was poisoned".to_owned()))?
        .clone();
    server.shutdown().await?;
    let passed = authorizations.len() == total as usize
        && authorizations.iter().all(|value| *value == expected)
        && token_calls.count() == 0;
    Ok((
        vec![MetricResult::measured(
            "request_time_authorized_stream_ns",
            "ns",
            "Provider::stream to the terminal event for one Chat Completions request whose bearer is resolved per attempt from a stored xAI OAuth credential, against a keep-alive loopback server",
            measured,
        )?],
        vec![CorrectnessCheck {
            name: "request_time_authorization".to_owned(),
            passed,
            detail: format!(
                "{} of {total} requests carried the stored bearer; {} token-client calls (expected 0)",
                authorizations
                    .iter()
                    .filter(|value| **value == expected)
                    .count(),
                token_calls.count()
            ),
        }],
    ))
}
