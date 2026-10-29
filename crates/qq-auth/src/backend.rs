#[cfg(windows)]
use std::{fs, io, path::PathBuf};

#[cfg(windows)]
use sha2::{Digest, Sha256};

use super::{AuthError, CredentialPaths, KEYRING_SERVICE};
#[cfg(windows)]
use super::{
    MAX_STATE_BYTES,
    state::{atomic_write, read_state_file, validate_regular_metadata},
};

#[cfg(windows)]
const WINDOWS_PROTECTED_MAGIC: &[u8] = b"QQDPAPI\x01";

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
    #[cfg(any(test, not(windows)))]
    Unavailable,
    Missing,
    Failure,
}

pub(crate) trait WindowsProtectedBackend: Send + Sync {
    fn get(&self, name: &str) -> Result<Vec<u8>, WindowsProtectedError>;
    fn set(&self, name: &str, secret: &[u8]) -> Result<(), WindowsProtectedError>;
    fn remove(&self, name: &str) -> Result<(), WindowsProtectedError>;
}

pub(crate) fn windows_protected_auth_error(
    error: WindowsProtectedError,
    operation: &'static str,
    name: &str,
) -> AuthError {
    match error {
        #[cfg(any(test, not(windows)))]
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

pub(crate) struct SystemWindowsProtected {
    #[cfg(windows)]
    paths: CredentialPaths,
}

impl SystemWindowsProtected {
    pub(crate) fn new(paths: CredentialPaths) -> Self {
        #[cfg(not(windows))]
        let _ = paths;
        Self {
            #[cfg(windows)]
            paths,
        }
    }

    #[cfg(windows)]
    pub(super) fn path(&self, name: &str) -> PathBuf {
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
pub(crate) fn windows_protected_entropy(name: &str) -> Vec<u8> {
    let mut entropy = Vec::with_capacity(KEYRING_SERVICE.len() + 1 + name.len());
    entropy.extend_from_slice(KEYRING_SERVICE.as_bytes());
    entropy.push(0);
    entropy.extend_from_slice(name.as_bytes());
    entropy
}

pub(crate) struct SystemKeyring;

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

pub(crate) fn classify_keyring_error(error: keyring::Error) -> KeyringError {
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
