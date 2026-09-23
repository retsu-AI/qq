//! Credential storage and secret resolution.

#![forbid(unsafe_code)]

use std::{
    collections::BTreeSet,
    env,
    ffi::OsString,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
#[cfg(windows)]
use sha2::{Digest, Sha256};
use thiserror::Error;

use qq_protocol::CredentialEpoch;
use qq_provider::SecretRef;

mod codex;
mod xai;

pub use codex::CodexLogin;
pub use xai::XaiLogin;

pub const KEYRING_SERVICE: &str = "dev.qq";
pub const MAX_CREDENTIAL_NAME_LEN: usize = 128;
pub const MAX_STATE_BYTES: usize = 1024 * 1024;

const INDEX_STATE_VERSION: u32 = 2;
const LEGACY_INDEX_STATE_VERSION: u32 = 1;
const FALLBACK_STATE_VERSION: u32 = 1;
const MAX_KIND_LEN: usize = 128;
const MAX_ENDPOINT_LEN: usize = 4096;
const INDEX_FILE_NAME: &str = "credentials.ron";
const FALLBACK_FILE_NAME: &str = "auth.ron";
const LOCK_FILE_NAME: &str = "auth.lock";
const CODEX_LOCK_FILE_NAME: &str = "openai-codex.lock";
const XAI_LOCK_FILE_NAME: &str = "xai.lock";
const WINDOWS_CREDENTIAL_DIRECTORY_NAME: &str = "windows-credentials";
#[cfg(windows)]
const WINDOWS_PROTECTED_MAGIC: &[u8] = b"QQDPAPI\x01";
const REQUEST_CREDENTIAL_CONCURRENCY: usize = 4;
const REQUEST_CREDENTIAL_CAPACITY_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_CREDENTIAL_LOAD_TIMEOUT: Duration = Duration::from_secs(65);

static TEMPORARY_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Secret bytes whose formatted representations never reveal their contents.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Vec<u8>);

impl Secret {
    #[must_use]
    pub fn from_secret_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    #[must_use]
    pub fn expose_secret_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn expose_secret_str(&self) -> Result<&str, AuthError> {
        std::str::from_utf8(&self.0).map_err(|_| AuthError::SecretNotUnicode)
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CredentialBackend {
    Keyring,
    File,
    WindowsProtectedFile,
}

impl fmt::Display for CredentialBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Keyring => formatter.write_str("OS keyring"),
            Self::File => formatter.write_str("plaintext auth file"),
            Self::WindowsProtectedFile => formatter.write_str("Windows user-protected file"),
        }
    }
}

/// Nonsecret information recorded for a stored credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialMetadata {
    pub name: String,
    pub backend: CredentialBackend,
    pub kind: Option<String>,
    pub endpoint: Option<String>,
}

/// All filesystem locations used by a credential store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialPaths {
    data_dir: PathBuf,
    index_file: PathBuf,
    fallback_file: PathBuf,
    lock_file: PathBuf,
    codex_lock_file: PathBuf,
    xai_lock_file: PathBuf,
    windows_credential_dir: PathBuf,
}

impl CredentialPaths {
    #[must_use]
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        let data_dir = data_dir.into();
        Self {
            index_file: data_dir.join(INDEX_FILE_NAME),
            fallback_file: data_dir.join(FALLBACK_FILE_NAME),
            lock_file: data_dir.join(LOCK_FILE_NAME),
            codex_lock_file: data_dir.join(CODEX_LOCK_FILE_NAME),
            xai_lock_file: data_dir.join(XAI_LOCK_FILE_NAME),
            windows_credential_dir: data_dir.join(WINDOWS_CREDENTIAL_DIRECTORY_NAME),
            data_dir,
        }
    }

    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    #[must_use]
    pub fn index_file(&self) -> &Path {
        &self.index_file
    }

    #[must_use]
    pub fn fallback_file(&self) -> &Path {
        &self.fallback_file
    }

    #[must_use]
    pub fn lock_file(&self) -> &Path {
        &self.lock_file
    }

    #[must_use]
    pub fn codex_lock_file(&self) -> &Path {
        &self.codex_lock_file
    }

    #[must_use]
    pub fn xai_lock_file(&self) -> &Path {
        &self.xai_lock_file
    }

    #[must_use]
    pub fn windows_credential_dir(&self) -> &Path {
        &self.windows_credential_dir
    }
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error(transparent)]
    Codex(#[from] codex::CodexAuthError),

    #[error(transparent)]
    XAi(#[from] xai::XaiAuthError),

    #[error("the operating system did not provide an application data directory")]
    SystemDirectoriesUnavailable,

    #[error("invalid credential name: {reason}")]
    InvalidCredentialName { reason: &'static str },

    #[error("environment variable `{variable}` is not set")]
    EnvironmentMissing { variable: String },

    /// A built-in provider has neither a stored credential nor its
    /// environment variable. Names both remedies because a new user reaches
    /// this before knowing either exists. `alternate_variables` lists the
    /// aliases that would also have been accepted (Google's `GOOGLE_API_KEY`).
    #[error(
        "no credential for provider `{provider}`: run `qq auth login {provider}` \
         or set the environment variable `{environment_variable}`{}",
        format_alternate_variables(.alternate_variables)
    )]
    ProviderCredentialMissing {
        provider: String,
        environment_variable: String,
        alternate_variables: &'static [&'static str],
    },

    #[error("environment variable `{variable}` is empty")]
    EnvironmentEmpty { variable: String },

    #[error("environment variable `{variable}` is not valid Unicode")]
    EnvironmentNotUnicode { variable: String },

    #[error("the resolved secret is not valid Unicode")]
    SecretNotUnicode,

    #[error("credential `{name}` is not registered")]
    StoredCredentialNotRegistered { name: String },

    #[error("credential `{name}` is registered in {backend}, but its secret is missing")]
    StoredCredentialMissing {
        name: String,
        backend: CredentialBackend,
    },

    #[error("credential `{name}` is bound to an endpoint; an expected endpoint is required")]
    EndpointRequired { name: String },

    #[error("credential `{name}` is bound to a different endpoint")]
    EndpointMismatch { name: String },

    #[error("invalid credential endpoint")]
    InvalidEndpoint,

    #[error("invalid credential kind/provider label")]
    InvalidKind,

    #[error(
        "the OS keyring is unavailable for credential `{name}`; plaintext file fallback requires explicit permission"
    )]
    FileFallbackNotAllowed { name: String },

    #[error("plaintext credential fallback is unsupported on this platform")]
    PlaintextFallbackUnsupported,

    #[error("the OS keyring is unavailable while attempting to {operation} credential `{name}`")]
    KeyringUnavailable {
        operation: &'static str,
        name: String,
    },

    #[error("the OS keyring failed while attempting to {operation} credential `{name}`")]
    KeyringFailure {
        operation: &'static str,
        name: String,
    },

    #[error(
        "Windows credential protection is unavailable while attempting to {operation} credential `{name}`"
    )]
    WindowsProtectionUnavailable {
        operation: &'static str,
        name: String,
    },

    #[error(
        "Windows credential protection failed while attempting to {operation} credential `{name}`"
    )]
    WindowsProtectionFailure {
        operation: &'static str,
        name: String,
    },

    #[error("credential state at `{path}` is corrupt")]
    CorruptState { path: PathBuf },

    #[error("credential state at `{path}` uses unsupported version {version}")]
    UnsupportedStateVersion { path: PathBuf, version: u32 },

    #[error("credential state at `{path}` contains duplicate credential name `{name}`")]
    DuplicateCredentialName { path: PathBuf, name: String },

    #[error("credential state at `{path}` exceeds the {limit}-byte limit")]
    StateTooLarge { path: PathBuf, limit: usize },

    #[error("credential path `{path}` must not be a symbolic link")]
    SymlinkPath { path: PathBuf },

    #[error("credential path `{path}` has the wrong file type")]
    WrongFileType { path: PathBuf },

    #[error("credential state at `{path}` has insecure permissions; expected mode {expected}")]
    InsecurePermissions {
        path: PathBuf,
        expected: &'static str,
    },

    #[error("atomic replacement of existing credential state at `{path}` is unsupported")]
    AtomicReplacementUnsupported { path: PathBuf },

    #[error("credential state could not be rolled back after a failed update")]
    StateRollbackFailed,

    #[error("failed to {operation} credential path `{path}`: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Clone)]
pub struct CredentialStore {
    paths: CredentialPaths,
    keyring: Arc<dyn KeyringBackend>,
    windows_protected: Arc<dyn WindowsProtectedBackend>,
    codex_client: Arc<dyn codex::CodexTokenClient>,
    codex_refresh: Arc<Mutex<()>>,
    xai_client: Arc<dyn xai::XaiTokenClient>,
    xai_refresh: Arc<Mutex<()>>,
    request_credential_permits: Arc<tokio::sync::Semaphore>,
}

impl CredentialStore {
    pub fn system() -> Result<Self, AuthError> {
        let project_dirs =
            ProjectDirs::from("dev", "qq", "qq").ok_or(AuthError::SystemDirectoriesUnavailable)?;
        Ok(Self::with_paths(CredentialPaths::new(
            project_dirs.data_dir(),
        )))
    }

    /// Constructs a store rooted at explicit paths while retaining the real OS keyring.
    #[must_use]
    pub fn with_paths(paths: CredentialPaths) -> Self {
        let windows_protected = Arc::new(SystemWindowsProtected::new(paths.clone()));
        Self::with_backends(paths, Arc::new(SystemKeyring), windows_protected)
    }

    #[must_use]
    pub fn paths(&self) -> &CredentialPaths {
        &self.paths
    }

    /// Resolves an unbound reference. Endpoint-bound stored credentials require
    /// `resolve_with_endpoint` so they cannot be used out of context.
    pub fn resolve(&self, reference: &SecretRef) -> Result<Secret, AuthError> {
        self.resolve_with_endpoint(reference, None)
    }

    pub fn resolve_with_endpoint(
        &self,
        reference: &SecretRef,
        expected_endpoint: Option<&str>,
    ) -> Result<Secret, AuthError> {
        match reference {
            SecretRef::Env(variable) => resolve_environment(variable),
            SecretRef::Stored(name) => {
                validate_credential_name(name)?;
                let _lock = self.lock_state_shared()?;
                let index = self.load_index()?;
                self.resolve_stored_from_index(&index, name, expected_endpoint)
            }
            SecretRef::Value(value) => Ok(Secret::from_secret_bytes(
                value.expose_secret().as_bytes().to_vec(),
            )),
        }
    }

    /// [`Self::resolve_with_endpoint`] plus the [`CredentialEpoch`] of the
    /// index the secret was read from, in one lock cycle. A caller that
    /// records the epoch alongside work derived from the secret (the plan
    /// cache) otherwise takes the state lock once for the epoch and again
    /// per secret. For a non-stored reference the epoch is read on its own,
    /// so the caller sees one value whatever the reference kind.
    pub fn resolve_with_epoch(
        &self,
        reference: &SecretRef,
        expected_endpoint: Option<&str>,
    ) -> Result<(Secret, CredentialEpoch), AuthError> {
        match reference {
            SecretRef::Stored(name) => {
                validate_credential_name(name)?;
                let _lock = self.lock_state_shared()?;
                let index = self.load_index()?;
                let epoch = CredentialEpoch::new(index.revision);
                let secret = self.resolve_stored_from_index(&index, name, expected_endpoint)?;
                Ok((secret, epoch))
            }
            SecretRef::Env(_) | SecretRef::Value(_) => {
                let secret = self.resolve_with_endpoint(reference, expected_endpoint)?;
                Ok((secret, self.epoch()?))
            }
        }
    }

    pub fn set(
        &self,
        name: &str,
        secret: impl AsRef<[u8]>,
        allow_file_fallback: bool,
    ) -> Result<CredentialBackend, AuthError> {
        self.set_with_metadata(name, secret, allow_file_fallback, None, None)
    }

    pub fn set_with_metadata(
        &self,
        name: &str,
        secret: impl AsRef<[u8]>,
        allow_file_fallback: bool,
        kind: Option<&str>,
        endpoint: Option<&str>,
    ) -> Result<CredentialBackend, AuthError> {
        validate_credential_name(name)?;
        let kind = normalize_kind(kind)?;
        let endpoint = endpoint.map(normalize_endpoint).transpose()?;
        let secret = secret.as_ref();
        let _codex_lock = self.lock_codex_operation(name)?;
        let _xai_lock = self.lock_xai_operation(name)?;
        self.set_with_metadata_normalized(name, secret, allow_file_fallback, kind, endpoint)
    }

    fn set_with_metadata_normalized(
        &self,
        name: &str,
        secret: &[u8],
        allow_file_fallback: bool,
        kind: Option<String>,
        endpoint: Option<String>,
    ) -> Result<CredentialBackend, AuthError> {
        let _lock = self.lock_state()?;
        let mut index = self.load_index()?;
        let old_record = index.find(name).cloned();

        if let Some(record) = old_record.as_ref()
            && record.kind == kind
            && record.endpoint == endpoint
        {
            match record.backend {
                CredentialBackend::Keyring => {
                    self.advance_index_epoch(&mut index)?;
                    match self.keyring.set(name, secret) {
                        Ok(()) => {
                            // In-place rotation leaves the record unchanged, but
                            // the index is still rewritten so the credential
                            // epoch advances for every durable secret change.
                            self.save_index(&index)?;
                            return Ok(CredentialBackend::Keyring);
                        }
                        Err(KeyringError::TooLarge) => {}
                        Err(KeyringError::Unavailable) if allow_file_fallback => {}
                        Err(KeyringError::Unavailable) => {
                            return Err(AuthError::FileFallbackNotAllowed {
                                name: name.to_owned(),
                            });
                        }
                        Err(KeyringError::Missing | KeyringError::Failure) => {
                            return Err(AuthError::KeyringFailure {
                                operation: "store",
                                name: name.to_owned(),
                            });
                        }
                    }
                }
                CredentialBackend::File if allow_file_fallback => {
                    let mut fallback = self.load_fallback()?;
                    if !fallback.contains(name) {
                        return Err(AuthError::StoredCredentialMissing {
                            name: name.to_owned(),
                            backend: CredentialBackend::File,
                        });
                    }
                    self.advance_index_epoch(&mut index)?;
                    fallback.upsert(name, secret);
                    self.save_fallback(&fallback)?;
                    self.save_index(&index)?;
                    return Ok(CredentialBackend::File);
                }
                CredentialBackend::File => {}
                CredentialBackend::WindowsProtectedFile => {
                    self.advance_index_epoch(&mut index)?;
                    self.windows_protected
                        .set(name, secret)
                        .map_err(|error| windows_protected_auth_error(error, "store", name))?;
                    self.save_index(&index)?;
                    return Ok(CredentialBackend::WindowsProtectedFile);
                }
            }
        }

        ensure_atomic_replacement_supported(self.paths.index_file())?;

        let old_fallback = if old_record
            .as_ref()
            .is_some_and(|record| record.backend == CredentialBackend::File)
        {
            let state = self.load_fallback()?;
            if !state.contains(name) {
                return Err(AuthError::StoredCredentialMissing {
                    name: name.to_owned(),
                    backend: CredentialBackend::File,
                });
            }
            Some(state)
        } else {
            None
        };

        if old_record.is_some() {
            self.advance_index_epoch(&mut index)?;
        }

        if old_record
            .as_ref()
            .is_some_and(|record| record.backend == CredentialBackend::WindowsProtectedFile)
        {
            return self.set_windows_protected(
                &mut index,
                old_record.as_ref(),
                name,
                secret,
                kind,
                endpoint,
            );
        }

        match self.keyring.set(name, secret) {
            Ok(()) => {
                if let Some(mut fallback) = old_fallback {
                    let original = fallback.clone();
                    fallback.remove(name);
                    self.save_fallback(&fallback)?;
                    index.upsert(CredentialRecord {
                        name: name.to_owned(),
                        backend: CredentialBackend::Keyring,
                        kind,
                        endpoint,
                    });
                    if let Err(error) = self.save_index(&index) {
                        if self.save_fallback(&original).is_err() {
                            return Err(AuthError::StateRollbackFailed);
                        }
                        return Err(error);
                    }
                } else {
                    index.upsert(CredentialRecord {
                        name: name.to_owned(),
                        backend: CredentialBackend::Keyring,
                        kind,
                        endpoint,
                    });
                    self.save_index(&index)?;
                }
                Ok(CredentialBackend::Keyring)
            }
            Err(KeyringError::Unavailable) => {
                if !allow_file_fallback {
                    return Err(AuthError::FileFallbackNotAllowed {
                        name: name.to_owned(),
                    });
                }
                self.set_fallback(&mut index, name, secret, kind, endpoint)
            }
            Err(KeyringError::TooLarge) => self.set_windows_protected(
                &mut index,
                old_record.as_ref(),
                name,
                secret,
                kind,
                endpoint,
            ),
            Err(KeyringError::Missing | KeyringError::Failure) => Err(AuthError::KeyringFailure {
                operation: "store",
                name: name.to_owned(),
            }),
        }
    }

    pub fn list(&self) -> Result<Vec<CredentialMetadata>, AuthError> {
        let _lock = self.lock_state_shared()?;
        let index = self.load_index()?;
        Ok(index.records.into_iter().map(Into::into).collect())
    }

    /// The opaque epoch of the stored-credential set. Every durable `set`,
    /// refresh, or `remove` advances it, so a caller that cached work derived
    /// from resolved secrets can detect rotation by comparing epochs without
    /// hashing or retaining secret material. A store with no index yet reports
    /// [`CredentialEpoch::NONE`]. Environment and inline secrets are outside
    /// the store and never move the epoch.
    pub fn epoch(&self) -> Result<CredentialEpoch, AuthError> {
        let _lock = self.lock_state_shared()?;
        let index = self.load_index()?;
        Ok(CredentialEpoch::new(index.revision))
    }

    pub fn status(&self, name: &str) -> Result<Option<CredentialMetadata>, AuthError> {
        validate_credential_name(name)?;
        let _lock = self.lock_state_shared()?;
        let index = self.load_index()?;
        Ok(index.find(name).cloned().map(Into::into))
    }

    pub fn is_registered(&self, name: &str) -> Result<bool, AuthError> {
        Ok(self.status(name)?.is_some())
    }

    /// Removes a registered credential. Returns `false` when no index record existed.
    pub fn remove(&self, name: &str) -> Result<bool, AuthError> {
        validate_credential_name(name)?;
        let _codex_lock = self.lock_codex_operation(name)?;
        let _xai_lock = self.lock_xai_operation(name)?;
        let _lock = self.lock_state()?;
        let mut index = self.load_index()?;
        let Some(record) = index.find(name).cloned() else {
            return Ok(false);
        };
        ensure_atomic_replacement_supported(self.paths.index_file())?;
        self.advance_index_epoch(&mut index)?;

        match record.backend {
            CredentialBackend::Keyring => match self.keyring.remove(name) {
                Ok(()) | Err(KeyringError::Missing) => {}
                Err(KeyringError::Unavailable) => {
                    return Err(AuthError::KeyringUnavailable {
                        operation: "remove",
                        name: name.to_owned(),
                    });
                }
                Err(KeyringError::Failure) => {
                    return Err(AuthError::KeyringFailure {
                        operation: "remove",
                        name: name.to_owned(),
                    });
                }
                Err(KeyringError::TooLarge) => {
                    return Err(AuthError::KeyringFailure {
                        operation: "remove",
                        name: name.to_owned(),
                    });
                }
            },
            CredentialBackend::File => {
                let mut fallback = self.load_fallback()?;
                if fallback.remove(name) {
                    self.save_fallback(&fallback)?;
                }
            }
            CredentialBackend::WindowsProtectedFile => match self.windows_protected.remove(name) {
                Ok(()) | Err(WindowsProtectedError::Missing) => {}
                Err(error) => return Err(windows_protected_auth_error(error, "remove", name)),
            },
        }

        index.remove(name);
        self.save_index(&index)?;
        Ok(true)
    }

    #[doc(hidden)]
    pub fn with_backend(paths: CredentialPaths, keyring: Arc<dyn KeyringBackend>) -> Self {
        let windows_protected = Arc::new(SystemWindowsProtected::new(paths.clone()));
        Self::with_backends(paths, keyring, windows_protected)
    }

    pub(crate) fn with_backends(
        paths: CredentialPaths,
        keyring: Arc<dyn KeyringBackend>,
        windows_protected: Arc<dyn WindowsProtectedBackend>,
    ) -> Self {
        Self {
            paths,
            keyring,
            windows_protected,
            codex_client: Arc::new(codex::SystemCodexTokenClient),
            codex_refresh: Arc::new(Mutex::new(())),
            xai_client: Arc::new(xai::SystemXaiTokenClient),
            xai_refresh: Arc::new(Mutex::new(())),
            request_credential_permits: Arc::new(tokio::sync::Semaphore::new(
                REQUEST_CREDENTIAL_CONCURRENCY,
            )),
        }
    }

    async fn load_request_credential<T, F>(
        &self,
        operation: F,
    ) -> Result<Result<T, AuthError>, qq_provider::RequestCredentialError>
    where
        T: Send + 'static,
        F: FnOnce(CredentialStore) -> Result<T, AuthError> + Send + 'static,
    {
        let permit = tokio::time::timeout(
            REQUEST_CREDENTIAL_CAPACITY_TIMEOUT,
            Arc::clone(&self.request_credential_permits).acquire_owned(),
        )
        .await
        .map_err(|_| qq_provider::RequestCredentialError::CapacityUnavailable)?
        .map_err(|_| qq_provider::RequestCredentialError::WorkerFailed)?;
        let store = self.clone();
        let worker = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            operation(store)
        });
        tokio::time::timeout(REQUEST_CREDENTIAL_LOAD_TIMEOUT, worker)
            .await
            .map_err(|_| qq_provider::RequestCredentialError::TimedOut)?
            .map_err(|_| qq_provider::RequestCredentialError::WorkerFailed)
    }

    fn replace_with_metadata_normalized(
        &self,
        name: &str,
        secret: &[u8],
        kind: String,
        endpoint: String,
    ) -> Result<CredentialBackend, AuthError> {
        let _lock = self.lock_state()?;
        let mut index = self.load_index()?;
        let record =
            index
                .find(name)
                .cloned()
                .ok_or_else(|| AuthError::StoredCredentialNotRegistered {
                    name: name.to_owned(),
                })?;
        if record.kind.as_deref() != Some(&kind) || record.endpoint.as_deref() != Some(&endpoint) {
            return Err(AuthError::StoredCredentialMissing {
                name: name.to_owned(),
                backend: record.backend,
            });
        }
        self.advance_index_epoch(&mut index)?;

        match record.backend {
            CredentialBackend::Keyring => match self.keyring.set(name, secret) {
                Ok(()) => {}
                Err(KeyringError::TooLarge) => {
                    return self.set_windows_protected(
                        &mut index,
                        Some(&record),
                        name,
                        secret,
                        Some(kind),
                        Some(endpoint),
                    );
                }
                Err(error) => {
                    return Err(match error {
                        KeyringError::Unavailable => AuthError::KeyringUnavailable {
                            operation: "refresh",
                            name: name.to_owned(),
                        },
                        KeyringError::Missing | KeyringError::Failure | KeyringError::TooLarge => {
                            AuthError::KeyringFailure {
                                operation: "refresh",
                                name: name.to_owned(),
                            }
                        }
                    });
                }
            },
            CredentialBackend::File => {
                let mut fallback = self.load_fallback()?;
                if !fallback.contains(name) {
                    return Err(AuthError::StoredCredentialMissing {
                        name: name.to_owned(),
                        backend: CredentialBackend::File,
                    });
                }
                fallback.upsert(name, secret);
                self.save_fallback(&fallback)?;
            }
            CredentialBackend::WindowsProtectedFile => {
                self.windows_protected
                    .set(name, secret)
                    .map_err(|error| windows_protected_auth_error(error, "refresh", name))?;
            }
        }
        index.upsert(CredentialRecord {
            name: name.to_owned(),
            backend: record.backend,
            kind: Some(kind),
            endpoint: Some(endpoint),
        });
        self.save_index(&index)?;
        Ok(record.backend)
    }

    fn resolve_registered(
        &self,
        name: &str,
        expected_endpoint: Option<&str>,
    ) -> Result<Option<Secret>, AuthError> {
        validate_credential_name(name)?;
        let _lock = self.lock_state_shared()?;
        let index = self.load_index()?;
        if index.find(name).is_none() {
            return Ok(None);
        }
        self.resolve_stored_from_index(&index, name, expected_endpoint)
            .map(Some)
    }

    fn resolve_stored_from_index(
        &self,
        index: &IndexState,
        name: &str,
        expected_endpoint: Option<&str>,
    ) -> Result<Secret, AuthError> {
        let record = index
            .find(name)
            .ok_or_else(|| AuthError::StoredCredentialNotRegistered {
                name: name.to_owned(),
            })?;
        enforce_endpoint_binding(record, expected_endpoint)?;

        let bytes = match record.backend {
            CredentialBackend::Keyring => match self.keyring.get(name) {
                Ok(secret) => secret,
                Err(KeyringError::Missing) => {
                    return Err(AuthError::StoredCredentialMissing {
                        name: name.to_owned(),
                        backend: CredentialBackend::Keyring,
                    });
                }
                Err(KeyringError::Unavailable) => {
                    return Err(AuthError::KeyringUnavailable {
                        operation: "read",
                        name: name.to_owned(),
                    });
                }
                Err(KeyringError::Failure) => {
                    return Err(AuthError::KeyringFailure {
                        operation: "read",
                        name: name.to_owned(),
                    });
                }
                Err(KeyringError::TooLarge) => {
                    return Err(AuthError::KeyringFailure {
                        operation: "read",
                        name: name.to_owned(),
                    });
                }
            },
            CredentialBackend::File => {
                let fallback = self.load_fallback()?;
                fallback
                    .get(name)
                    .ok_or_else(|| AuthError::StoredCredentialMissing {
                        name: name.to_owned(),
                        backend: CredentialBackend::File,
                    })?
            }
            CredentialBackend::WindowsProtectedFile => {
                self.windows_protected
                    .get(name)
                    .map_err(|error| match error {
                        WindowsProtectedError::Missing => AuthError::StoredCredentialMissing {
                            name: name.to_owned(),
                            backend: CredentialBackend::WindowsProtectedFile,
                        },
                        other => windows_protected_auth_error(other, "read", name),
                    })?
            }
        };
        Ok(Secret::from_secret_bytes(bytes))
    }

    fn set_fallback(
        &self,
        index: &mut IndexState,
        name: &str,
        secret: &[u8],
        kind: Option<String>,
        endpoint: Option<String>,
    ) -> Result<CredentialBackend, AuthError> {
        #[cfg(not(unix))]
        {
            let _ = (index, name, secret, kind, endpoint);
            Err(AuthError::PlaintextFallbackUnsupported)
        }

        #[cfg(unix)]
        {
            let mut fallback = self.load_fallback()?;
            let original = fallback.clone();
            fallback.upsert(name, secret);
            self.save_fallback(&fallback)?;

            index.upsert(CredentialRecord {
                name: name.to_owned(),
                backend: CredentialBackend::File,
                kind,
                endpoint,
            });
            if let Err(error) = self.save_index(index) {
                if self.save_fallback(&original).is_err() {
                    return Err(AuthError::StateRollbackFailed);
                }
                return Err(error);
            }
            Ok(CredentialBackend::File)
        }
    }

    fn set_windows_protected(
        &self,
        index: &mut IndexState,
        old_record: Option<&CredentialRecord>,
        name: &str,
        secret: &[u8],
        kind: Option<String>,
        endpoint: Option<String>,
    ) -> Result<CredentialBackend, AuthError> {
        let old_secret = match old_record.map(|record| record.backend) {
            Some(CredentialBackend::Keyring) => match self.keyring.get(name) {
                Ok(secret) => Some(secret),
                Err(KeyringError::Missing) => None,
                Err(KeyringError::Unavailable) => {
                    return Err(AuthError::KeyringUnavailable {
                        operation: "migrate",
                        name: name.to_owned(),
                    });
                }
                Err(KeyringError::TooLarge | KeyringError::Failure) => {
                    return Err(AuthError::KeyringFailure {
                        operation: "migrate",
                        name: name.to_owned(),
                    });
                }
            },
            Some(CredentialBackend::WindowsProtectedFile) => Some(
                self.windows_protected
                    .get(name)
                    .map_err(|error| windows_protected_auth_error(error, "read", name))?,
            ),
            _ => None,
        };
        let old_fallback =
            if old_record.is_some_and(|record| record.backend == CredentialBackend::File) {
                Some(self.load_fallback()?)
            } else {
                None
            };

        self.windows_protected
            .set(name, secret)
            .map_err(|error| windows_protected_auth_error(error, "store", name))?;

        if old_record.is_some_and(|record| record.backend == CredentialBackend::Keyring) {
            match self.keyring.remove(name) {
                Ok(()) | Err(KeyringError::Missing) => {}
                Err(error) => {
                    let _ = self.windows_protected.remove(name);
                    return Err(match error {
                        KeyringError::Unavailable => AuthError::KeyringUnavailable {
                            operation: "migrate",
                            name: name.to_owned(),
                        },
                        _ => AuthError::KeyringFailure {
                            operation: "migrate",
                            name: name.to_owned(),
                        },
                    });
                }
            }
        }

        if let Some(mut fallback) = old_fallback.clone() {
            fallback.remove(name);
            if let Err(error) = self.save_fallback(&fallback) {
                let _ = self.windows_protected.remove(name);
                return Err(error);
            }
        }

        let original_index = index.clone();
        index.upsert(CredentialRecord {
            name: name.to_owned(),
            backend: CredentialBackend::WindowsProtectedFile,
            kind,
            endpoint,
        });
        if let Err(error) = self.save_index(index) {
            let mut rollback_failed = false;
            match old_record.map(|record| record.backend) {
                Some(CredentialBackend::Keyring) => {
                    if let Some(old_secret) = old_secret.as_deref()
                        && self.keyring.set(name, old_secret).is_err()
                    {
                        rollback_failed = true;
                    }
                    if self.windows_protected.remove(name).is_err() {
                        rollback_failed = true;
                    }
                }
                Some(CredentialBackend::WindowsProtectedFile) => {
                    if let Some(old_secret) = old_secret.as_deref()
                        && self.windows_protected.set(name, old_secret).is_err()
                    {
                        rollback_failed = true;
                    }
                }
                Some(CredentialBackend::File) => {
                    if let Some(old_fallback) = old_fallback.as_ref()
                        && self.save_fallback(old_fallback).is_err()
                    {
                        rollback_failed = true;
                    }
                    if self.windows_protected.remove(name).is_err() {
                        rollback_failed = true;
                    }
                }
                None => {
                    if !matches!(
                        self.windows_protected.remove(name),
                        Ok(()) | Err(WindowsProtectedError::Missing)
                    ) {
                        rollback_failed = true;
                    }
                }
            }
            *index = original_index;
            if rollback_failed {
                return Err(AuthError::StateRollbackFailed);
            }
            return Err(error);
        }
        Ok(CredentialBackend::WindowsProtectedFile)
    }

    /// The exclusive state lock, for every operation that writes the index
    /// or a fallback file.
    fn lock_state(&self) -> Result<StateLock, AuthError> {
        self.lock_state_as(LockMode::Exclusive)
    }

    /// The shared state lock, for reads. Concurrent readers (every compile
    /// resolving its secrets, every reviewer checking the epoch) no longer
    /// serialize on each other; a writer still excludes them all. The
    /// verification after the lock is the same as for a writer.
    fn lock_state_shared(&self) -> Result<StateLock, AuthError> {
        self.lock_state_as(LockMode::Shared)
    }

    fn lock_state_as(&self, mode: LockMode) -> Result<StateLock, AuthError> {
        ensure_data_directory(self.paths.data_dir())?;
        let file = open_lock_file(self.paths.lock_file())?;
        match mode {
            LockMode::Exclusive => file.lock(),
            LockMode::Shared => file.lock_shared(),
        }
        .map_err(|source| AuthError::Io {
            operation: "lock",
            path: self.paths.lock_file().to_owned(),
            source,
        })?;
        verify_open_regular_file(self.paths.lock_file(), &file, true)?;
        validate_existing_state_file(self.paths.index_file())?;
        validate_existing_state_file(self.paths.fallback_file())?;
        Ok(StateLock { _file: file })
    }

    fn lock_codex_operation(&self, name: &str) -> Result<Option<CodexStateLock<'_>>, AuthError> {
        if !name.starts_with("openai-codex/") {
            return Ok(None);
        }
        let process = self
            .codex_refresh
            .lock()
            .map_err(|_| codex::CodexAuthError::RefreshLockUnavailable)?;
        ensure_data_directory(self.paths.data_dir())?;
        let file = open_lock_file(self.paths.codex_lock_file())?;
        file.lock().map_err(|source| AuthError::Io {
            operation: "lock",
            path: self.paths.codex_lock_file().to_owned(),
            source,
        })?;
        verify_open_regular_file(self.paths.codex_lock_file(), &file, true)?;
        Ok(Some(CodexStateLock {
            _process: process,
            _file: file,
        }))
    }

    fn lock_xai_operation(&self, name: &str) -> Result<Option<CodexStateLock<'_>>, AuthError> {
        if !name.starts_with("xai/") {
            return Ok(None);
        }
        let process = self
            .xai_refresh
            .lock()
            .map_err(|_| xai::XaiAuthError::RefreshLockUnavailable)?;
        ensure_data_directory(self.paths.data_dir())?;
        let file = open_lock_file(self.paths.xai_lock_file())?;
        file.lock().map_err(|source| AuthError::Io {
            operation: "lock",
            path: self.paths.xai_lock_file().to_owned(),
            source,
        })?;
        verify_open_regular_file(self.paths.xai_lock_file(), &file, true)?;
        Ok(Some(CodexStateLock {
            _process: process,
            _file: file,
        }))
    }

    fn load_index(&self) -> Result<IndexState, AuthError> {
        let Some(bytes) = read_state_file(self.paths.index_file())? else {
            return Ok(IndexState::default());
        };
        let content = std::str::from_utf8(&bytes).map_err(|_| AuthError::CorruptState {
            path: self.paths.index_file().to_owned(),
        })?;
        let mut state: IndexState =
            ron::from_str(content).map_err(|_| AuthError::CorruptState {
                path: self.paths.index_file().to_owned(),
            })?;
        state.validate(self.paths.index_file())?;
        state.sort();
        Ok(state)
    }

    fn save_index(&self, state: &IndexState) -> Result<(), AuthError> {
        let mut state = state.clone();
        state.version = INDEX_STATE_VERSION;
        state.revision = state.revision.wrapping_add(1);
        state.sort();
        let bytes = serialize_state(&state, self.paths.index_file())?;
        atomic_write(self.paths.index_file(), &bytes)
    }

    /// Durably invalidates request-time caches before mutating any credential
    /// backend. If the later backend or index update fails, readers still see
    /// a different epoch and cannot reuse material from before the attempt.
    fn advance_index_epoch(&self, state: &mut IndexState) -> Result<(), AuthError> {
        self.save_index(state)?;
        state.version = INDEX_STATE_VERSION;
        state.revision = state.revision.wrapping_add(1);
        Ok(())
    }

    fn load_fallback(&self) -> Result<FallbackState, AuthError> {
        let Some(bytes) = read_state_file(self.paths.fallback_file())? else {
            return Ok(FallbackState::default());
        };
        let content = std::str::from_utf8(&bytes).map_err(|_| AuthError::CorruptState {
            path: self.paths.fallback_file().to_owned(),
        })?;
        let mut state: FallbackState =
            ron::from_str(content).map_err(|_| AuthError::CorruptState {
                path: self.paths.fallback_file().to_owned(),
            })?;
        state.validate(self.paths.fallback_file())?;
        state.sort();
        Ok(state)
    }

    fn save_fallback(&self, state: &FallbackState) -> Result<(), AuthError> {
        let mut state = state.clone();
        state.sort();
        let bytes = serialize_state(&state, self.paths.fallback_file())?;
        atomic_write(self.paths.fallback_file(), &bytes)
    }
}

/// Resolves provider credentials in precedence order: explicit reference,
/// registered stored credential, then provider environment variable.
pub fn resolve_provider_credential(
    store: &CredentialStore,
    explicit: Option<&SecretRef>,
    stored_name: &str,
    environment_variable: &str,
    expected_endpoint: Option<&str>,
) -> Result<Secret, AuthError> {
    resolve_provider_credential_with_aliases(
        store,
        explicit,
        stored_name,
        environment_variable,
        &[],
        expected_endpoint,
    )
}

/// [`resolve_provider_credential`] for providers whose SDKs read more than
/// one variable: `environment_variable` wins, then each of
/// `alternate_variables` in order. A set-but-empty or non-Unicode variable
/// is reported for the first variable that is present rather than skipped.
pub fn resolve_provider_credential_with_aliases(
    store: &CredentialStore,
    explicit: Option<&SecretRef>,
    stored_name: &str,
    environment_variable: &str,
    alternate_variables: &'static [&'static str],
    expected_endpoint: Option<&str>,
) -> Result<Secret, AuthError> {
    if let Some(reference) = explicit {
        return store.resolve_with_endpoint(reference, expected_endpoint);
    }
    if let Some(secret) = store.resolve_registered(stored_name, expected_endpoint)? {
        return Ok(secret);
    }
    match environment_secret_from_any(environment_variable, alternate_variables, |variable| {
        env::var_os(variable)
    }) {
        Err(AuthError::EnvironmentMissing { .. }) => {
            // `PROVIDER/PROFILE`: the provider half is what `qq auth login`
            // takes. A name without a slash is used verbatim.
            let provider = stored_name.split('/').next().unwrap_or(stored_name);
            Err(AuthError::ProviderCredentialMissing {
                provider: provider.to_owned(),
                environment_variable: environment_variable.to_owned(),
                alternate_variables,
            })
        }
        other => other,
    }
}

/// Reads the first of `environment_variable` then `alternate_variables` that
/// is set. A variable that is set but empty or non-Unicode is reported, not
/// skipped, so a typo in the preferred variable is never masked by an alias.
/// `lookup` is injected so the walk is testable without touching the process
/// environment.
fn environment_secret_from_any(
    environment_variable: &str,
    alternate_variables: &[&str],
    lookup: impl Fn(&str) -> Option<OsString>,
) -> Result<Secret, AuthError> {
    for variable in std::iter::once(environment_variable).chain(alternate_variables.iter().copied())
    {
        match environment_secret(variable, lookup(variable)) {
            Err(AuthError::EnvironmentMissing { .. }) => {}
            other => return other,
        }
    }
    Err(AuthError::EnvironmentMissing {
        variable: environment_variable.to_owned(),
    })
}

/// Renders the `(or `A` or `B`)` suffix for alias variables; empty when there
/// are none so single-variable providers keep the short message.
fn format_alternate_variables(alternate_variables: &[&str]) -> String {
    if alternate_variables.is_empty() {
        return String::new();
    }
    let mut suffix = String::from(" (or ");
    for (index, variable) in alternate_variables.iter().enumerate() {
        if index > 0 {
            suffix.push_str(" or ");
        }
        suffix.push('`');
        suffix.push_str(variable);
        suffix.push('`');
    }
    suffix.push(')');
    suffix
}

/// Text attached to request-time "missing credential" failures so the run
/// error names the provider and the command that fixes it. Built once when a
/// request-credential provider is constructed; the request path only clones
/// an `Arc`. `qq-provider` carries the text opaquely and never learns about
/// `qq auth login`.
pub(crate) struct MissingCredentialRemedies {
    /// `PROVIDER/PROFILE`, the name the provider stores under.
    name: String,
    /// Nothing usable exists: not stored and (when applicable) no variable.
    absent: Arc<str>,
    /// The index knows the name but the secret behind it is gone.
    secret_missing: Arc<str>,
}

impl MissingCredentialRemedies {
    /// `login_commands` are the `qq auth login` spellings for this provider
    /// in the order they should be suggested; `--profile` is appended for a
    /// non-default profile. `environment_variable` is the fallback variable
    /// the provider also reads, if any.
    pub(crate) fn new(
        provider: &str,
        profile: &str,
        login_commands: &[&str],
        environment_variable: Option<&str>,
    ) -> Self {
        let name = format!("{provider}/{profile}");
        let mut commands = String::new();
        for (index, command) in login_commands.iter().enumerate() {
            if index > 0 {
                commands.push_str(" or ");
            }
            commands.push('`');
            commands.push_str(command);
            if profile != "default" {
                commands.push_str(" --profile ");
                commands.push_str(profile);
            }
            commands.push('`');
        }
        let variable = environment_variable.map_or_else(String::new, |variable| {
            format!(" or set the environment variable `{variable}`")
        });
        let absent = if profile == "default" {
            format!("no credential for provider `{provider}`: run {commands}{variable}")
        } else {
            format!("credential `{name}` is not registered: run {commands}{variable}")
        };
        let secret_missing = format!(
            "credential `{name}` is registered, but its secret is missing: \
             run `qq auth logout {name}`, then {commands}"
        );
        Self {
            name,
            absent: Arc::from(absent),
            secret_missing: Arc::from(secret_missing),
        }
    }

    /// Maps one of the missing-credential `AuthError`s to a request error
    /// carrying the matching remedy. An error about a credential the user
    /// referenced explicitly (an `Env(...)` variable, a `Stored(...)` name
    /// other than ours) keeps its own message, since our remedies would
    /// describe the wrong thing.
    pub(crate) fn missing(&self, error: &AuthError) -> qq_provider::RequestCredentialError {
        let remedy = match error {
            AuthError::ProviderCredentialMissing { .. } => Arc::clone(&self.absent),
            AuthError::StoredCredentialNotRegistered { name } if *name == self.name => {
                Arc::clone(&self.absent)
            }
            AuthError::StoredCredentialMissing { name, .. } if *name == self.name => {
                Arc::clone(&self.secret_missing)
            }
            AuthError::EnvironmentMissing { .. }
            | AuthError::StoredCredentialNotRegistered { .. }
            | AuthError::StoredCredentialMissing { .. } => Arc::from(error.to_string()),
            _ => return qq_provider::RequestCredentialError::missing(),
        };
        qq_provider::RequestCredentialError::missing_with_remedy(remedy)
    }
}

pub fn validate_credential_name(name: &str) -> Result<(), AuthError> {
    if name.is_empty() {
        return Err(AuthError::InvalidCredentialName {
            reason: "the name is empty",
        });
    }
    if name.len() > MAX_CREDENTIAL_NAME_LEN {
        return Err(AuthError::InvalidCredentialName {
            reason: "the name is too long",
        });
    }
    if !name.as_bytes()[0].is_ascii_lowercase() {
        return Err(AuthError::InvalidCredentialName {
            reason: "the name must start with an ASCII lowercase letter",
        });
    }
    if !name.bytes().all(|byte| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || matches!(byte, b'.' | b'_' | b'-' | b'/')
    }) {
        return Err(AuthError::InvalidCredentialName {
            reason: "the name contains an unsupported character",
        });
    }
    if name.split('/').any(str::is_empty) {
        return Err(AuthError::InvalidCredentialName {
            reason: "the name contains an empty segment",
        });
    }
    if name.contains("..") {
        return Err(AuthError::InvalidCredentialName {
            reason: "the name contains `..`",
        });
    }
    Ok(())
}

fn resolve_environment(variable: &str) -> Result<Secret, AuthError> {
    environment_secret(variable, env::var_os(variable))
}

fn environment_secret(variable: &str, value: Option<OsString>) -> Result<Secret, AuthError> {
    let value = value.ok_or_else(|| AuthError::EnvironmentMissing {
        variable: variable.to_owned(),
    })?;
    let value = value
        .into_string()
        .map_err(|_| AuthError::EnvironmentNotUnicode {
            variable: variable.to_owned(),
        })?;
    if value.is_empty() {
        return Err(AuthError::EnvironmentEmpty {
            variable: variable.to_owned(),
        });
    }
    Ok(Secret::from_secret_bytes(value.into_bytes()))
}

fn normalize_kind(kind: Option<&str>) -> Result<Option<String>, AuthError> {
    kind.map(|value| {
        if value.is_empty()
            || value.len() > MAX_KIND_LEN
            || value.trim() != value
            || value.chars().any(char::is_control)
        {
            return Err(AuthError::InvalidKind);
        }
        Ok(value.to_owned())
    })
    .transpose()
}

fn normalize_endpoint(endpoint: &str) -> Result<String, AuthError> {
    if endpoint.is_empty()
        || endpoint.len() > MAX_ENDPOINT_LEN
        || endpoint.trim() != endpoint
        || endpoint.chars().any(char::is_control)
        || endpoint.contains(['?', '#', '\\'])
    {
        return Err(AuthError::InvalidEndpoint);
    }
    let (scheme, remainder) = endpoint
        .split_once("://")
        .ok_or(AuthError::InvalidEndpoint)?;
    if !valid_scheme(scheme) || remainder.is_empty() {
        return Err(AuthError::InvalidEndpoint);
    }
    let authority_end = remainder.find('/').unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    let path = &remainder[authority_end..];
    if authority.is_empty()
        || authority.contains('@')
        || authority.chars().any(char::is_whitespace)
        || !valid_authority(authority)
    {
        return Err(AuthError::InvalidEndpoint);
    }

    let scheme = scheme.to_ascii_lowercase();
    let mut authority = authority.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" => Some(":80"),
        "https" => Some(":443"),
        _ => None,
    };
    if let Some(default_port) = default_port
        && authority.ends_with(default_port)
    {
        authority.truncate(authority.len() - default_port.len());
    }

    let path = path.trim_end_matches('/');
    Ok(format!("{scheme}://{authority}{path}"))
}

fn valid_scheme(scheme: &str) -> bool {
    let mut bytes = scheme.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
}

fn valid_authority(authority: &str) -> bool {
    if let Some(remainder) = authority.strip_prefix('[') {
        let Some((host, port)) = remainder.split_once(']') else {
            return false;
        };
        return !host.is_empty()
            && host
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || matches!(byte, b':' | b'.'))
            && (port.is_empty() || port.strip_prefix(':').is_some_and(valid_numeric_port));
    }

    if authority.matches(':').count() > 1 {
        return false;
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => !host.is_empty() && valid_numeric_port(port),
        None => !authority.is_empty(),
    }
}

fn valid_numeric_port(port: &str) -> bool {
    !port.is_empty()
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|port| port != 0)
}

fn enforce_endpoint_binding(
    record: &CredentialRecord,
    expected_endpoint: Option<&str>,
) -> Result<(), AuthError> {
    let Some(bound_endpoint) = record.endpoint.as_deref() else {
        return Ok(());
    };
    let expected_endpoint = expected_endpoint.ok_or_else(|| AuthError::EndpointRequired {
        name: record.name.clone(),
    })?;
    if normalize_endpoint(expected_endpoint)? != bound_endpoint {
        return Err(AuthError::EndpointMismatch {
            name: record.name.clone(),
        });
    }
    Ok(())
}

impl From<CredentialRecord> for CredentialMetadata {
    fn from(record: CredentialRecord) -> Self {
        Self {
            name: record.name,
            backend: record.backend,
            kind: record.kind,
            endpoint: record.endpoint,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialRecord {
    name: String,
    backend: CredentialBackend,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexState {
    version: u32,
    /// Advanced by every durable index write. Indexes written before the
    /// field existed load as revision zero, so the first mutation after an
    /// upgrade still produces a strictly newer epoch.
    #[serde(default)]
    revision: u64,
    records: Vec<CredentialRecord>,
}

impl Default for IndexState {
    fn default() -> Self {
        Self {
            version: INDEX_STATE_VERSION,
            revision: 0,
            records: Vec::new(),
        }
    }
}

impl IndexState {
    fn validate(&self, path: &Path) -> Result<(), AuthError> {
        if !matches!(
            self.version,
            LEGACY_INDEX_STATE_VERSION | INDEX_STATE_VERSION
        ) {
            return Err(AuthError::UnsupportedStateVersion {
                path: path.to_owned(),
                version: self.version,
            });
        }
        let mut names = BTreeSet::new();
        for record in &self.records {
            if !names.insert(record.name.clone()) {
                return Err(AuthError::DuplicateCredentialName {
                    path: path.to_owned(),
                    name: record.name.clone(),
                });
            }
            let endpoint_is_invalid = record.endpoint.as_deref().is_some_and(|endpoint| {
                !matches!(normalize_endpoint(endpoint), Ok(normalized) if normalized == endpoint)
            });
            if validate_credential_name(&record.name).is_err()
                || normalize_kind(record.kind.as_deref()).is_err()
                || endpoint_is_invalid
            {
                return Err(AuthError::CorruptState {
                    path: path.to_owned(),
                });
            }
        }
        Ok(())
    }

    fn find(&self, name: &str) -> Option<&CredentialRecord> {
        self.records.iter().find(|record| record.name == name)
    }

    fn upsert(&mut self, record: CredentialRecord) {
        self.remove(&record.name);
        self.records.push(record);
        self.sort();
    }

    fn remove(&mut self, name: &str) -> bool {
        let original_len = self.records.len();
        self.records.retain(|record| record.name != name);
        original_len != self.records.len()
    }

    fn sort(&mut self) {
        self.records
            .sort_by(|left, right| left.name.cmp(&right.name));
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FallbackRecord {
    name: String,
    secret: Vec<u8>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FallbackState {
    version: u32,
    records: Vec<FallbackRecord>,
}

impl Default for FallbackState {
    fn default() -> Self {
        Self {
            version: FALLBACK_STATE_VERSION,
            records: Vec::new(),
        }
    }
}

impl FallbackState {
    fn validate(&self, path: &Path) -> Result<(), AuthError> {
        if self.version != FALLBACK_STATE_VERSION {
            return Err(AuthError::UnsupportedStateVersion {
                path: path.to_owned(),
                version: self.version,
            });
        }
        let mut names = BTreeSet::new();
        for record in &self.records {
            if !names.insert(record.name.clone()) {
                return Err(AuthError::DuplicateCredentialName {
                    path: path.to_owned(),
                    name: record.name.clone(),
                });
            }
            if validate_credential_name(&record.name).is_err() {
                return Err(AuthError::CorruptState {
                    path: path.to_owned(),
                });
            }
        }
        Ok(())
    }

    fn contains(&self, name: &str) -> bool {
        self.records.iter().any(|record| record.name == name)
    }

    fn get(&self, name: &str) -> Option<Vec<u8>> {
        self.records
            .iter()
            .find(|record| record.name == name)
            .map(|record| record.secret.clone())
    }

    fn upsert(&mut self, name: &str, secret: &[u8]) {
        self.remove(name);
        self.records.push(FallbackRecord {
            name: name.to_owned(),
            secret: secret.to_vec(),
        });
        self.sort();
    }

    fn remove(&mut self, name: &str) -> bool {
        let original_len = self.records.len();
        self.records.retain(|record| record.name != name);
        original_len != self.records.len()
    }

    fn sort(&mut self) {
        self.records
            .sort_by(|left, right| left.name.cmp(&right.name));
    }
}

fn serialize_state<T: Serialize>(state: &T, path: &Path) -> Result<Vec<u8>, AuthError> {
    let content =
        ron::ser::to_string_pretty(state, ron::ser::PrettyConfig::default()).map_err(|_| {
            AuthError::CorruptState {
                path: path.to_owned(),
            }
        })?;
    if content.len() > MAX_STATE_BYTES {
        return Err(AuthError::StateTooLarge {
            path: path.to_owned(),
            limit: MAX_STATE_BYTES,
        });
    }
    Ok(content.into_bytes())
}

fn ensure_data_directory(path: &Path) -> Result<(), AuthError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_directory_metadata(path, &metadata)?,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(path).map_err(|source| AuthError::Io {
                operation: "create",
                path: path.to_owned(),
                source,
            })?;
            let metadata = fs::symlink_metadata(path).map_err(|source| AuthError::Io {
                operation: "inspect",
                path: path.to_owned(),
                source,
            })?;
            validate_directory_metadata(path, &metadata)?;
        }
        Err(source) => {
            return Err(AuthError::Io {
                operation: "inspect",
                path: path.to_owned(),
                source,
            });
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let directory = File::open(path).map_err(|source| AuthError::Io {
            operation: "open",
            path: path.to_owned(),
            source,
        })?;
        let open_metadata = verify_open_directory(path, &directory)?;
        // This runs on every lock cycle, so the chmod is issued only when the
        // mode actually differs; the verified metadata already carries it.
        if open_metadata.permissions().mode() & 0o777 != 0o700 {
            directory
                .set_permissions(fs::Permissions::from_mode(0o700))
                .map_err(|source| AuthError::Io {
                    operation: "set permissions on",
                    path: path.to_owned(),
                    source,
                })?;
        }
    }
    Ok(())
}

fn validate_directory_metadata(path: &Path, metadata: &fs::Metadata) -> Result<(), AuthError> {
    if metadata.file_type().is_symlink() {
        return Err(AuthError::SymlinkPath {
            path: path.to_owned(),
        });
    }
    if !metadata.is_dir() {
        return Err(AuthError::WrongFileType {
            path: path.to_owned(),
        });
    }
    Ok(())
}

fn open_lock_file(path: &Path) -> Result<File, AuthError> {
    loop {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                validate_regular_metadata(path, &metadata)?;
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .map_err(|source| AuthError::Io {
                        operation: "open",
                        path: path.to_owned(),
                        source,
                    })?;
                verify_open_regular_file(path, &file, true)?;
                return Ok(file);
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                let mut options = OpenOptions::new();
                options.read(true).write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                match options.open(path) {
                    Ok(file) => {
                        set_private_file_permissions(path, &file)?;
                        verify_open_regular_file(path, &file, true)?;
                        return Ok(file);
                    }
                    Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(source) => {
                        return Err(AuthError::Io {
                            operation: "create",
                            path: path.to_owned(),
                            source,
                        });
                    }
                }
            }
            Err(source) => {
                return Err(AuthError::Io {
                    operation: "inspect",
                    path: path.to_owned(),
                    source,
                });
            }
        }
    }
}

fn read_state_file(path: &Path) -> Result<Option<Vec<u8>>, AuthError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(AuthError::Io {
                operation: "inspect",
                path: path.to_owned(),
                source,
            });
        }
    };
    validate_regular_metadata(path, &metadata)?;
    if metadata.len() > MAX_STATE_BYTES as u64 {
        return Err(AuthError::StateTooLarge {
            path: path.to_owned(),
            limit: MAX_STATE_BYTES,
        });
    }

    let file = File::open(path).map_err(|source| AuthError::Io {
        operation: "open",
        path: path.to_owned(),
        source,
    })?;
    verify_open_regular_file(path, &file, true)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_STATE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|source| AuthError::Io {
            operation: "read",
            path: path.to_owned(),
            source,
        })?;
    if bytes.len() > MAX_STATE_BYTES {
        return Err(AuthError::StateTooLarge {
            path: path.to_owned(),
            limit: MAX_STATE_BYTES,
        });
    }
    Ok(Some(bytes))
}

fn validate_existing_state_file(path: &Path) -> Result<(), AuthError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(AuthError::Io {
                operation: "inspect",
                path: path.to_owned(),
                source,
            });
        }
    };
    validate_regular_metadata(path, &metadata)?;
    if metadata.len() > MAX_STATE_BYTES as u64 {
        return Err(AuthError::StateTooLarge {
            path: path.to_owned(),
            limit: MAX_STATE_BYTES,
        });
    }
    let file = File::open(path).map_err(|source| AuthError::Io {
        operation: "open",
        path: path.to_owned(),
        source,
    })?;
    verify_open_regular_file(path, &file, true)
}

fn validate_regular_metadata(path: &Path, metadata: &fs::Metadata) -> Result<(), AuthError> {
    if metadata.file_type().is_symlink() {
        return Err(AuthError::SymlinkPath {
            path: path.to_owned(),
        });
    }
    if !metadata.is_file() {
        return Err(AuthError::WrongFileType {
            path: path.to_owned(),
        });
    }
    Ok(())
}

/// Returns the open handle's metadata so the caller can act on the verified
/// mode without a further stat.
fn verify_open_directory(path: &Path, file: &File) -> Result<fs::Metadata, AuthError> {
    let open_metadata = file.metadata().map_err(|source| AuthError::Io {
        operation: "inspect",
        path: path.to_owned(),
        source,
    })?;
    let path_metadata = fs::symlink_metadata(path).map_err(|source| AuthError::Io {
        operation: "inspect",
        path: path.to_owned(),
        source,
    })?;
    validate_directory_metadata(path, &path_metadata)?;
    if !open_metadata.is_dir() || !same_file(&open_metadata, &path_metadata) {
        return Err(AuthError::WrongFileType {
            path: path.to_owned(),
        });
    }
    Ok(open_metadata)
}

fn verify_open_regular_file(
    path: &Path,
    file: &File,
    require_private_permissions: bool,
) -> Result<(), AuthError> {
    let open_metadata = file.metadata().map_err(|source| AuthError::Io {
        operation: "inspect",
        path: path.to_owned(),
        source,
    })?;
    let path_metadata = fs::symlink_metadata(path).map_err(|source| AuthError::Io {
        operation: "inspect",
        path: path.to_owned(),
        source,
    })?;
    validate_regular_metadata(path, &path_metadata)?;
    if !open_metadata.is_file() || !same_file(&open_metadata, &path_metadata) {
        return Err(AuthError::WrongFileType {
            path: path.to_owned(),
        });
    }

    #[cfg(unix)]
    if require_private_permissions {
        use std::os::unix::fs::MetadataExt;
        if open_metadata.mode() & 0o7777 != 0o600 {
            return Err(AuthError::InsecurePermissions {
                path: path.to_owned(),
                expected: "0600",
            });
        }
    }
    #[cfg(not(unix))]
    let _ = require_private_permissions;
    Ok(())
}

#[cfg(unix)]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    true
}

fn set_private_file_permissions(path: &Path, file: &File) -> Result<(), AuthError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|source| AuthError::Io {
                operation: "set permissions on",
                path: path.to_owned(),
                source,
            })?;
    }
    #[cfg(not(unix))]
    let _ = (path, file);
    Ok(())
}

#[cfg(windows)]
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), AuthError> {
    if bytes.len() > MAX_STATE_BYTES {
        return Err(AuthError::StateTooLarge {
            path: path.to_owned(),
            limit: MAX_STATE_BYTES,
        });
    }
    let parent = path.parent().ok_or_else(|| AuthError::Io {
        operation: "locate parent of",
        path: path.to_owned(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "state path has no parent"),
    })?;
    ensure_data_directory(parent)?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        validate_regular_metadata(path, &metadata)?;
    }
    let mut file =
        atomic_write_file::AtomicWriteFile::open(path).map_err(|source| AuthError::Io {
            operation: "create temporary replacement for",
            path: path.to_owned(),
            source,
        })?;
    file.write_all(bytes).map_err(|source| AuthError::Io {
        operation: "write temporary replacement for",
        path: path.to_owned(),
        source,
    })?;
    file.sync_all().map_err(|source| AuthError::Io {
        operation: "synchronize temporary replacement for",
        path: path.to_owned(),
        source,
    })?;
    file.commit().map_err(|source| AuthError::Io {
        operation: "replace",
        path: path.to_owned(),
        source,
    })?;
    let metadata = fs::symlink_metadata(path).map_err(|source| AuthError::Io {
        operation: "inspect",
        path: path.to_owned(),
        source,
    })?;
    validate_regular_metadata(path, &metadata)
}

#[cfg(not(windows))]
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), AuthError> {
    if bytes.len() > MAX_STATE_BYTES {
        return Err(AuthError::StateTooLarge {
            path: path.to_owned(),
            limit: MAX_STATE_BYTES,
        });
    }
    let parent = path.parent().ok_or_else(|| AuthError::Io {
        operation: "locate parent of",
        path: path.to_owned(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "state path has no parent"),
    })?;
    ensure_data_directory(parent)?;

    let destination_exists = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_regular_metadata(path, &metadata)?;
            true
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => false,
        Err(source) => {
            return Err(AuthError::Io {
                operation: "inspect",
                path: path.to_owned(),
                source,
            });
        }
    };

    #[cfg(not(unix))]
    if destination_exists {
        return Err(AuthError::AtomicReplacementUnsupported {
            path: path.to_owned(),
        });
    }
    #[cfg(unix)]
    let _ = destination_exists;

    let temporary = temporary_path(parent, path);
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).map_err(|source| AuthError::Io {
            operation: "create",
            path: temporary.clone(),
            source,
        })?;
        set_private_file_permissions(&temporary, &file)?;
        file.write_all(bytes).map_err(|source| AuthError::Io {
            operation: "write",
            path: temporary.clone(),
            source,
        })?;
        file.sync_all().map_err(|source| AuthError::Io {
            operation: "synchronize",
            path: temporary.clone(),
            source,
        })?;
        verify_open_regular_file(&temporary, &file, true)?;
        let written_metadata = file.metadata().map_err(|source| AuthError::Io {
            operation: "inspect",
            path: temporary.clone(),
            source,
        })?;
        drop(file);

        if let Ok(metadata) = fs::symlink_metadata(path) {
            validate_regular_metadata(path, &metadata)?;
        }
        fs::rename(&temporary, path).map_err(|source| AuthError::Io {
            operation: "replace",
            path: path.to_owned(),
            source,
        })?;

        let replaced_metadata = fs::symlink_metadata(path).map_err(|source| AuthError::Io {
            operation: "inspect",
            path: path.to_owned(),
            source,
        })?;
        validate_regular_metadata(path, &replaced_metadata)?;
        if !same_file(&written_metadata, &replaced_metadata) {
            return Err(AuthError::WrongFileType {
                path: path.to_owned(),
            });
        }

        #[cfg(unix)]
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| AuthError::Io {
                operation: "synchronize",
                path: parent.to_owned(),
                source,
            })?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn ensure_atomic_replacement_supported(path: &Path) -> Result<(), AuthError> {
    #[cfg(all(not(unix), not(windows)))]
    if path.exists() {
        return Err(AuthError::AtomicReplacementUnsupported {
            path: path.to_owned(),
        });
    }
    #[cfg(any(unix, windows))]
    let _ = path;
    Ok(())
}

fn temporary_path(parent: &Path, destination: &Path) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = TEMPORARY_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("auth-state");
    parent.join(format!(
        ".{name}.{}.{}.{}.tmp",
        std::process::id(),
        nonce,
        counter
    ))
}

struct StateLock {
    _file: File,
}

#[derive(Clone, Copy)]
enum LockMode {
    Exclusive,
    Shared,
}

struct CodexStateLock<'a> {
    _process: MutexGuard<'a, ()>,
    _file: File,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub enum KeyringError {
    Unavailable,
    Missing,
    TooLarge,
    Failure,
}

#[doc(hidden)]
pub trait KeyringBackend: Send + Sync {
    fn get(&self, name: &str) -> Result<Vec<u8>, KeyringError>;
    fn set(&self, name: &str, secret: &[u8]) -> Result<(), KeyringError>;
    fn remove(&self, name: &str) -> Result<(), KeyringError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(any(test, windows)),
    allow(
        dead_code,
        reason = "non-Windows production builds only construct the unavailable backend error"
    )
)]
pub(crate) enum WindowsProtectedError {
    Unavailable,
    Missing,
    Failure,
}

pub(crate) trait WindowsProtectedBackend: Send + Sync {
    fn get(&self, name: &str) -> Result<Vec<u8>, WindowsProtectedError>;
    fn set(&self, name: &str, secret: &[u8]) -> Result<(), WindowsProtectedError>;
    fn remove(&self, name: &str) -> Result<(), WindowsProtectedError>;
}

fn windows_protected_auth_error(
    error: WindowsProtectedError,
    operation: &'static str,
    name: &str,
) -> AuthError {
    match error {
        WindowsProtectedError::Unavailable => AuthError::WindowsProtectionUnavailable {
            operation,
            name: name.to_owned(),
        },
        WindowsProtectedError::Missing | WindowsProtectedError::Failure => {
            AuthError::WindowsProtectionFailure {
                operation,
                name: name.to_owned(),
            }
        }
    }
}

struct SystemWindowsProtected {
    #[cfg(windows)]
    paths: CredentialPaths,
}

impl SystemWindowsProtected {
    fn new(paths: CredentialPaths) -> Self {
        #[cfg(not(windows))]
        let _ = paths;
        Self {
            #[cfg(windows)]
            paths,
        }
    }

    #[cfg(windows)]
    fn path(&self, name: &str) -> PathBuf {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

        let digest = Sha256::digest(name.as_bytes());
        self.paths
            .windows_credential_dir()
            .join(format!("{}.bin", URL_SAFE_NO_PAD.encode(digest)))
    }
}

#[cfg(windows)]
impl WindowsProtectedBackend for SystemWindowsProtected {
    fn get(&self, name: &str) -> Result<Vec<u8>, WindowsProtectedError> {
        let path = self.path(name);
        let bytes = read_state_file(&path)
            .map_err(|_| WindowsProtectedError::Failure)?
            .ok_or(WindowsProtectedError::Missing)?;
        let encrypted = bytes
            .strip_prefix(WINDOWS_PROTECTED_MAGIC)
            .ok_or(WindowsProtectedError::Failure)?;
        let entropy = windows_protected_entropy(name);
        windows_dpapi::decrypt_data(encrypted, windows_dpapi::Scope::User, Some(&entropy))
            .map_err(|_| WindowsProtectedError::Failure)
    }

    fn set(&self, name: &str, secret: &[u8]) -> Result<(), WindowsProtectedError> {
        if secret.len() > MAX_STATE_BYTES {
            return Err(WindowsProtectedError::Failure);
        }
        let entropy = windows_protected_entropy(name);
        let encrypted =
            windows_dpapi::encrypt_data(secret, windows_dpapi::Scope::User, Some(&entropy))
                .map_err(|_| WindowsProtectedError::Failure)?;
        let mut bytes = Vec::with_capacity(WINDOWS_PROTECTED_MAGIC.len() + encrypted.len());
        bytes.extend_from_slice(WINDOWS_PROTECTED_MAGIC);
        bytes.extend_from_slice(&encrypted);
        atomic_write(&self.path(name), &bytes).map_err(|_| WindowsProtectedError::Failure)
    }

    fn remove(&self, name: &str) -> Result<(), WindowsProtectedError> {
        let path = self.path(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => validate_regular_metadata(&path, &metadata)
                .map_err(|_| WindowsProtectedError::Failure)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(WindowsProtectedError::Missing);
            }
            Err(_) => return Err(WindowsProtectedError::Failure),
        }
        fs::remove_file(path).map_err(|_| WindowsProtectedError::Failure)
    }
}

#[cfg(not(windows))]
impl WindowsProtectedBackend for SystemWindowsProtected {
    fn get(&self, _name: &str) -> Result<Vec<u8>, WindowsProtectedError> {
        Err(WindowsProtectedError::Unavailable)
    }

    fn set(&self, _name: &str, _secret: &[u8]) -> Result<(), WindowsProtectedError> {
        Err(WindowsProtectedError::Unavailable)
    }

    fn remove(&self, _name: &str) -> Result<(), WindowsProtectedError> {
        Err(WindowsProtectedError::Unavailable)
    }
}

#[cfg(windows)]
fn windows_protected_entropy(name: &str) -> Vec<u8> {
    let mut entropy = Vec::with_capacity(KEYRING_SERVICE.len() + 1 + name.len());
    entropy.extend_from_slice(KEYRING_SERVICE.as_bytes());
    entropy.push(0);
    entropy.extend_from_slice(name.as_bytes());
    entropy
}

struct SystemKeyring;

impl KeyringBackend for SystemKeyring {
    fn get(&self, name: &str) -> Result<Vec<u8>, KeyringError> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, name).map_err(classify_keyring_error)?;
        entry.get_secret().map_err(classify_keyring_error)
    }

    fn set(&self, name: &str, secret: &[u8]) -> Result<(), KeyringError> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, name).map_err(classify_keyring_error)?;
        entry.set_secret(secret).map_err(classify_keyring_error)
    }

    fn remove(&self, name: &str) -> Result<(), KeyringError> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, name).map_err(classify_keyring_error)?;
        entry.delete_credential().map_err(classify_keyring_error)
    }
}

fn classify_keyring_error(error: keyring::Error) -> KeyringError {
    match error {
        keyring::Error::NoEntry => KeyringError::Missing,
        #[cfg(windows)]
        keyring::Error::TooLong(attribute, _) if attribute == "secret" => KeyringError::TooLarge,
        keyring::Error::NoDefaultStore
        | keyring::Error::NoStorageAccess(_)
        | keyring::Error::PlatformFailure(_)
        | keyring::Error::NotSupportedByStore(_) => KeyringError::Unavailable,
        _ => KeyringError::Failure,
    }
}

#[cfg(test)]
mod tests;
