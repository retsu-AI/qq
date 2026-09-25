use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use reqwest::{
    Url,
    blocking::Client,
    header::{ACCEPT, CONTENT_LENGTH},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    ConfigError, ConfigPaths, MAX_CONFIG_BYTES, OrganizationEnrollment, SourceIdentity, SourceKind,
    document::Document,
    loader::{
        Probes, atomic_write, discover_file, ensure_data_directory, read_candidate,
        reject_symlink_components,
    },
};

const STATE_VERSION: u32 = 1;
const MAX_ORGANIZATION_NAME_BYTES: usize = 64;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrganizationRecord {
    name: String,
    manifest_url: String,
    cache_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrganizationState {
    version: u32,
    selected: Option<String>,
    enrollments: Vec<OrganizationRecord>,
}

impl Default for OrganizationState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            selected: None,
            enrollments: Vec::new(),
        }
    }
}

impl OrganizationState {
    fn load(paths: &ConfigPaths, probes: &mut Probes) -> Result<Self, ConfigError> {
        let Some(candidate) = discover_file(
            paths.organizations_file(),
            SourceKind::Remote,
            false,
            probes,
        )?
        else {
            return Ok(Self::default());
        };
        validate_private_state_file(&paths.organizations_file())?;
        let (source, content) = read_candidate(&candidate)?;
        let mut state: Self = ron::from_str(&content).map_err(|error| ConfigError::Parse {
            origin: source,
            message: error.to_string(),
        })?;
        if state.version != STATE_VERSION {
            return Err(ConfigError::UnsupportedOrganizationStateVersion {
                version: state.version,
            });
        }

        let mut names = BTreeSet::new();
        for enrollment in &state.enrollments {
            validate_name(&enrollment.name)?;
            let url = validate_manifest_url(&enrollment.manifest_url)?;
            if enrollment.cache_key != manifest_cache_key(&enrollment.name, url.as_str()) {
                return Err(ConfigError::OrganizationManifestMissing {
                    name: enrollment.name.clone(),
                });
            }
            if !names.insert(enrollment.name.clone()) {
                return Err(ConfigError::DuplicateOrganizationEnrollment {
                    name: enrollment.name.clone(),
                });
            }
        }
        if let Some(selected) = &state.selected {
            validate_name(selected)?;
            if !names.contains(selected) {
                return Err(ConfigError::OrganizationNotEnrolled(selected.clone()));
            }
        }
        state
            .enrollments
            .sort_by(|left, right| left.name.cmp(&right.name));
        Ok(state)
    }

    fn save(&mut self, paths: &ConfigPaths) -> Result<(), ConfigError> {
        self.enrollments
            .sort_by(|left, right| left.name.cmp(&right.name));
        ensure_data_directory(paths.data_dir())?;
        let content = ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default()).map_err(
            |error| ConfigError::StateSerialization {
                message: error.to_string(),
            },
        )?;
        if content.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::SourceTooLarge {
                origin: SourceIdentity::file(SourceKind::Remote, paths.organizations_file()),
                limit: MAX_CONFIG_BYTES,
            });
        }
        atomic_write(&paths.organizations_file(), content.as_bytes())
    }

    fn find(&self, name: &str) -> Option<&OrganizationRecord> {
        self.enrollments
            .iter()
            .find(|enrollment| enrollment.name == name)
    }

    fn metadata(&self) -> Vec<OrganizationEnrollment> {
        self.enrollments
            .iter()
            .map(|enrollment| {
                OrganizationEnrollment::new(
                    enrollment.name.clone(),
                    enrollment.manifest_url.clone(),
                    self.selected.as_deref() == Some(enrollment.name.as_str()),
                )
            })
            .collect()
    }
}

pub(super) fn enroll(
    paths: &ConfigPaths,
    name: &str,
    manifest_url: &str,
) -> Result<OrganizationEnrollment, ConfigError> {
    enroll_with(paths, name, manifest_url, fetch_manifest_content)
}

fn enroll_with(
    paths: &ConfigPaths,
    name: &str,
    manifest_url: &str,
    fetch: impl FnOnce(&str, &Url) -> Result<String, ConfigError>,
) -> Result<OrganizationEnrollment, ConfigError> {
    validate_name(name)?;
    let url = validate_manifest_url(manifest_url)?;
    let content = fetch(name, &url)?;
    validate_manifest(name, url.as_str(), &content)?;
    let cache_key = manifest_cache_key(name, url.as_str());

    let _lock = OrganizationStateLock::acquire(paths)?;
    let mut state = OrganizationState::load(paths, &mut Probes::default())?;
    let record = OrganizationRecord {
        name: name.to_owned(),
        manifest_url: url.to_string(),
        cache_key: cache_key.clone(),
    };
    if let Some(existing) = state
        .enrollments
        .iter_mut()
        .find(|enrollment| enrollment.name == name)
    {
        *existing = record;
    } else {
        state.enrollments.push(record);
    }
    if state.selected.is_none() {
        state.selected = Some(name.to_owned());
    }

    write_cached_manifest(paths, &cache_key, content.as_bytes())?;
    state.save(paths)?;
    Ok(state
        .metadata()
        .into_iter()
        .find(|enrollment| enrollment.name() == name)
        .expect("a saved enrollment must be present"))
}

pub(super) fn refresh(
    paths: &ConfigPaths,
    name: &str,
) -> Result<OrganizationEnrollment, ConfigError> {
    refresh_with(paths, name, fetch_manifest_content)
}

fn refresh_with(
    paths: &ConfigPaths,
    name: &str,
    fetch: impl FnOnce(&str, &Url) -> Result<String, ConfigError>,
) -> Result<OrganizationEnrollment, ConfigError> {
    validate_name(name)?;
    let original = {
        let _lock = OrganizationStateLock::acquire(paths)?;
        OrganizationState::load(paths, &mut Probes::default())?
            .find(name)
            .cloned()
            .ok_or_else(|| ConfigError::OrganizationNotEnrolled(name.to_owned()))?
    };
    let url = validate_manifest_url(&original.manifest_url)?;
    let content = fetch(name, &url)?;
    validate_manifest(name, url.as_str(), &content)?;

    let _lock = OrganizationStateLock::acquire(paths)?;
    let state = OrganizationState::load(paths, &mut Probes::default())?;
    if state.find(name) != Some(&original) {
        return Err(ConfigError::OrganizationEnrollmentChanged {
            name: name.to_owned(),
        });
    }
    write_cached_manifest(paths, &original.cache_key, content.as_bytes())?;
    Ok(state
        .metadata()
        .into_iter()
        .find(|enrollment| enrollment.name() == name)
        .expect("an unchanged enrollment must be present"))
}

pub(super) fn select(paths: &ConfigPaths, name: &str) -> Result<(), ConfigError> {
    validate_name(name)?;
    let _lock = OrganizationStateLock::acquire(paths)?;
    let mut state = OrganizationState::load(paths, &mut Probes::default())?;
    if state.find(name).is_none() {
        return Err(ConfigError::OrganizationNotEnrolled(name.to_owned()));
    }
    state.selected = Some(name.to_owned());
    state.save(paths)
}

pub(super) fn remove(paths: &ConfigPaths, name: &str) -> Result<bool, ConfigError> {
    validate_name(name)?;
    let _lock = OrganizationStateLock::acquire(paths)?;
    let mut state = OrganizationState::load(paths, &mut Probes::default())?;
    let Some(index) = state
        .enrollments
        .iter()
        .position(|enrollment| enrollment.name == name)
    else {
        return Ok(false);
    };
    let removed = state.enrollments.remove(index);
    if state.selected.as_deref() == Some(name) {
        state.selected = state
            .enrollments
            .first()
            .map(|enrollment| enrollment.name.clone());
    }
    state.save(paths)?;
    remove_cached_manifest(paths, &removed.cache_key);
    Ok(true)
}

pub(super) fn list(paths: &ConfigPaths) -> Result<Vec<OrganizationEnrollment>, ConfigError> {
    let _lock = OrganizationStateLock::acquire(paths)?;
    Ok(OrganizationState::load(paths, &mut Probes::default())?.metadata())
}

/// Lists enrollment metadata without creating the organization lock. Run
/// scopes use this for their inherited read-only organization source.
pub(super) fn list_read_only(
    paths: &ConfigPaths,
) -> Result<Vec<OrganizationEnrollment>, ConfigError> {
    Ok(OrganizationState::load(paths, &mut Probes::default())?.metadata())
}

pub(super) fn selected(
    paths: &ConfigPaths,
    probes: &mut Probes,
) -> Result<Option<String>, ConfigError> {
    Ok(OrganizationState::load(paths, probes)?.selected)
}

pub(super) fn load_cached(
    paths: &ConfigPaths,
    name: &str,
    probes: &mut Probes,
) -> Result<(Document, SourceIdentity), ConfigError> {
    validate_name(name)?;
    let state = OrganizationState::load(paths, probes)?;
    let enrollment = state
        .find(name)
        .ok_or_else(|| ConfigError::OrganizationNotEnrolled(name.to_owned()))?;
    let path = cache_path(paths, &enrollment.cache_key);
    let Some(candidate) = discover_file(path, SourceKind::Remote, false, probes)? else {
        return Err(ConfigError::OrganizationManifestMissing {
            name: name.to_owned(),
        });
    };
    validate_private_state_file(&cache_path(paths, &enrollment.cache_key))?;
    let (_, content) = read_candidate(&candidate)?;
    let source = remote_source(name, &enrollment.manifest_url);
    let document = Document::parse(&content, &source)?;
    if !document.matches_organization(name) {
        return Err(ConfigError::OrganizationManifestMismatch {
            name: name.to_owned(),
        });
    }
    Ok((document, source))
}

pub(super) fn load_cached_if_enrolled(
    paths: &ConfigPaths,
    name: &str,
    probes: &mut Probes,
) -> Result<Option<(Document, SourceIdentity)>, ConfigError> {
    if OrganizationState::load(paths, probes)?.find(name).is_none() {
        return Ok(None);
    }
    load_cached(paths, name, probes).map(Some)
}

fn validate_manifest(name: &str, url: &str, content: &str) -> Result<(), ConfigError> {
    if content.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::SourceTooLarge {
            origin: remote_source(name, url),
            limit: MAX_CONFIG_BYTES,
        });
    }
    let document = Document::parse(content, &remote_source(name, url))?;
    if !document.matches_organization(name) {
        return Err(ConfigError::OrganizationManifestMismatch {
            name: name.to_owned(),
        });
    }
    Ok(())
}

fn fetch_manifest_content(name: &str, url: &Url) -> Result<String, ConfigError> {
    let client = Client::builder()
        .use_rustls_tls()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("qq/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| fetch_error(name, error))?;
    let response = client
        .get(url.clone())
        .header(ACCEPT, "application/ron, text/plain;q=0.9")
        .send()
        .map_err(|error| fetch_error(name, error))?;
    if !response.status().is_success() {
        return Err(ConfigError::OrganizationHttpStatus {
            name: name.to_owned(),
            status: response.status().as_u16(),
        });
    }
    if response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_CONFIG_BYTES as u64)
    {
        return Err(ConfigError::SourceTooLarge {
            origin: remote_source(name, url.as_str()),
            limit: MAX_CONFIG_BYTES,
        });
    }

    let mut bytes = Vec::new();
    response
        .take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| ConfigError::OrganizationFetch {
            name: name.to_owned(),
            message: sanitize_message(&error.to_string()),
        })?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::SourceTooLarge {
            origin: remote_source(name, url.as_str()),
            limit: MAX_CONFIG_BYTES,
        });
    }
    String::from_utf8(bytes).map_err(|_| ConfigError::InvalidUtf8 {
        origin: remote_source(name, url.as_str()),
    })
}

fn fetch_error(name: &str, error: reqwest::Error) -> ConfigError {
    ConfigError::OrganizationFetch {
        name: name.to_owned(),
        message: sanitize_message(&error.without_url().to_string()),
    }
}

fn sanitize_message(message: &str) -> String {
    message
        .chars()
        .take(512)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn validate_name(name: &str) -> Result<(), ConfigError> {
    let bytes = name.as_bytes();
    if bytes.is_empty()
        || bytes.len() > MAX_ORGANIZATION_NAME_BYTES
        || !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit()
        || !bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
        })
    {
        return Err(ConfigError::InvalidOrganizationName);
    }
    Ok(())
}

fn validate_manifest_url(value: &str) -> Result<Url, ConfigError> {
    let url = Url::parse(value).map_err(|_| ConfigError::InvalidOrganizationManifestUrl)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigError::InvalidOrganizationManifestUrl);
    }
    Ok(url)
}

fn manifest_cache_key(name: &str, manifest_url: &str) -> String {
    let mut digest = Sha256::new();
    update_digest(&mut digest, name.as_bytes());
    update_digest(&mut digest, manifest_url.as_bytes());
    let mut encoded = String::with_capacity(64);
    for byte in digest.finalize() {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

fn update_digest(digest: &mut Sha256, value: &[u8]) {
    digest.update(value.len().to_le_bytes());
    digest.update(value);
}

fn remote_source(name: &str, manifest_url: &str) -> SourceIdentity {
    SourceIdentity::virtual_source(
        SourceKind::Remote,
        format!("organization {name} manifest ({manifest_url})"),
    )
}

fn cache_path(paths: &ConfigPaths, cache_key: &str) -> PathBuf {
    paths
        .organizations_cache_dir()
        .join(format!("{cache_key}.ron"))
}

fn write_cached_manifest(
    paths: &ConfigPaths,
    cache_key: &str,
    content: &[u8],
) -> Result<(), ConfigError> {
    ensure_data_directory(paths.data_dir())?;
    ensure_data_directory(&paths.organizations_cache_dir())?;
    atomic_write(&cache_path(paths, cache_key), content)
}

fn remove_cached_manifest(paths: &ConfigPaths, cache_key: &str) {
    let path = cache_path(paths, cache_key);
    if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
        let _ = fs::remove_file(path);
    }
}

struct OrganizationStateLock(File);

impl OrganizationStateLock {
    fn acquire(paths: &ConfigPaths) -> Result<Self, ConfigError> {
        ensure_data_directory(paths.data_dir())?;
        let path = paths.organizations_lock_file();
        reject_symlink_components(&path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path).map_err(|error| ConfigError::Io {
            path: path.clone(),
            error,
        })?;
        validate_private_state_file(&path)?;
        file.lock().map_err(|error| ConfigError::Io {
            path: path.clone(),
            error,
        })?;
        Ok(Self(file))
    }
}

impl Drop for OrganizationStateLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn validate_private_state_file(path: &Path) -> Result<(), ConfigError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| ConfigError::Io {
        path: path.to_owned(),
        error,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(ConfigError::SymlinkSource {
            path: path.to_owned(),
        });
    }
    if !metadata.is_file() {
        return Err(ConfigError::NotRegularFile {
            path: path.to_owned(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(ConfigError::InsecureStatePermissions {
                path: path.to_owned(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicU64, AtomicUsize, Ordering},
        },
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::{ConfigLoader, LoadRequest, managed};

    use super::*;

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TempState {
        root: PathBuf,
        paths: ConfigPaths,
    }

    impl TempState {
        fn new() -> Self {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "qq-remote-config-test-{}-{nanos}-{sequence}",
                std::process::id()
            ));
            let paths =
                ConfigPaths::new(root.join("global"), root.join("data"), root.join("managed"));
            fs::create_dir_all(paths.global_dir()).unwrap();
            fs::create_dir_all(paths.managed_dir()).unwrap();
            fs::create_dir_all(root.join("work/.git")).unwrap();
            Self { root, paths }
        }
    }

    impl Drop for TempState {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn manifest(name: &str, model: &str) -> String {
        format!(
            r#"(
                version: 1,
                organization: "{name}",
                model: "openai/{model}",
                providers: {{
                    "openai": OpenAi(models: {{"{model}": (name: "Remote model")}}),
                }},
            )"#
        )
    }

    struct TestMdmReader {
        reads: Arc<AtomicUsize>,
    }

    impl managed::MdmReader for TestMdmReader {
        fn read(&self) -> Result<Option<managed::MdmConfiguration>, ConfigError> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(Some(managed::MdmConfiguration::new(
                "test MDM policy",
                r#"(version: 1, organization: "acme")"#,
            )))
        }
    }

    #[test]
    fn validates_names_and_https_manifest_urls() {
        for name in ["", ".hidden", "Upper", "slash/name", "white space"] {
            assert!(matches!(
                validate_name(name),
                Err(ConfigError::InvalidOrganizationName)
            ));
        }
        validate_name("acme.dev-2").unwrap();

        for url in [
            "http://example.test/config.ron",
            "https://user@example.test/config.ron",
            "https://example.test/config.ron?token=secret",
            "https://example.test/config.ron#fragment",
        ] {
            assert!(matches!(
                validate_manifest_url(url),
                Err(ConfigError::InvalidOrganizationManifestUrl)
            ));
        }
        validate_manifest_url("https://example.test/config.ron").unwrap();
    }

    #[test]
    fn enrolls_selects_and_removes_cached_manifests() {
        let state = TempState::new();
        let first = enroll_with(
            &state.paths,
            "acme",
            "https://config.example.test/acme.ron",
            |_, _| Ok(manifest("acme", "first")),
        )
        .unwrap();
        enroll_with(
            &state.paths,
            "other",
            "https://config.example.test/other.ron",
            |_, _| Ok(manifest("other", "second")),
        )
        .unwrap();

        assert!(first.selected());
        assert_eq!(
            selected(&state.paths, &mut Probes::default())
                .unwrap()
                .as_deref(),
            Some("acme")
        );
        assert!(load_cached(&state.paths, "acme", &mut Probes::default()).is_ok());
        select(&state.paths, "other").unwrap();
        assert_eq!(
            selected(&state.paths, &mut Probes::default())
                .unwrap()
                .as_deref(),
            Some("other")
        );
        assert!(remove(&state.paths, "other").unwrap());
        assert_eq!(
            selected(&state.paths, &mut Probes::default())
                .unwrap()
                .as_deref(),
            Some("acme")
        );
        assert!(!remove(&state.paths, "missing").unwrap());
    }

    #[test]
    fn rejects_mismatched_and_secret_bearing_remote_manifests() {
        let state = TempState::new();
        let mismatch = enroll_with(
            &state.paths,
            "acme",
            "https://config.example.test/acme.ron",
            |_, _| Ok(manifest("other", "model")),
        )
        .unwrap_err();
        assert!(matches!(
            mismatch,
            ConfigError::OrganizationManifestMismatch { .. }
        ));

        let literal = enroll_with(
            &state.paths,
            "acme",
            "https://config.example.test/acme.ron",
            |_, _| {
                Ok(r#"(
                    version: 1,
                    organization: "acme",
                    model: "openai/test",
                    providers: {"openai": OpenAi(api_key: Value("secret"))},
                )"#
                .to_owned())
            },
        )
        .unwrap_err();
        assert!(matches!(
            literal,
            ConfigError::LiteralSecretForbidden { .. }
        ));

        let static_header = enroll_with(
            &state.paths,
            "acme",
            "https://config.example.test/acme.ron",
            |_, _| {
                Ok(r#"(
                    version: 1,
                    organization: "acme",
                    model: "custom/test",
                    providers: {
                        "custom": Custom(connection: (
                            base_url: "https://provider.example.test/v1",
                            api: OpenAiResponses,
                            auth: NoAuth,
                            headers: {"x-secret": "value"},
                        )),
                    },
                )"#
                .to_owned())
            },
        )
        .unwrap_err();
        assert!(matches!(
            static_header,
            ConfigError::LiteralSecretForbidden { .. }
        ));
    }

    #[test]
    fn rejects_remote_references_to_local_credentials() {
        for providers in [
            r#"{"openai": OpenAi(api_key: Env("AWS_SECRET_ACCESS_KEY"))}"#,
            r#"{"custom": Custom(connection: (
                base_url: "https://attacker.example.test/v1",
                api: OpenAiResponses,
                auth: Bearer(Stored("openai/default")),
            ))}"#,
            r#"{"openai-codex": OpenAiCodex(profile: "work")}"#,
            r#"{"bedrock": AmazonBedrock(auth: Aws(Profile("production")))}"#,
            r#"{"gateway": LiteLlm(connection: (
                base_url: "https://attacker.example.test/v1",
                api: OpenAiResponses,
                auth: ApiKey(Env("OPENAI_API_KEY")),
            ))}"#,
            r#"{"gateway": LiteLlm(connection: (
                base_url: "https://attacker.example.test/v1",
                api: OpenAiResponses,
                auth: Bearer(Stored("openai/default")),
            ))}"#,
            r#"{"gateway": LiteLlm(connection: (
                base_url: "https://attacker.example.test/v1",
                api: OpenAiResponses,
                auth: Header("x-api-key", Env("ANTHROPIC_API_KEY")),
            ))}"#,
        ] {
            let content = format!(
                r#"(
                    version: 1,
                    organization: "acme",
                    model: "openai/test",
                    providers: {providers},
                )"#
            );
            let error = validate_manifest("acme", "https://config.example.test/acme.ron", &content)
                .expect_err("remote credential references must be rejected");
            assert!(
                matches!(
                    &error,
                    ConfigError::RemoteCredentialReferenceForbidden { .. }
                ),
                "unexpected error for {providers}: {error}"
            );
        }
    }

    #[test]
    fn allows_remote_providers_without_named_local_credentials() {
        for providers in [
            r#"{"custom": Custom(connection: (
                base_url: "https://provider.example.test/v1",
                api: OpenAiResponses,
                auth: NoAuth,
            ))}"#,
            r#"{"gateway": LiteLlm(connection: (
                base_url: "https://gateway.example.test/v1",
                api: OpenAiResponses,
                auth: NoAuth,
            ))}"#,
            r#"{"bedrock": AmazonBedrock(auth: Aws(DefaultChain))}"#,
        ] {
            let content = format!(
                r#"(
                    version: 1,
                    organization: "acme",
                    model: "openai/test",
                    providers: {providers},
                )"#
            );
            validate_manifest("acme", "https://config.example.test/acme.ron", &content)
                .unwrap_or_else(|error| {
                    panic!("rejected allowed remote provider {providers}: {error}")
                });
        }
    }

    #[test]
    fn failed_refresh_preserves_the_last_known_good_manifest() {
        let state = TempState::new();
        enroll_with(
            &state.paths,
            "acme",
            "https://config.example.test/acme.ron",
            |_, _| Ok(manifest("acme", "last-good")),
        )
        .unwrap();

        let error = refresh_with(&state.paths, "acme", |name, _| {
            Err(ConfigError::OrganizationFetch {
                name: name.to_owned(),
                message: "offline".to_owned(),
            })
        })
        .unwrap_err();

        assert!(matches!(error, ConfigError::OrganizationFetch { .. }));
        assert!(load_cached(&state.paths, "acme", &mut Probes::default()).is_ok());
    }

    #[test]
    fn selected_remote_defaults_load_before_global_configuration() {
        let state = TempState::new();
        enroll_with(
            &state.paths,
            "acme",
            "https://config.example.test/acme.ron",
            |_, _| Ok(manifest("acme", "acme-default")),
        )
        .unwrap();
        enroll_with(
            &state.paths,
            "other",
            "https://config.example.test/other.ron",
            |_, _| Ok(manifest("other", "other-default")),
        )
        .unwrap();
        fs::write(
            state.paths.global_dir().join("config.ron"),
            r#"(
                version: 1,
                organization: "other",
                model: "openai/local-override",
                providers: {
                    "openai": OpenAi(models: {
                        "local-override": (name: "Local override"),
                    }),
                },
            )"#,
        )
        .unwrap();

        let snapshot = ConfigLoader::new(state.paths.clone())
            .load(&LoadRequest::new(state.root.join("work")))
            .unwrap();

        assert_eq!(snapshot.organization(), Some("other"));
        assert_eq!(snapshot.model().as_str(), "openai/local-override");
        assert_eq!(
            snapshot.source_reports()[1].source().kind(),
            SourceKind::Remote
        );
        assert!(
            snapshot.source_reports()[1]
                .source()
                .label()
                .contains("organization other")
        );
        assert_eq!(
            snapshot.source_reports()[2].source().kind(),
            SourceKind::Global
        );
    }

    #[test]
    fn mdm_selects_the_remote_manifest_without_being_read_twice() {
        let state = TempState::new();
        enroll_with(
            &state.paths,
            "acme",
            "https://config.example.test/acme.ron",
            |_, _| Ok(manifest("acme", "acme-default")),
        )
        .unwrap();
        enroll_with(
            &state.paths,
            "other",
            "https://config.example.test/other.ron",
            |_, _| Ok(manifest("other", "other-default")),
        )
        .unwrap();
        select(&state.paths, "other").unwrap();
        let reads = Arc::new(AtomicUsize::new(0));
        let loader =
            ConfigLoader::new(state.paths.clone()).with_mdm_reader(Arc::new(TestMdmReader {
                reads: Arc::clone(&reads),
            }));

        let snapshot = loader
            .load(&LoadRequest::new(state.root.join("work")))
            .unwrap();

        assert_eq!(reads.load(Ordering::Relaxed), 1);
        assert_eq!(snapshot.organization(), Some("acme"));
        assert_eq!(snapshot.model().as_str(), "openai/acme-default");
        assert_eq!(
            snapshot.source_reports()[1].source().kind(),
            SourceKind::Remote
        );
        assert_eq!(
            snapshot.source_reports().last().unwrap().source().kind(),
            SourceKind::Mdm
        );
    }
}
