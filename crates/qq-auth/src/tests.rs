use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::{Read as _, Write as _},
    net::TcpStream,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

static TEST_DIRECTORY_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum FakeMode {
    #[default]
    Available,
    Unavailable,
    Failure,
}

#[derive(Default)]
struct FakeKeyring {
    mode: Mutex<FakeMode>,
    max_secret_len: Mutex<Option<usize>>,
    values: Mutex<BTreeMap<String, Vec<u8>>>,
    reads: Mutex<BTreeMap<String, usize>>,
    break_index_after_set: Mutex<Option<PathBuf>>,
}

impl FakeKeyring {
    fn set_mode(&self, mode: FakeMode) {
        *self.mode.lock().unwrap() = mode;
    }

    fn erase(&self, name: &str) {
        self.values.lock().unwrap().remove(name);
    }

    fn value(&self, name: &str) -> Option<Vec<u8>> {
        self.values.lock().unwrap().get(name).cloned()
    }

    fn read_count(&self, name: &str) -> usize {
        self.reads.lock().unwrap().get(name).copied().unwrap_or(0)
    }

    fn break_index_after_next_set(&self, path: &Path) {
        *self.break_index_after_set.lock().unwrap() = Some(path.to_owned());
    }

    fn set_max_secret_len(&self, limit: usize) {
        *self.max_secret_len.lock().unwrap() = Some(limit);
    }

    fn check_mode(&self) -> Result<(), KeyringError> {
        match *self.mode.lock().unwrap() {
            FakeMode::Available => Ok(()),
            FakeMode::Unavailable => Err(KeyringError::Unavailable),
            FakeMode::Failure => Err(KeyringError::Failure),
        }
    }
}

impl KeyringBackend for FakeKeyring {
    fn get(&self, name: &str) -> Result<Vec<u8>, KeyringError> {
        self.check_mode()?;
        *self
            .reads
            .lock()
            .unwrap()
            .entry(name.to_owned())
            .or_default() += 1;
        self.value(name).ok_or(KeyringError::Missing)
    }

    fn set(&self, name: &str, secret: &[u8]) -> Result<(), KeyringError> {
        self.check_mode()?;
        if self
            .max_secret_len
            .lock()
            .unwrap()
            .is_some_and(|limit| secret.len() > limit)
        {
            return Err(KeyringError::TooLarge);
        }
        self.values
            .lock()
            .unwrap()
            .insert(name.to_owned(), secret.to_vec());
        if let Some(path) = self.break_index_after_set.lock().unwrap().take() {
            fs::remove_file(&path).unwrap();
            fs::create_dir(&path).unwrap();
        }
        Ok(())
    }

    fn remove(&self, name: &str) -> Result<(), KeyringError> {
        self.check_mode()?;
        self.values
            .lock()
            .unwrap()
            .remove(name)
            .map(|_| ())
            .ok_or(KeyringError::Missing)
    }
}

#[derive(Default)]
struct FakeWindowsProtected {
    mode: Mutex<FakeMode>,
    values: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl FakeWindowsProtected {
    fn set_mode(&self, mode: FakeMode) {
        *self.mode.lock().unwrap() = mode;
    }

    fn value(&self, name: &str) -> Option<Vec<u8>> {
        self.values.lock().unwrap().get(name).cloned()
    }

    fn check_mode(&self) -> Result<(), WindowsProtectedError> {
        match *self.mode.lock().unwrap() {
            FakeMode::Available => Ok(()),
            FakeMode::Unavailable => Err(WindowsProtectedError::Unavailable),
            FakeMode::Failure => Err(WindowsProtectedError::Failure),
        }
    }
}

impl WindowsProtectedBackend for FakeWindowsProtected {
    fn get(&self, name: &str) -> Result<Vec<u8>, WindowsProtectedError> {
        self.check_mode()?;
        self.value(name).ok_or(WindowsProtectedError::Missing)
    }

    fn set(&self, name: &str, secret: &[u8]) -> Result<(), WindowsProtectedError> {
        self.check_mode()?;
        self.values
            .lock()
            .unwrap()
            .insert(name.to_owned(), secret.to_vec());
        Ok(())
    }

    fn remove(&self, name: &str) -> Result<(), WindowsProtectedError> {
        self.check_mode()?;
        self.values
            .lock()
            .unwrap()
            .remove(name)
            .map(|_| ())
            .ok_or(WindowsProtectedError::Missing)
    }
}

struct FakeCodexTokenClient {
    exchanges: Mutex<Vec<(String, String, String)>>,
    refreshes: Mutex<Vec<String>>,
    exchanged: codex::ExchangedTokens,
    refreshed: codex::RefreshedTokens,
}

#[derive(Clone, Copy)]
enum FakeCodexRefreshFailure {
    Rejected,
    Unavailable,
}

struct FailingCodexTokenClient(FakeCodexRefreshFailure);

impl codex::CodexTokenClient for FailingCodexTokenClient {
    fn exchange(
        &self,
        _code: &str,
        _redirect_uri: &str,
        _code_verifier: &str,
    ) -> Result<codex::ExchangedTokens, codex::CodexAuthError> {
        Err(codex::CodexAuthError::TokenRequestFailed {
            operation: "exchange",
        })
    }

    fn refresh(
        &self,
        _refresh_token: &str,
    ) -> Result<codex::RefreshedTokens, codex::CodexAuthError> {
        match self.0 {
            FakeCodexRefreshFailure::Rejected => Err(codex::CodexAuthError::TokenRequestRejected {
                operation: "refresh",
                status: 401,
            }),
            FakeCodexRefreshFailure::Unavailable => {
                Err(codex::CodexAuthError::TokenRequestFailed {
                    operation: "refresh",
                })
            }
        }
    }
}

impl FakeCodexTokenClient {
    fn new(exchanged: codex::ExchangedTokens, refreshed: codex::RefreshedTokens) -> Self {
        Self {
            exchanges: Mutex::new(Vec::new()),
            refreshes: Mutex::new(Vec::new()),
            exchanged,
            refreshed,
        }
    }
}

impl codex::CodexTokenClient for FakeCodexTokenClient {
    fn exchange(
        &self,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> Result<codex::ExchangedTokens, codex::CodexAuthError> {
        self.exchanges.lock().unwrap().push((
            code.to_owned(),
            redirect_uri.to_owned(),
            code_verifier.to_owned(),
        ));
        Ok(self.exchanged.clone())
    }

    fn refresh(
        &self,
        refresh_token: &str,
    ) -> Result<codex::RefreshedTokens, codex::CodexAuthError> {
        self.refreshes
            .lock()
            .unwrap()
            .push(refresh_token.to_owned());
        Ok(self.refreshed.clone())
    }
}

struct FakeXaiTokenClient {
    refreshes: Mutex<Vec<String>>,
    refreshed: xai::TokenSet,
}

impl xai::XaiTokenClient for FakeXaiTokenClient {
    fn start_device(&self) -> Result<xai::DeviceAuthorization, xai::XaiAuthError> {
        Err(xai::XaiAuthError::DeviceRequestFailed)
    }

    fn poll_device(&self, _device_code: &str) -> Result<xai::DevicePoll, xai::XaiAuthError> {
        Err(xai::XaiAuthError::DeviceRequestFailed)
    }

    fn refresh(&self, refresh_token: &str) -> Result<xai::TokenSet, xai::XaiAuthError> {
        self.refreshes
            .lock()
            .unwrap()
            .push(refresh_token.to_owned());
        Ok(self.refreshed.clone())
    }
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        loop {
            let counter = TEST_DIRECTORY_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("qq-auth-test-{}-{counter}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("failed to create test directory: {error}"),
            }
        }
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn test_store() -> (CredentialStore, Arc<FakeKeyring>, TestDirectory) {
    let (store, keyring, _protected, directory) = test_store_with_protected();
    (store, keyring, directory)
}

fn test_store_with_protected() -> (
    CredentialStore,
    Arc<FakeKeyring>,
    Arc<FakeWindowsProtected>,
    TestDirectory,
) {
    let directory = TestDirectory::new();
    let keyring = Arc::new(FakeKeyring::default());
    let protected = Arc::new(FakeWindowsProtected::default());
    let store = CredentialStore::with_backends(
        CredentialPaths::new(directory.path()),
        keyring.clone(),
        protected.clone(),
    );
    (store, keyring, protected, directory)
}

fn jwt(payload: serde_json::Value) -> String {
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
    format!("e30.{payload}.signature")
}

fn stored_codex_credential(access_token: String, refresh_token: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "id_token": jwt(serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "workspace-test-id",
                "chatgpt_account_is_fedramp": false
            }
        })),
        "access_token": access_token,
        "refresh_token": refresh_token,
        "account_id": "workspace-test-id",
        "is_fedramp": false,
        "refreshed_at": 0
    }))
    .unwrap()
}

fn codex_request_provider(
    store: &CredentialStore,
    profile: &str,
) -> codex::CodexRequestCredentials {
    codex::CodexRequestCredentials::new(store, profile)
}

fn callback(port: u16, query: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET /auth/callback?{query} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn write_private(path: &Path, bytes: impl AsRef<[u8]>) {
    fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
}

#[test]
fn secret_debug_and_display_are_redacted() {
    let secret = Secret::from_secret_bytes(b"do-not-print".to_vec());

    assert_eq!(format!("{secret:?}"), "<redacted>");
    assert_eq!(format!("{secret}"), "<redacted>");
    assert_eq!(secret.expose_secret_bytes(), b"do-not-print");
    assert_eq!(secret.expose_secret_str().unwrap(), "do-not-print");
}

#[test]
fn literal_resolution_preserves_whitespace() {
    let (store, _, _directory) = test_store();
    let reference: SecretRef = ron::from_str(r#"Value("  secret value  ")"#).unwrap();

    let secret = store.resolve(&reference).unwrap();

    assert_eq!(secret.expose_secret_str().unwrap(), "  secret value  ");
}

#[test]
fn environment_errors_are_distinct_and_values_are_not_trimmed() {
    let missing = environment_secret("QQ_TEST_MISSING", None).unwrap_err();
    let empty = environment_secret("QQ_TEST_EMPTY", Some(OsString::from(""))).unwrap_err();
    let present = environment_secret("QQ_TEST_PRESENT", Some(OsString::from(" value\n"))).unwrap();

    assert!(matches!(missing, AuthError::EnvironmentMissing { .. }));
    assert!(matches!(empty, AuthError::EnvironmentEmpty { .. }));
    assert_eq!(present.expose_secret_str().unwrap(), " value\n");
}

#[test]
fn a_built_in_provider_without_any_credential_names_both_remedies() {
    let (store, _keyring, _directory) = test_store();

    let error = resolve_provider_credential(
        &store,
        None,
        "openai/default",
        "QQ_TEST_OPENAI_KEY_THAT_DOES_NOT_EXIST_3F2A",
        Some("https://api.openai.com"),
    )
    .unwrap_err();

    assert!(matches!(
        &error,
        AuthError::ProviderCredentialMissing { provider, environment_variable, alternate_variables }
            if provider == "openai"
                && environment_variable == "QQ_TEST_OPENAI_KEY_THAT_DOES_NOT_EXIST_3F2A"
                && alternate_variables.is_empty()
    ));
    let message = error.to_string();
    assert_eq!(
        message,
        "no credential for provider `openai`: run `qq auth login openai` or set the \
         environment variable `QQ_TEST_OPENAI_KEY_THAT_DOES_NOT_EXIST_3F2A`"
    );

    // An explicit Env(...) reference is the user's own choice of variable;
    // it keeps the plain environment error.
    let explicit = store
        .resolve(&SecretRef::Env(
            "QQ_TEST_OPENAI_KEY_THAT_DOES_NOT_EXIST_3F2A".to_owned(),
        ))
        .unwrap_err();
    assert!(matches!(explicit, AuthError::EnvironmentMissing { .. }));
}

#[test]
fn a_provider_with_alias_variables_names_every_variable_when_none_is_set() {
    let (store, _keyring, _directory) = test_store();

    let error = resolve_provider_credential_with_aliases(
        &store,
        None,
        "google/default",
        "QQ_TEST_GEMINI_KEY_THAT_DOES_NOT_EXIST_7B1C",
        &["QQ_TEST_GOOGLE_KEY_THAT_DOES_NOT_EXIST_7B1C"],
        Some("https://generativelanguage.googleapis.com"),
    )
    .unwrap_err();

    assert!(matches!(
        &error,
        AuthError::ProviderCredentialMissing { provider, .. } if provider == "google"
    ));
    assert_eq!(
        error.to_string(),
        "no credential for provider `google`: run `qq auth login google` or set the \
         environment variable `QQ_TEST_GEMINI_KEY_THAT_DOES_NOT_EXIST_7B1C` \
         (or `QQ_TEST_GOOGLE_KEY_THAT_DOES_NOT_EXIST_7B1C`)"
    );
}

#[test]
fn alias_variables_are_read_in_precedence_order_and_an_empty_one_is_reported() {
    const PRIMARY: &str = "QQ_TEST_GEMINI_KEY_A91E";
    const ALIAS: &str = "QQ_TEST_GOOGLE_KEY_A91E";
    let lookup = |values: &[(&str, &str)]| {
        let values = values
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(*value)))
            .collect::<BTreeMap<_, _>>();
        move |variable: &str| values.get(variable).cloned()
    };

    let only_alias =
        environment_secret_from_any(PRIMARY, &[ALIAS], lookup(&[(ALIAS, "from-alias")])).unwrap();
    assert_eq!(only_alias.expose_secret_str().unwrap(), "from-alias");

    let both = environment_secret_from_any(
        PRIMARY,
        &[ALIAS],
        lookup(&[(PRIMARY, "from-primary"), (ALIAS, "from-alias")]),
    )
    .unwrap();
    assert_eq!(both.expose_secret_str().unwrap(), "from-primary");

    let empty_primary = environment_secret_from_any(
        PRIMARY,
        &[ALIAS],
        lookup(&[(PRIMARY, ""), (ALIAS, "from-alias")]),
    )
    .unwrap_err();
    assert!(matches!(
        empty_primary,
        AuthError::EnvironmentEmpty { variable } if variable == PRIMARY
    ));

    let neither = environment_secret_from_any(PRIMARY, &[ALIAS], lookup(&[])).unwrap_err();
    assert!(matches!(
        neither,
        AuthError::EnvironmentMissing { variable } if variable == PRIMARY
    ));
}

#[test]
fn public_environment_resolution_reports_a_missing_value() {
    let (store, _keyring, _directory) = test_store();
    let reference = SecretRef::Env("QQ_TEST_ENV_THAT_DOES_NOT_EXIST_90D1".to_owned());

    assert!(matches!(
        store.resolve(&reference).unwrap_err(),
        AuthError::EnvironmentMissing { .. }
    ));
}

#[cfg(unix)]
#[test]
fn non_unicode_environment_value_is_distinct() {
    use std::os::unix::ffi::OsStringExt;

    let error = environment_secret("QQ_TEST_NON_UNICODE", Some(OsString::from_vec(vec![0xff])))
        .unwrap_err();

    assert!(matches!(error, AuthError::EnvironmentNotUnicode { .. }));
}

#[test]
fn credential_names_are_strictly_validated() {
    for valid in [
        "a",
        "openai",
        "provider/openai",
        "provider/openai.api-key_2",
    ] {
        assert!(validate_credential_name(valid).is_ok(), "{valid}");
    }

    let too_long = format!("a{}", "b".repeat(MAX_CREDENTIAL_NAME_LEN));
    for invalid in [
        "",
        "Openai",
        "1openai",
        "open ai",
        "openai/",
        "openai//key",
        "openai/../key",
        "openai..key",
        &too_long,
    ] {
        assert!(validate_credential_name(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn keyring_success_records_and_resolves_the_backend() {
    let (store, keyring, _directory) = test_store();

    let backend = store.set("openai", "  key  ", false).unwrap();

    assert_eq!(backend, CredentialBackend::Keyring);
    assert_eq!(keyring.value("openai").unwrap(), b"  key  ");
    assert_eq!(
        store
            .resolve(&SecretRef::Stored("openai".to_owned()))
            .unwrap()
            .expose_secret_str()
            .unwrap(),
        "  key  "
    );
    assert_eq!(
        store.status("openai").unwrap().unwrap().backend,
        CredentialBackend::Keyring
    );
}

#[test]
fn credential_epoch_advances_on_every_durable_mutation_and_survives_reload() {
    let (store, _keyring, directory) = test_store();
    assert_eq!(store.epoch().unwrap(), CredentialEpoch::NONE);

    store.set("openai", "first", false).unwrap();
    let after_set = store.epoch().unwrap();
    assert!(after_set > CredentialEpoch::NONE);

    // Reading secrets does not move the epoch, and a resolve reports the
    // epoch of the index it read from, for either reference kind.
    store
        .resolve(&SecretRef::Stored("openai".to_owned()))
        .unwrap();
    assert_eq!(store.epoch().unwrap(), after_set);
    let (secret, epoch) = store
        .resolve_with_epoch(&SecretRef::Stored("openai".to_owned()), None)
        .unwrap();
    assert_eq!(secret.expose_secret_str().unwrap(), "first");
    assert_eq!(epoch, after_set);
    let (_, epoch) = store
        .resolve_with_epoch(&SecretRef::Value("inline".to_owned().into()), None)
        .unwrap();
    assert_eq!(epoch, after_set);

    store.set("openai", "rotated", false).unwrap();
    let after_rotation = store.epoch().unwrap();
    assert!(after_rotation > after_set);

    assert!(store.remove("openai").unwrap());
    let after_remove = store.epoch().unwrap();
    assert!(after_remove > after_rotation);

    // A second store over the same files observes the persisted revision.
    let reopened =
        CredentialStore::with_backend(store.paths().clone(), Arc::new(FakeKeyring::default()));
    assert_eq!(reopened.epoch().unwrap(), after_remove);
    drop(directory);
}

#[test]
fn legacy_index_without_a_revision_loads_as_epoch_zero_and_then_advances() {
    let (store, _keyring, _directory) = test_store();
    write_private(store.paths().index_file(), "(version: 2, records: [])");
    assert_eq!(store.epoch().unwrap(), CredentialEpoch::NONE);
    store.set("anthropic", "key", false).unwrap();
    assert_eq!(store.epoch().unwrap(), CredentialEpoch::new(1));
}

#[test]
fn unavailable_keyring_never_silently_falls_back() {
    let (store, keyring, _directory) = test_store();
    keyring.set_mode(FakeMode::Unavailable);

    let error = store.set("openai", "secret", false).unwrap_err();

    assert!(matches!(error, AuthError::FileFallbackNotAllowed { .. }));
    assert!(!store.paths().fallback_file().exists());
    assert!(!store.paths().index_file().exists());
}

#[test]
fn keyring_failures_do_not_trigger_an_allowed_fallback() {
    let (store, keyring, _directory) = test_store();
    keyring.set_mode(FakeMode::Failure);

    let error = store.set("openai", "secret", true).unwrap_err();

    assert!(matches!(error, AuthError::KeyringFailure { .. }));
    assert!(!store.paths().fallback_file().exists());
}

#[test]
fn oversized_keyring_secrets_use_windows_protection_without_plaintext_permission() {
    let (store, keyring, protected, _directory) = test_store_with_protected();
    keyring.set_max_secret_len(16);

    assert_eq!(
        store.set("small", b"small-secret", false).unwrap(),
        CredentialBackend::Keyring
    );
    let oversized = vec![b'x'; 32];
    assert_eq!(
        store.set("large", &oversized, false).unwrap(),
        CredentialBackend::WindowsProtectedFile
    );

    assert_eq!(keyring.value("small").unwrap(), b"small-secret");
    assert!(keyring.value("large").is_none());
    assert_eq!(protected.value("large").unwrap(), oversized);
    assert_eq!(
        store
            .resolve(&SecretRef::Stored("large".to_owned()))
            .unwrap()
            .expose_secret_bytes(),
        oversized
    );
    assert_eq!(
        store.status("large").unwrap().unwrap().backend,
        CredentialBackend::WindowsProtectedFile
    );
}

#[test]
fn growing_keyring_secret_migrates_and_removes_the_old_entry() {
    let (store, keyring, protected, _directory) = test_store_with_protected();
    keyring.set_max_secret_len(16);
    store.set("openai", b"old-secret", false).unwrap();

    let replacement = vec![b'n'; 32];
    assert_eq!(
        store.set("openai", &replacement, false).unwrap(),
        CredentialBackend::WindowsProtectedFile
    );

    assert!(keyring.value("openai").is_none());
    assert_eq!(protected.value("openai").unwrap(), replacement);
    assert_eq!(
        store
            .resolve(&SecretRef::Stored("openai".to_owned()))
            .unwrap()
            .expose_secret_bytes(),
        replacement
    );
}

#[test]
fn windows_protection_failure_preserves_the_existing_keyring_credential() {
    let (store, keyring, protected, _directory) = test_store_with_protected();
    keyring.set_max_secret_len(16);
    store.set("openai", b"old-secret", false).unwrap();
    protected.set_mode(FakeMode::Failure);

    let error = store.set("openai", vec![b'n'; 32], false).unwrap_err();

    assert!(matches!(error, AuthError::WindowsProtectionFailure { .. }));
    assert_eq!(keyring.value("openai").unwrap(), b"old-secret");
    assert_eq!(
        store.status("openai").unwrap().unwrap().backend,
        CredentialBackend::Keyring
    );
}

#[test]
fn removing_windows_protected_credential_removes_secret_and_metadata() {
    let (store, keyring, protected, _directory) = test_store_with_protected();
    keyring.set_max_secret_len(4);
    store.set("openai", b"oversized", false).unwrap();

    assert!(store.remove("openai").unwrap());
    assert!(protected.value("openai").is_none());
    assert!(store.status("openai").unwrap().is_none());
}

#[test]
fn version_one_index_is_upgraded_on_the_next_write() {
    let (store, _keyring, _protected, _directory) = test_store_with_protected();
    write_private(store.paths().index_file(), b"(version:1,records:[])\n");

    store.set("openai", b"secret", false).unwrap();

    let persisted = fs::read_to_string(store.paths().index_file()).unwrap();
    assert!(persisted.contains("version: 2"));
}

#[cfg(windows)]
#[test]
fn windows_dpapi_round_trip_replaces_and_removes_oversized_secret() {
    let directory = TestDirectory::new();
    let paths = CredentialPaths::new(directory.path());
    let store = CredentialStore::with_paths(paths.clone());
    let first = vec![b'a'; 4_300];
    let second = vec![b'b'; 4_400];

    assert_eq!(
        store.set("openai-codex/default", &first, false).unwrap(),
        CredentialBackend::WindowsProtectedFile
    );
    assert_eq!(
        store.set("openai-codex/default", &second, false).unwrap(),
        CredentialBackend::WindowsProtectedFile
    );
    drop(store);
    let store = CredentialStore::with_paths(paths);
    assert_eq!(
        store
            .resolve(&SecretRef::Stored("openai-codex/default".to_owned()))
            .unwrap()
            .expose_secret_bytes(),
        second
    );
    let protected_path = SystemWindowsProtected::new(CredentialPaths::new(directory.path()))
        .path("openai-codex/default");
    let ciphertext = fs::read(&protected_path).unwrap();
    assert!(
        !ciphertext
            .windows(second.len())
            .any(|window| window == second)
    );
    assert!(store.remove("openai-codex/default").unwrap());
    assert!(!protected_path.exists());
}

#[cfg(unix)]
#[test]
fn explicit_file_fallback_is_private_and_resolvable() {
    use std::os::unix::fs::PermissionsExt;

    let (store, keyring, _directory) = test_store();
    keyring.set_mode(FakeMode::Unavailable);

    let backend = store.set("openai", " file secret ", true).unwrap();

    assert_eq!(backend, CredentialBackend::File);
    assert_eq!(
        store
            .resolve(&SecretRef::Stored("openai".to_owned()))
            .unwrap()
            .expose_secret_str()
            .unwrap(),
        " file secret "
    );
    assert_eq!(
        fs::metadata(store.paths().data_dir())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    for path in [
        store.paths().fallback_file(),
        store.paths().index_file(),
        store.paths().lock_file(),
    ] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert!(store.remove("openai").unwrap());
    assert!(store.status("openai").unwrap().is_none());
    assert!(matches!(
        store
            .resolve(&SecretRef::Stored("openai".to_owned()))
            .unwrap_err(),
        AuthError::StoredCredentialNotRegistered { .. }
    ));
}

#[test]
fn endpoint_binding_is_normalized_and_enforced() {
    let (store, _keyring, _directory) = test_store();
    store
        .set_with_metadata(
            "openai",
            "secret",
            false,
            Some("openai"),
            Some("HTTPS://API.Example.TEST:443/v1/"),
        )
        .unwrap();
    let reference = SecretRef::Stored("openai".to_owned());

    assert_eq!(
        store.status("openai").unwrap().unwrap().endpoint.as_deref(),
        Some("https://api.example.test/v1")
    );
    assert!(matches!(
        store.resolve(&reference).unwrap_err(),
        AuthError::EndpointRequired { .. }
    ));
    assert_eq!(
        store
            .resolve_with_endpoint(&reference, Some("https://api.example.test/v1"))
            .unwrap()
            .expose_secret_str()
            .unwrap(),
        "secret"
    );
    assert!(matches!(
        store
            .resolve_with_endpoint(&reference, Some("https://other.example.test/v1"))
            .unwrap_err(),
        AuthError::EndpointMismatch { .. }
    ));
}

#[test]
fn list_status_and_remove_use_index_metadata() {
    let (store, keyring, _directory) = test_store();
    store
        .set_with_metadata("zeta", "z", false, Some("custom"), None)
        .unwrap();
    store.set("alpha", "a", false).unwrap();

    let list = store.list().unwrap();
    assert_eq!(
        list.iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "zeta"]
    );
    assert_eq!(list[1].kind.as_deref(), Some("custom"));
    assert!(store.is_registered("alpha").unwrap());

    assert!(store.remove("alpha").unwrap());
    assert!(!store.remove("alpha").unwrap());
    assert!(keyring.value("alpha").is_none());
    assert!(store.status("alpha").unwrap().is_none());
}

#[test]
fn missing_keyring_delete_is_idempotent_but_failures_are_not_swallowed() {
    let (store, keyring, _directory) = test_store();
    store.set("missing", "secret", false).unwrap();
    keyring.erase("missing");
    assert!(store.remove("missing").unwrap());

    store.set("failing", "secret", false).unwrap();
    keyring.set_mode(FakeMode::Failure);
    assert!(matches!(
        store.remove("failing").unwrap_err(),
        AuthError::KeyringFailure { .. }
    ));
    keyring.set_mode(FakeMode::Available);
    assert!(store.status("failing").unwrap().is_some());
}

#[test]
fn provider_resolution_does_not_fall_back_past_a_registered_record() {
    let (store, keyring, _directory) = test_store();
    store.set("openai", "secret", false).unwrap();
    keyring.erase("openai");

    let error = resolve_provider_credential(
        &store,
        None,
        "openai",
        "QQ_TEST_ENV_THAT_DOES_NOT_EXIST_4F2D",
        None,
    )
    .unwrap_err();

    assert!(matches!(error, AuthError::StoredCredentialMissing { .. }));
}

#[test]
fn provider_resolution_prefers_an_explicit_reference() {
    let (store, _keyring, _directory) = test_store();
    let explicit: SecretRef = ron::from_str(r#"Value("explicit")"#).unwrap();

    let secret = resolve_provider_credential(
        &store,
        Some(&explicit),
        "not/a/valid/../stored/name",
        "QQ_TEST_ENV_THAT_DOES_NOT_EXIST_56AA",
        None,
    )
    .unwrap();

    assert_eq!(secret.expose_secret_str().unwrap(), "explicit");
}

#[test]
fn codex_login_uses_pkce_rejects_wrong_state_and_stores_tokens() {
    let (mut store, _keyring, _directory) = test_store();
    let id_token = jwt(serde_json::json!({
        "https://api.openai.com/auth": {
            "chatgpt_account_id": "workspace-test-id",
            "chatgpt_account_is_fedramp": true
        }
    }));
    let client = Arc::new(FakeCodexTokenClient::new(
        codex::ExchangedTokens {
            id_token,
            access_token: "access-token".to_owned(),
            refresh_token: "refresh-token".to_owned(),
        },
        codex::RefreshedTokens::default(),
    ));
    store.codex_client = client.clone();
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let login =
        CodexLogin::start_for_test(0, "known-state", verifier, Duration::from_secs(5)).unwrap();
    let authorization = reqwest::Url::parse(login.authorization_url()).unwrap();
    let parameters = authorization
        .query_pairs()
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect::<BTreeMap<_, _>>();
    let redirect = reqwest::Url::parse(&parameters["redirect_uri"]).unwrap();
    let port = redirect.port().unwrap();

    assert_eq!(
        authorization.as_str().split('?').next().unwrap(),
        "https://auth.openai.com/oauth/authorize"
    );
    assert_eq!(parameters["response_type"], "code");
    assert_eq!(parameters["client_id"], codex::CLIENT_ID);
    assert_eq!(
        parameters["scope"],
        "openid profile email offline_access api.connectors.read api.connectors.invoke"
    );
    assert_eq!(
        parameters["code_challenge"],
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
    assert_eq!(parameters["code_challenge_method"], "S256");
    assert_eq!(parameters["id_token_add_organizations"], "true");
    assert_eq!(parameters["codex_cli_simplified_flow"], "true");
    assert_eq!(parameters["state"], "known-state");
    assert_eq!(parameters["originator"], "qq");

    let completion_store = store.clone();
    let completion = thread::spawn(move || login.complete(&completion_store, "default", false));
    let mismatch = callback(port, "code=ignored&state=wrong-state");
    assert!(mismatch.starts_with("HTTP/1.1 400"));
    let success = callback(port, "code=authorization-code&state=known-state");
    assert!(success.starts_with("HTTP/1.1 200"));
    assert_eq!(
        completion.join().unwrap().unwrap(),
        CredentialBackend::Keyring
    );

    assert_eq!(
        client.exchanges.lock().unwrap().as_slice(),
        [(
            "authorization-code".to_owned(),
            parameters["redirect_uri"].clone(),
            verifier.to_owned()
        )]
    );
    let credential = store.resolve_codex("default").unwrap();
    assert_eq!(
        credential.access_token().expose_secret_str().unwrap(),
        "access-token"
    );
    assert_eq!(credential.account_id(), "workspace-test-id");
    assert!(credential.is_fedramp());
    let metadata = store.status("openai-codex/default").unwrap().unwrap();
    assert_eq!(metadata.kind.as_deref(), Some("openai-codex"));
    assert_eq!(metadata.endpoint.as_deref(), Some("https://chatgpt.com"));
}

#[test]
fn codex_login_routes_a_realistic_oversized_bundle_to_windows_protection() {
    let (mut store, keyring, protected, _directory) = test_store_with_protected();
    keyring.set_max_secret_len(2_560);
    let id_token = jwt(serde_json::json!({
        "https://api.openai.com/auth": {
            "chatgpt_account_id": "workspace-test-id",
            "chatgpt_account_is_fedramp": false
        },
        "padding": "x".repeat(1_400)
    }));
    let access_token = "a".repeat(1_750);
    let refresh_token = "r".repeat(200);
    store.codex_client = Arc::new(FakeCodexTokenClient::new(
        codex::ExchangedTokens {
            id_token,
            access_token: access_token.clone(),
            refresh_token,
        },
        codex::RefreshedTokens::default(),
    ));
    let login = CodexLogin::start_for_test(
        0,
        "known-state",
        "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
        Duration::from_secs(5),
    )
    .unwrap();
    let authorization = reqwest::Url::parse(login.authorization_url()).unwrap();
    let redirect_uri = authorization
        .query_pairs()
        .find(|(name, _)| name == "redirect_uri")
        .unwrap()
        .1;
    let port = reqwest::Url::parse(&redirect_uri).unwrap().port().unwrap();

    let completion_store = store.clone();
    let completion = thread::spawn(move || login.complete(&completion_store, "default", false));
    let response = callback(port, "code=authorization-code&state=known-state");
    assert!(response.starts_with("HTTP/1.1 200"));

    assert_eq!(
        completion.join().unwrap().unwrap(),
        CredentialBackend::WindowsProtectedFile
    );
    assert!(protected.value("openai-codex/default").unwrap().len() > 2_560);
    assert!(keyring.value("openai-codex/default").is_none());
    assert_eq!(
        store
            .resolve_codex("default")
            .unwrap()
            .access_token()
            .expose_secret_str()
            .unwrap(),
        access_token
    );
}

#[test]
fn codex_resolution_refreshes_an_expired_access_token_once() {
    let (mut store, _keyring, _directory) = test_store();
    let expires_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3_600;
    let refreshed_access_token = jwt(serde_json::json!({"exp": expires_at}));
    let client = Arc::new(FakeCodexTokenClient::new(
        codex::ExchangedTokens {
            id_token: String::new(),
            access_token: String::new(),
            refresh_token: String::new(),
        },
        codex::RefreshedTokens {
            id_token: None,
            access_token: Some(refreshed_access_token.clone()),
            refresh_token: Some("rotated-refresh-token".to_owned()),
        },
    ));
    store.codex_client = client.clone();
    let expired_access_token = jwt(serde_json::json!({"exp": 1}));
    let stored = serde_json::json!({
        "version": 1,
        "id_token": jwt(serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "workspace-test-id",
                "chatgpt_account_is_fedramp": false
            }
        })),
        "access_token": expired_access_token,
        "refresh_token": "original-refresh-token",
        "account_id": "workspace-test-id",
        "is_fedramp": false,
        "refreshed_at": 0
    });
    store
        .set_with_metadata(
            "openai-codex/work",
            serde_json::to_vec(&stored).unwrap(),
            false,
            Some("openai-codex"),
            Some("https://chatgpt.com"),
        )
        .unwrap();

    let first = store.resolve_codex("work").unwrap();
    let second = store.resolve_codex("work").unwrap();

    assert_eq!(
        first.access_token().expose_secret_str().unwrap(),
        refreshed_access_token
    );
    assert_eq!(
        second.access_token().expose_secret_str().unwrap(),
        refreshed_access_token
    );
    assert_eq!(first.account_id(), "workspace-test-id");
    assert_eq!(
        client.refreshes.lock().unwrap().as_slice(),
        ["original-refresh-token"]
    );
}

#[tokio::test]
async fn codex_request_credentials_reuse_one_keyring_read_for_concurrent_requests() {
    let (store, keyring, _directory) = test_store();
    let expires_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3_600;
    store
        .set_with_metadata(
            "openai-codex/work",
            stored_codex_credential(jwt(serde_json::json!({"exp": expires_at})), "refresh"),
            false,
            Some("openai-codex"),
            Some("https://chatgpt.com"),
        )
        .unwrap();
    let provider = Arc::new(codex_request_provider(&store, "work"));

    let requests = (0..8)
        .map(|_| {
            let provider = Arc::clone(&provider);
            tokio::spawn(async move {
                qq_provider::RequestCredentialProvider::credential(provider.as_ref()).await
            })
        })
        .collect::<Vec<_>>();
    for request in requests {
        request.await.unwrap().unwrap();
    }

    assert_eq!(keyring.read_count("openai-codex/work"), 1);
}

#[tokio::test]
async fn codex_request_cache_reloads_after_rotation_and_rejects_deletion() {
    let (store, keyring, _directory) = test_store();
    let expires_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3_600;
    let metadata = (Some("openai-codex"), Some("https://chatgpt.com"));
    store
        .set_with_metadata(
            "openai-codex/work",
            stored_codex_credential(jwt(serde_json::json!({"exp": expires_at})), "refresh-1"),
            false,
            metadata.0,
            metadata.1,
        )
        .unwrap();
    let provider = codex_request_provider(&store, "work");
    qq_provider::RequestCredentialProvider::credential(&provider)
        .await
        .unwrap();

    let rotated_access_token = jwt(serde_json::json!({"exp": expires_at + 1}));
    store
        .set_with_metadata(
            "openai-codex/work",
            stored_codex_credential(rotated_access_token.clone(), "refresh-2"),
            false,
            metadata.0,
            metadata.1,
        )
        .unwrap();
    qq_provider::RequestCredentialProvider::credential(&provider)
        .await
        .unwrap();
    assert_eq!(keyring.read_count("openai-codex/work"), 2);
    assert_eq!(
        provider
            .cache
            .lock()
            .await
            .as_ref()
            .unwrap()
            .credential
            .access_token()
            .expose_secret_str()
            .unwrap(),
        rotated_access_token
    );

    assert!(store.remove("openai-codex/work").unwrap());
    let error = qq_provider::RequestCredentialProvider::credential(&provider)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        qq_provider::RequestCredentialError::Missing { .. }
    ));
    assert_eq!(
        error.to_string(),
        "credential `openai-codex/work` is not registered: run `qq auth login openai-codex --profile work`"
    );
    assert_eq!(keyring.read_count("openai-codex/work"), 2);
}

#[tokio::test]
async fn xai_request_credentials_name_the_provider_and_both_remedies_when_nothing_exists() {
    let (store, _keyring, _directory) = test_store();
    let provider = store.xai_request_credentials("default", None);

    let error = qq_provider::RequestCredentialProvider::credential(&provider)
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        qq_provider::RequestCredentialError::Missing { .. }
    ));
    // Depends on `XAI_API_KEY` being unset in the test environment, which
    // the suite requires for the plain-resolution tests above as well.
    assert_eq!(
        error.to_string(),
        "no credential for provider `xai`: run `qq auth login xai --oauth` or \
         `qq auth login xai` or set the environment variable `XAI_API_KEY`"
    );
}

#[tokio::test]
async fn xai_request_credentials_name_a_missing_profile_with_its_flag() {
    let (store, _keyring, _directory) = test_store();
    let provider = store.xai_request_credentials("work", None);

    let error = qq_provider::RequestCredentialProvider::credential(&provider)
        .await
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "credential `xai/work` is not registered: run `qq auth login xai --oauth --profile work` \
         or `qq auth login xai --profile work` or set the environment variable `XAI_API_KEY`"
    );
}

#[tokio::test]
async fn xai_request_credentials_report_a_registered_name_whose_secret_is_gone() {
    let (store, keyring, _directory) = test_store();
    store
        .set_with_metadata(
            "xai/default",
            "secret",
            false,
            Some("xai"),
            Some("https://api.x.ai"),
        )
        .unwrap();
    keyring.erase("xai/default");
    let provider = store.xai_request_credentials("default", None);

    let error = qq_provider::RequestCredentialProvider::credential(&provider)
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        qq_provider::RequestCredentialError::Missing { .. }
    ));
    assert_eq!(
        error.to_string(),
        "credential `xai/default` is registered, but its secret is missing: run \
         `qq auth logout xai/default`, then `qq auth login xai --oauth` or `qq auth login xai`"
    );
}

#[tokio::test]
async fn xai_request_credentials_keep_the_message_of_an_explicit_reference() {
    let (store, _keyring, _directory) = test_store();
    let provider = store.xai_request_credentials(
        "default",
        Some(SecretRef::Env(
            "QQ_TEST_XAI_EXPLICIT_THAT_DOES_NOT_EXIST_C4D2".to_owned(),
        )),
    );

    let error = qq_provider::RequestCredentialProvider::credential(&provider)
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        qq_provider::RequestCredentialError::Missing { .. }
    ));
    assert_eq!(
        error.to_string(),
        "environment variable `QQ_TEST_XAI_EXPLICIT_THAT_DOES_NOT_EXIST_C4D2` is not set"
    );
}

#[tokio::test]
async fn codex_request_credentials_name_the_provider_when_nothing_is_stored() {
    let (store, _keyring, _directory) = test_store();
    let provider = store.codex_request_credentials("default");

    let error = qq_provider::RequestCredentialProvider::credential(&provider)
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        qq_provider::RequestCredentialError::Missing { .. }
    ));
    assert_eq!(
        error.to_string(),
        "no credential for provider `openai-codex`: run `qq auth login openai-codex`"
    );
}

#[tokio::test]
async fn codex_request_cache_rechecks_endpoint_binding_after_index_revision() {
    let (store, keyring, _directory) = test_store();
    let expires_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3_600;
    let stored = stored_codex_credential(jwt(serde_json::json!({"exp": expires_at})), "refresh");
    store
        .set_with_metadata(
            "openai-codex/work",
            &stored,
            false,
            Some("openai-codex"),
            Some("https://chatgpt.com"),
        )
        .unwrap();
    let provider = codex_request_provider(&store, "work");
    qq_provider::RequestCredentialProvider::credential(&provider)
        .await
        .unwrap();

    store
        .set_with_metadata(
            "openai-codex/work",
            &stored,
            false,
            Some("openai-codex"),
            Some("https://other.example.test"),
        )
        .unwrap();
    assert_eq!(
        qq_provider::RequestCredentialProvider::credential(&provider)
            .await
            .unwrap_err(),
        qq_provider::RequestCredentialError::StorageUnavailable
    );
    assert_eq!(keyring.read_count("openai-codex/work"), 1);
}

#[tokio::test]
async fn codex_request_cache_coalesces_expired_refresh() {
    let (mut store, keyring, _directory) = test_store();
    let expires_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3_600;
    let client = Arc::new(FakeCodexTokenClient::new(
        codex::ExchangedTokens {
            id_token: String::new(),
            access_token: String::new(),
            refresh_token: String::new(),
        },
        codex::RefreshedTokens {
            id_token: None,
            access_token: Some(jwt(serde_json::json!({"exp": expires_at}))),
            refresh_token: Some("rotated-refresh".to_owned()),
        },
    ));
    store.codex_client = client.clone();
    store
        .set_with_metadata(
            "openai-codex/work",
            stored_codex_credential(jwt(serde_json::json!({"exp": 1})), "original-refresh"),
            false,
            Some("openai-codex"),
            Some("https://chatgpt.com"),
        )
        .unwrap();
    let provider = Arc::new(codex_request_provider(&store, "work"));

    let requests = (0..4)
        .map(|_| {
            let provider = Arc::clone(&provider);
            tokio::spawn(async move {
                qq_provider::RequestCredentialProvider::credential(provider.as_ref()).await
            })
        })
        .collect::<Vec<_>>();
    for request in requests {
        request.await.unwrap().unwrap();
    }

    assert_eq!(
        client.refreshes.lock().unwrap().as_slice(),
        ["original-refresh"]
    );
    assert_eq!(keyring.read_count("openai-codex/work"), 3);
}

#[tokio::test]
async fn codex_request_cache_fails_closed_when_rotation_index_commit_fails() {
    let (store, keyring, _directory) = test_store();
    let expires_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3_600;
    store
        .set_with_metadata(
            "openai-codex/work",
            stored_codex_credential(jwt(serde_json::json!({"exp": expires_at})), "refresh-1"),
            false,
            Some("openai-codex"),
            Some("https://chatgpt.com"),
        )
        .unwrap();
    let provider = codex_request_provider(&store, "work");
    qq_provider::RequestCredentialProvider::credential(&provider)
        .await
        .unwrap();

    keyring.break_index_after_next_set(store.paths().index_file());
    let rotated =
        stored_codex_credential(jwt(serde_json::json!({"exp": expires_at + 1})), "refresh-2");
    assert!(
        store
            .set_with_metadata(
                "openai-codex/work",
                &rotated,
                false,
                Some("openai-codex"),
                Some("https://chatgpt.com"),
            )
            .is_err()
    );
    assert_eq!(keyring.value("openai-codex/work").unwrap(), rotated);
    assert_eq!(
        qq_provider::RequestCredentialProvider::credential(&provider)
            .await
            .unwrap_err(),
        qq_provider::RequestCredentialError::StorageUnavailable
    );
}

#[tokio::test]
async fn codex_rotation_does_not_touch_backend_when_pre_invalidation_fails() {
    let (store, keyring, _directory) = test_store();
    let expires_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3_600;
    let original =
        stored_codex_credential(jwt(serde_json::json!({"exp": expires_at})), "refresh-1");
    store
        .set_with_metadata(
            "openai-codex/work",
            &original,
            false,
            Some("openai-codex"),
            Some("https://chatgpt.com"),
        )
        .unwrap();

    fs::remove_file(store.paths().index_file()).unwrap();
    fs::create_dir(store.paths().index_file()).unwrap();
    assert!(
        store
            .set_with_metadata(
                "openai-codex/work",
                stored_codex_credential(
                    jwt(serde_json::json!({"exp": expires_at + 1})),
                    "refresh-2",
                ),
                false,
                Some("openai-codex"),
                Some("https://chatgpt.com"),
            )
            .is_err()
    );
    assert_eq!(keyring.value("openai-codex/work").unwrap(), original);
}

#[tokio::test]
async fn codex_request_cache_does_not_mask_refresh_failures_after_rotation() {
    for (failure, expected) in [
        (
            FakeCodexRefreshFailure::Rejected,
            qq_provider::RequestCredentialError::RefreshRejected,
        ),
        (
            FakeCodexRefreshFailure::Unavailable,
            qq_provider::RequestCredentialError::RefreshUnavailable,
        ),
    ] {
        let (mut store, _keyring, _directory) = test_store();
        store.codex_client = Arc::new(FailingCodexTokenClient(failure));
        let expires_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3_600;
        store
            .set_with_metadata(
                "openai-codex/work",
                stored_codex_credential(jwt(serde_json::json!({"exp": expires_at})), "refresh"),
                false,
                Some("openai-codex"),
                Some("https://chatgpt.com"),
            )
            .unwrap();
        let provider = codex_request_provider(&store, "work");
        qq_provider::RequestCredentialProvider::credential(&provider)
            .await
            .unwrap();

        store
            .set_with_metadata(
                "openai-codex/work",
                stored_codex_credential(jwt(serde_json::json!({"exp": 1})), "refresh"),
                false,
                Some("openai-codex"),
                Some("https://chatgpt.com"),
            )
            .unwrap();
        assert_eq!(
            qq_provider::RequestCredentialProvider::credential(&provider)
                .await
                .unwrap_err(),
            expected
        );
    }
}

#[tokio::test]
async fn codex_request_cache_rechecks_store_at_its_refresh_deadline() {
    let (store, keyring, _directory) = test_store();
    let expires_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3_600;
    store
        .set_with_metadata(
            "openai-codex/work",
            stored_codex_credential(jwt(serde_json::json!({"exp": expires_at})), "refresh"),
            false,
            Some("openai-codex"),
            Some("https://chatgpt.com"),
        )
        .unwrap();
    let provider = codex_request_provider(&store, "work");
    qq_provider::RequestCredentialProvider::credential(&provider)
        .await
        .unwrap();
    provider.cache.lock().await.as_mut().unwrap().refresh_after = 0;

    qq_provider::RequestCredentialProvider::credential(&provider)
        .await
        .unwrap();
    assert_eq!(keyring.read_count("openai-codex/work"), 2);
}

#[test]
fn xai_resolution_refreshes_once_and_persists_the_rotated_token() {
    let (mut store, keyring, _directory) = test_store();
    let client = Arc::new(FakeXaiTokenClient {
        refreshes: Mutex::new(Vec::new()),
        refreshed: xai::TokenSet {
            access_token: "refreshed-access-token".to_owned(),
            refresh_token: Some("rotated-refresh-token".to_owned()),
            expires_in: Some(3_600),
        },
    });
    store.xai_client = client.clone();
    store
        .set_with_metadata(
            "xai/work",
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "access_token": "expired-access-token",
                "refresh_token": "original-refresh-token",
                "expires_at": 1
            }))
            .unwrap(),
            false,
            Some("xai-oauth"),
            Some("https://api.x.ai"),
        )
        .unwrap();

    let first = store.resolve_xai_oauth("work").unwrap();
    let second = store.resolve_xai_oauth("work").unwrap();

    assert_eq!(first.expose_secret_str().unwrap(), "refreshed-access-token");
    assert_eq!(
        second.expose_secret_str().unwrap(),
        "refreshed-access-token"
    );
    assert_eq!(
        client.refreshes.lock().unwrap().as_slice(),
        ["original-refresh-token"]
    );
    let persisted: serde_json::Value =
        serde_json::from_slice(&keyring.value("xai/work").unwrap()).unwrap();
    assert_eq!(persisted["refresh_token"], "rotated-refresh-token");
    assert_eq!(persisted["access_token"], "refreshed-access-token");
}

#[cfg(unix)]
#[test]
fn codex_refresh_preserves_an_explicit_private_file_backend() {
    use std::os::unix::fs::PermissionsExt;

    let (mut store, keyring, _directory) = test_store();
    keyring.set_mode(FakeMode::Unavailable);
    let expires_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3_600;
    let refreshed_access_token = jwt(serde_json::json!({"exp": expires_at}));
    let client = Arc::new(FakeCodexTokenClient::new(
        codex::ExchangedTokens {
            id_token: String::new(),
            access_token: String::new(),
            refresh_token: String::new(),
        },
        codex::RefreshedTokens {
            id_token: None,
            access_token: Some(refreshed_access_token.clone()),
            refresh_token: Some("rotated-refresh-token".to_owned()),
        },
    ));
    store.codex_client = client;
    let stored = serde_json::json!({
        "version": 1,
        "id_token": jwt(serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "workspace-test-id",
                "chatgpt_account_is_fedramp": false
            }
        })),
        "access_token": jwt(serde_json::json!({"exp": 1})),
        "refresh_token": "original-refresh-token",
        "account_id": "workspace-test-id",
        "is_fedramp": false,
        "refreshed_at": 0
    });
    store
        .set_with_metadata(
            "openai-codex/file",
            serde_json::to_vec(&stored).unwrap(),
            true,
            Some("openai-codex"),
            Some("https://chatgpt.com"),
        )
        .unwrap();

    let resolved = store.resolve_codex("file").unwrap();

    assert_eq!(
        resolved.access_token().expose_secret_str().unwrap(),
        refreshed_access_token
    );
    assert_eq!(
        store.status("openai-codex/file").unwrap().unwrap().backend,
        CredentialBackend::File
    );
    assert_eq!(
        fs::metadata(store.paths().codex_lock_file())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn corrupt_unknown_and_unsupported_index_state_is_rejected() {
    let (store, _keyring, _directory) = test_store();
    assert!(store.list().unwrap().is_empty());

    write_private(store.paths().index_file(), "not ron");
    assert!(matches!(
        store.list().unwrap_err(),
        AuthError::CorruptState { .. }
    ));

    write_private(
        store.paths().index_file(),
        "(version: 1, records: [], unknown: true)",
    );
    assert!(matches!(
        store.list().unwrap_err(),
        AuthError::CorruptState { .. }
    ));

    write_private(store.paths().index_file(), "(version: 3, records: [])");
    assert!(matches!(
        store.list().unwrap_err(),
        AuthError::UnsupportedStateVersion { version: 3, .. }
    ));
}

#[test]
fn duplicate_names_in_both_state_files_are_rejected() {
    let (store, _keyring, _directory) = test_store();
    assert!(store.list().unwrap().is_empty());
    write_private(
        store.paths().index_file(),
        r#"(
            version: 1,
            records: [
                (name: "openai", backend: File),
                (name: "openai", backend: File),
            ],
        )"#,
    );
    assert!(matches!(
        store.list().unwrap_err(),
        AuthError::DuplicateCredentialName { .. }
    ));

    write_private(
        store.paths().index_file(),
        r#"(version: 1, records: [(name: "openai", backend: File)])"#,
    );
    write_private(
        store.paths().fallback_file(),
        r#"(
            version: 1,
            records: [
                (name: "openai", secret: [1]),
                (name: "openai", secret: [2]),
            ],
        )"#,
    );
    assert!(matches!(
        store
            .resolve(&SecretRef::Stored("openai".to_owned()))
            .unwrap_err(),
        AuthError::DuplicateCredentialName { .. }
    ));
}

#[test]
fn oversized_state_is_rejected_before_parsing() {
    let (store, _keyring, _directory) = test_store();
    assert!(store.list().unwrap().is_empty());
    write_private(store.paths().index_file(), vec![b'x'; MAX_STATE_BYTES + 1]);

    assert!(matches!(
        store.list().unwrap_err(),
        AuthError::StateTooLarge { .. }
    ));
}

#[cfg(unix)]
#[test]
fn symlinked_state_and_data_directories_are_rejected() {
    use std::os::unix::fs::symlink;

    let (store, _keyring, directory) = test_store();
    assert!(store.list().unwrap().is_empty());
    let target = directory.path().join("target.ron");
    write_private(&target, "(version: 1, records: [])");
    symlink(&target, store.paths().index_file()).unwrap();
    assert!(matches!(
        store.list().unwrap_err(),
        AuthError::SymlinkPath { .. }
    ));

    let outer = TestDirectory::new();
    let actual = outer.path().join("actual");
    let linked = outer.path().join("linked");
    fs::create_dir(&actual).unwrap();
    symlink(&actual, &linked).unwrap();
    let linked_store = CredentialStore::with_backend(
        CredentialPaths::new(&linked),
        Arc::new(FakeKeyring::default()),
    );
    assert!(matches!(
        linked_store.list().unwrap_err(),
        AuthError::SymlinkPath { .. }
    ));
}

#[cfg(unix)]
#[test]
fn insecure_existing_state_permissions_are_rejected() {
    use std::os::unix::fs::PermissionsExt;

    let (store, _keyring, _directory) = test_store();
    assert!(store.list().unwrap().is_empty());
    write_private(store.paths().index_file(), "(version: 1, records: [])");
    fs::set_permissions(
        store.paths().index_file(),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();

    assert!(matches!(
        store.list().unwrap_err(),
        AuthError::InsecurePermissions { .. }
    ));
}

#[cfg(unix)]
#[test]
fn file_lock_serializes_concurrent_updates() {
    let (store, keyring, _directory) = test_store();
    keyring.set_mode(FakeMode::Unavailable);
    let barrier = Arc::new(std::sync::Barrier::new(9));
    let mut threads = Vec::new();

    for index in 0..8 {
        let store = store.clone();
        let barrier = barrier.clone();
        threads.push(thread::spawn(move || {
            barrier.wait();
            store
                .set(
                    &format!("provider/key-{index}"),
                    format!("secret-{index}"),
                    true,
                )
                .unwrap();
        }));
    }
    barrier.wait();
    for thread in threads {
        thread.join().unwrap();
    }

    let list = store.list().unwrap();
    assert_eq!(list.len(), 8);
    for index in 0..8 {
        let secret = store
            .resolve(&SecretRef::Stored(format!("provider/key-{index}")))
            .unwrap();
        assert_eq!(
            secret.expose_secret_str().unwrap(),
            format!("secret-{index}")
        );
    }
}
