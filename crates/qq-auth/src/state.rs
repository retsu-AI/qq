//! On-disk credential index and plaintext fallback formats, and the hardened
//! file operations every credential-state write goes through.

use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::MutexGuard,
};

#[cfg(not(windows))]
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use super::{
    AuthError, CredentialBackend, CredentialMetadata, MAX_STATE_BYTES, normalize_endpoint,
    normalize_kind, validate_credential_name,
};

pub(crate) const INDEX_STATE_VERSION: u32 = 2;
const LEGACY_INDEX_STATE_VERSION: u32 = 1;
const FALLBACK_STATE_VERSION: u32 = 1;

#[cfg(not(windows))]
static TEMPORARY_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

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
pub(crate) struct CredentialRecord {
    pub(crate) name: String,
    pub(crate) backend: CredentialBackend,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) endpoint: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IndexState {
    pub(crate) version: u32,
    /// Advanced by every durable index write. Indexes written before the
    /// field existed load as revision zero, so the first mutation after an
    /// upgrade still produces a strictly newer epoch.
    #[serde(default)]
    pub(crate) revision: u64,
    pub(crate) records: Vec<CredentialRecord>,
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
    pub(crate) fn validate(&self, path: &Path) -> Result<(), AuthError> {
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

    pub(crate) fn find(&self, name: &str) -> Option<&CredentialRecord> {
        self.records.iter().find(|record| record.name == name)
    }

    pub(crate) fn upsert(&mut self, record: CredentialRecord) {
        self.remove(&record.name);
        self.records.push(record);
        self.sort();
    }

    pub(crate) fn remove(&mut self, name: &str) -> bool {
        let original_len = self.records.len();
        self.records.retain(|record| record.name != name);
        original_len != self.records.len()
    }

    pub(crate) fn sort(&mut self) {
        self.records
            .sort_by(|left, right| left.name.cmp(&right.name));
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FallbackRecord {
    pub(crate) name: String,
    pub(crate) secret: Vec<u8>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FallbackState {
    pub(crate) version: u32,
    pub(crate) records: Vec<FallbackRecord>,
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
    pub(crate) fn validate(&self, path: &Path) -> Result<(), AuthError> {
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

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.records.iter().any(|record| record.name == name)
    }

    pub(crate) fn get(&self, name: &str) -> Option<Vec<u8>> {
        self.records
            .iter()
            .find(|record| record.name == name)
            .map(|record| record.secret.clone())
    }

    pub(crate) fn upsert(&mut self, name: &str, secret: &[u8]) {
        self.remove(name);
        self.records.push(FallbackRecord {
            name: name.to_owned(),
            secret: secret.to_vec(),
        });
        self.sort();
    }

    pub(crate) fn remove(&mut self, name: &str) -> bool {
        let original_len = self.records.len();
        self.records.retain(|record| record.name != name);
        original_len != self.records.len()
    }

    pub(crate) fn sort(&mut self) {
        self.records
            .sort_by(|left, right| left.name.cmp(&right.name));
    }
}

pub(crate) fn serialize_state<T: Serialize>(state: &T, path: &Path) -> Result<Vec<u8>, AuthError> {
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

pub(crate) fn ensure_data_directory(path: &Path) -> Result<(), AuthError> {
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

pub(crate) fn validate_directory_metadata(
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<(), AuthError> {
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

pub(crate) fn open_lock_file(path: &Path) -> Result<File, AuthError> {
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

pub(crate) fn read_state_file(path: &Path) -> Result<Option<Vec<u8>>, AuthError> {
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

pub(crate) fn validate_existing_state_file(path: &Path) -> Result<(), AuthError> {
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

pub(crate) fn validate_regular_metadata(
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<(), AuthError> {
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
#[cfg(unix)]
pub(crate) fn verify_open_directory(path: &Path, file: &File) -> Result<fs::Metadata, AuthError> {
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

pub(crate) fn verify_open_regular_file(
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
pub(crate) fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
pub(crate) fn same_file(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    true
}

pub(crate) fn set_private_file_permissions(path: &Path, file: &File) -> Result<(), AuthError> {
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
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), AuthError> {
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
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), AuthError> {
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

pub(crate) fn ensure_atomic_replacement_supported(path: &Path) -> Result<(), AuthError> {
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

#[cfg(not(windows))]
pub(crate) fn temporary_path(parent: &Path, destination: &Path) -> PathBuf {
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

pub(crate) struct StateLock {
    pub(crate) _file: File,
}

#[derive(Clone, Copy)]
pub(crate) enum LockMode {
    Exclusive,
    Shared,
}

pub(crate) struct OAuthOperationLock<'a> {
    pub(crate) _process: MutexGuard<'a, ()>,
    pub(crate) _file: File,
}
