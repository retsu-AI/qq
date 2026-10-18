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
    io::{BufRead, BufReader, Read as _, Write as _},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures_util::{StreamExt, future::join_all};
use qq_auth::{CredentialPaths, CredentialStore, KeyringBackend, KeyringError};
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
/// `PROFILE`. The token is valid for a day, so no sample ever refreshes.
fn xai_oauth_store(directory: &TempDir) -> Result<CredentialStore, PerfError> {
    let store = CredentialStore::with_backend(
        CredentialPaths::new(directory.path()),
        Arc::new(MemoryKeyring::default()),
    );
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
    Ok(store)
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
    let store = xai_oauth_store(&directory)?;
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
            passed: true,
            detail: format!(
                "{total} sequential resolutions and {total} batches of {CONCURRENT_RESOLUTIONS} concurrent resolutions returned a bearer credential without refreshing"
            ),
        }],
    ))
}

/// A keep-alive HTTP/1.1 loopback that answers every request with one small
/// Chat Completions SSE body and records each request's `Authorization`.
struct KeepAliveSse {
    base_url: String,
    authorizations: Arc<Mutex<Vec<String>>>,
}

impl KeepAliveSse {
    fn start() -> Result<Self, PerfError> {
        let listener =
            TcpListener::bind("127.0.0.1:0").map_err(fixture_error("bind loopback listener"))?;
        let base_url = format!(
            "http://{}",
            listener
                .local_addr()
                .map_err(fixture_error("read loopback address"))?
        );
        let authorizations = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&authorizations);
        // The accept loop lives as long as the process; each connection
        // thread ends when the client closes it.
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let recorded = Arc::clone(&recorded);
                thread::spawn(move || serve_connection(stream, &recorded));
            }
        });
        Ok(Self {
            base_url,
            authorizations,
        })
    }
}

fn serve_connection(stream: TcpStream, authorizations: &Mutex<Vec<String>>) {
    if stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .is_err()
    {
        return;
    }
    let Ok(writer) = stream.try_clone() else {
        return;
    };
    let mut writer = writer;
    let mut reader = BufReader::new(stream);
    loop {
        let mut content_length = 0_usize;
        let mut authorization = None;
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        loop {
            line.clear();
            if reader.read_line(&mut line).is_err() {
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
        let mut body = vec![0_u8; content_length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        if let (Some(value), Ok(mut seen)) = (authorization, authorizations.lock()) {
            seen.push(value);
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{SSE_BODY}",
            SSE_BODY.len()
        );
        if writer.write_all(response.as_bytes()).is_err() || writer.flush().is_err() {
            return;
        }
    }
}

pub(super) async fn request_time_authorization_workloads(
    samples: u16,
    warmups: u16,
) -> Result<(Vec<MetricResult>, Vec<CorrectnessCheck>), PerfError> {
    let directory = TempDir::new().map_err(fixture_error("create credential directory"))?;
    let store = xai_oauth_store(&directory)?;
    let server = KeepAliveSse::start()?;
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
    let passed = authorizations.len() == total as usize
        && authorizations.iter().all(|value| *value == expected);
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
                "{} of {total} requests carried the stored bearer",
                authorizations
                    .iter()
                    .filter(|value| **value == expected)
                    .count()
            ),
        }],
    ))
}
