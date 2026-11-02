#[test]
fn extracted_mcp_cache_failure_keeps_its_server_classification() {
    let error = super::RuntimeBuildError::from(crate::McpBuildError::CacheUnavailable);
    assert_eq!(error.failure_kind(), qq_protocol::RunFailureKind::Server);
    assert_eq!(error.to_string(), "runtime cache is unavailable");
}

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use super::*;

#[test]
fn unresolved_projection_keeps_the_server_recovery_message() {
    let error = SessionRuntimeError::ToolProjectionUnavailable {
        run: "historical-run".to_owned(),
    };
    let expected = error.to_string();
    assert!(
        matches!(map_session_runtime_error(error), ServerHandlerError::InvalidRequest(message) if message == expected && message.contains("start a new session"))
    );
}

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::stream;
use qq_auth::{CredentialPaths, KeyringBackend, KeyringError};
use qq_config::{ConfigPaths, ProviderKind, RuntimeOverrides, UsageType};
use qq_core::Runtime;
use qq_protocol::{
    CommandId, CommandOutcome, ModelSelection, RunId, RunPromptIdentity, RunStatus, SessionCommand,
    SessionId, WorkspaceId,
};
use qq_provider::{ModelRequest, Provider, ProviderEvent, ProviderStream};
use qq_server::{ServerOptions, ServerPaths, StartOutcome};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(super) const TEST_BUILD: crate::HostBuild = crate::HostBuild {
    source_revision: "test",
    version: "0.0.0+test",
};

#[derive(Default)]
pub(super) struct MemoryKeyring(Mutex<BTreeMap<String, Vec<u8>>>);

pub(super) fn stored_codex_login(account_id: &str) -> Vec<u8> {
    let claims = serde_json::to_vec(&serde_json::json!({
        "https://api.openai.com/auth": {
            "chatgpt_account_id": account_id,
            "chatgpt_account_is_fedramp": false
        }
    }))
    .unwrap();
    serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "id_token": format!("e30.{}.signature", URL_SAFE_NO_PAD.encode(claims)),
        "access_token": "access-token",
        "refresh_token": "refresh-token",
        "account_id": account_id,
        "is_fedramp": false,
        "refreshed_at": 1
    }))
    .unwrap()
}

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
        self.0
            .lock()
            .unwrap()
            .remove(name)
            .map(|_| ())
            .ok_or(KeyringError::Missing)
    }
}

pub(super) struct PanicKeyring;

impl KeyringBackend for PanicKeyring {
    fn get(&self, name: &str) -> Result<Vec<u8>, KeyringError> {
        panic!("isolated TUI QA attempted to read keyring entry {name:?}")
    }

    fn set(&self, name: &str, _secret: &[u8]) -> Result<(), KeyringError> {
        panic!("isolated TUI QA attempted to write keyring entry {name:?}")
    }

    fn remove(&self, name: &str) -> Result<(), KeyringError> {
        panic!("isolated TUI QA attempted to remove keyring entry {name:?}")
    }
}

#[derive(Default)]
struct PanicOnReadKeyring(Mutex<BTreeMap<String, Vec<u8>>>);

impl KeyringBackend for PanicOnReadKeyring {
    fn get(&self, name: &str) -> Result<Vec<u8>, KeyringError> {
        panic!("plan compilation attempted to read keyring entry {name:?}")
    }

    fn set(&self, name: &str, secret: &[u8]) -> Result<(), KeyringError> {
        self.0
            .lock()
            .unwrap()
            .insert(name.to_owned(), secret.to_vec());
        Ok(())
    }

    fn remove(&self, name: &str) -> Result<(), KeyringError> {
        self.0
            .lock()
            .unwrap()
            .remove(name)
            .map(|_| ())
            .ok_or(KeyringError::Missing)
    }
}

pub(super) struct RuntimeFixture {
    root: PathBuf,
}

impl RuntimeFixture {
    pub(super) fn new() -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "qq-runtime-test-{}-{nanos}-{sequence}",
            std::process::id()
        ));
        for directory in [".git", "global", "data", "managed", "work"] {
            fs::create_dir_all(root.join(directory)).unwrap();
        }
        let root = root.canonicalize().unwrap();
        Self { root }
    }

    pub(super) fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.root.join(relative)
    }

    fn factory(&self) -> RuntimeFactory {
        self.factory_with_credentials(CredentialStore::with_paths(CredentialPaths::new(
            self.path("data"),
        )))
    }

    pub(super) fn factory_with_credentials(&self, credentials: CredentialStore) -> RuntimeFactory {
        RuntimeFactory::new(
            ConfigLoader::new(ConfigPaths::new(
                self.path("global"),
                self.path("data"),
                self.path("managed"),
            )),
            credentials,
        )
        .unwrap()
    }

    fn request(&self, content: impl Into<String>) -> LoadRequest {
        LoadRequest::new(self.path("work"))
            .with_explicit_content(content)
            .with_overrides(RuntimeOverrides::new().with_max_output_tokens(128))
    }
}

impl Drop for RuntimeFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct CapturingProvider {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl Provider for CapturingProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        self.requests.lock().unwrap().push(request);
        Box::pin(stream::iter([
            Ok(ProviderEvent::OutputTextDelta {
                text: "done".to_owned(),
            }),
            Ok(ProviderEvent::Completed { usage: None }),
        ]))
    }
}

struct FixedRuntimeLoader {
    runtime: Arc<Runtime>,
}

impl RuntimeLoader for FixedRuntimeLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let runtime = Arc::clone(&self.runtime);
        Box::pin(async move {
            LoadedRuntime::compile_blocking(
                &runtime,
                ResolvedModel {
                    version: ResolvedModelVersion::new(1).unwrap(),
                    request_shape: None,
                    route: "test/model".to_owned(),
                    provider_model: "test/model".to_owned(),
                    organization: None,
                    credential_profile: None,
                    max_output_tokens: 256,
                    context_window: None,
                    pricing: None,
                    output_token_control: CapabilitySupport::Native,
                    generation: GenerationCapabilities {
                        reasoning_effort: CapabilitySupport::Unsupported,
                    },
                    prompt_cache: PromptCacheCapabilities {
                        control: CapabilitySupport::Unsupported,
                        cache_read_usage: false,
                        cache_write_usage: false,
                    },
                },
                PathBuf::from(request.workspace),
            )
            .map_err(|error| RuntimeLoadError {
                kind: RunFailureKind::Configuration,
                message: error.to_string(),
            })
        })
    }
}

async fn completed_prompt_identity(
    runtime: &SessionRuntime,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    run_id: RunId,
) -> RunPromptIdentity {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = runtime
                .snapshot(SnapshotRequest {
                    workspace_id,
                    focused_session_id: Some(session_id),
                    include_sessions: Vec::new(),
                    session_limit: 8,
                    message_limit: 32,
                })
                .await
                .unwrap();
            if let Some(run) = snapshot
                .focused
                .unwrap()
                .runs
                .into_iter()
                .find(|run| run.id == run_id)
                && run.status == RunStatus::Completed
            {
                return *run.prompt_identity.unwrap();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn runtime_factory_can_be_dropped_in_async_context() {
    let fixture = RuntimeFixture::new();

    drop(fixture.factory());
}
mod catalog;
mod clients;
mod continuation;
mod goal_policy;
mod plan_compilation;
mod plans;
mod providers;
mod resolved_models;
mod spawn_validation;
