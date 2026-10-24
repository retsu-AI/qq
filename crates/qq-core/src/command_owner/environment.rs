//! Captured invocation values. Persisted request identities contain only a digest.
use super::CommandOwnerError;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;

const MAX_NATIVE_ENV_NAMES: usize = 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NativeEnvironment(Vec<(String, Value)>);

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Value {
    Unix(Vec<u8>),
    Windows(Vec<u16>),
}

impl std::fmt::Debug for NativeEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeEnvironment")
            .field(
                "names",
                &self.0.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl NativeEnvironment {
    /// Preserve the caller's normal tool environment, excluding credential values
    /// unless the host explicitly permits the requested name.
    /// macOS loader overrides are not inherited automatically: they could run
    /// code in sandbox-exec before its policy takes effect.
    /// Missing names stay absent; the worker never supplies replacement values.
    pub fn capture(requested: &[String]) -> Result<Self, CommandOwnerError> {
        if requested.len() > crate::MAX_SHELL_ENV_NAMES
            || requested
                .iter()
                .any(|name| !crate::runtime::valid_env_name(name))
        {
            return Err(CommandOwnerError::Invalid);
        }
        Self::capture_values(
            std::env::vars_os().filter_map(|(name, value)| Some((name.into_string().ok()?, value))),
            requested,
        )
    }

    fn capture_values(
        values: impl IntoIterator<Item = (String, OsString)>,
        requested: &[String],
    ) -> Result<Self, CommandOwnerError> {
        Self::from_values(values.into_iter().filter(|(name, _)| {
            let loader_override =
                cfg!(target_os = "macos") && (name.starts_with("DYLD_") || name.starts_with("LD_"));
            crate::runtime::valid_env_name(name)
                && ((!credential_name(name) && !loader_override)
                    || requested.iter().any(|requested| same_name(requested, name)))
        }))
    }

    /// Build a bounded snapshot from explicit native values. Debug output omits values.
    pub fn from_values(
        values: impl IntoIterator<Item = (String, OsString)>,
    ) -> Result<Self, CommandOwnerError> {
        let mut entries = Vec::new();
        for (name, value) in values {
            if entries.len() >= MAX_NATIVE_ENV_NAMES {
                return Err(CommandOwnerError::Invalid);
            }
            #[cfg(unix)]
            let value = {
                use std::os::unix::ffi::OsStringExt as _;
                Value::Unix(value.into_vec())
            };
            #[cfg(windows)]
            let value = {
                use std::os::windows::ffi::OsStrExt as _;
                Value::Windows(value.encode_wide().collect())
            };
            #[cfg(not(any(unix, windows)))]
            {
                let _ = (name, value);
                return Err(CommandOwnerError::Invalid);
            }
            #[cfg(any(unix, windows))]
            entries.push((name, value));
        }
        let mut environment = Self(entries);
        environment.normalize()?;
        Ok(environment)
    }

    /// Dedicated trusted-helper input. Ordinary shell policy never supplies it.
    pub(crate) fn with_task_hosting_token(
        mut self,
        credential: &qq_provider::SecretLiteral,
    ) -> Result<Self, CommandOwnerError> {
        let name = crate::task_git_job::HOSTING_TOKEN_ENV;
        self.0.retain(|(saved, _)| !same_name(saved, name));
        let added =
            Self::from_values([(name.to_owned(), OsString::from(credential.expose_secret()))])?;
        self.0.extend(added.0);
        self.normalize()?;
        Ok(self)
    }

    pub(crate) fn with_task_home(
        mut self,
        home: &std::path::Path,
    ) -> Result<Self, CommandOwnerError> {
        self.0
            .retain(|(name, _)| !same_name(name, "HOME") && !same_name(name, "TMPDIR"));
        let overrides = Self::from_values([
            ("HOME".to_owned(), home.as_os_str().to_owned()),
            ("TMPDIR".to_owned(), home.join("tmp").into_os_string()),
        ])?;
        self.0.extend(overrides.0);
        self.normalize()?;
        Ok(self)
    }

    /// Keep HOME and installed-tool configuration; only scratch writes move
    /// into the task's writable directory.
    pub(crate) fn with_task_temporary(
        mut self,
        home: &std::path::Path,
    ) -> Result<Self, CommandOwnerError> {
        let names: &[&str] = if cfg!(windows) {
            &["TMPDIR", "TEMP", "TMP"]
        } else {
            &["TMPDIR"]
        };
        self.0
            .retain(|(name, _)| !names.iter().any(|key| same_name(name, key)));
        let temporary = home.join("tmp").into_os_string();
        self.0.extend(
            Self::from_values(
                names
                    .iter()
                    .map(|name| ((*name).to_owned(), temporary.clone())),
            )?
            .0,
        );
        self.normalize()?;
        Ok(self)
    }

    pub(super) fn normalize(&mut self) -> Result<(), CommandOwnerError> {
        if self.0.len() > MAX_NATIVE_ENV_NAMES {
            return Err(CommandOwnerError::Invalid);
        }
        self.0.sort_by(|a, b| a.0.cmp(&b.0));
        let mut bytes = 0usize;
        for (index, (name, value)) in self.0.iter().enumerate() {
            if !crate::runtime::valid_env_name(name)
                || self.0[..index]
                    .iter()
                    .any(|(saved, _)| same_name(saved, name))
            {
                return Err(CommandOwnerError::Invalid);
            }
            let size = match value {
                #[cfg(unix)]
                Value::Unix(bytes) if !bytes.contains(&0) => bytes.len(),
                #[cfg(windows)]
                Value::Windows(units) if !units.contains(&0) => units.len().saturating_mul(2),
                _ => return Err(CommandOwnerError::Invalid),
            };
            bytes = bytes.saturating_add(name.len()).saturating_add(size);
            if bytes > 256 * 1024 {
                return Err(CommandOwnerError::Invalid);
            }
        }
        Ok(())
    }

    pub(crate) fn get(&self, name: &str) -> Option<OsString> {
        let (_, value) = self.0.iter().find(|(saved, _)| same_name(saved, name))?;
        match value {
            #[cfg(unix)]
            Value::Unix(bytes) => {
                use std::os::unix::ffi::OsStringExt as _;
                Some(OsString::from_vec(bytes.clone()))
            }
            #[cfg(windows)]
            Value::Windows(units) => {
                use std::os::windows::ffi::OsStringExt as _;
                Some(OsString::from_wide(units))
            }
            _ => None,
        }
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(name, _)| name.as_str())
    }

    pub(super) fn fingerprint(&self) -> Result<String, CommandOwnerError> {
        use sha2::{Digest as _, Sha256};
        Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(self)?)))
    }
}

fn credential_name(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    name == "QQ_CONFIG_CONTENT"
        || ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "AUTH"]
            .iter()
            .any(|part| name.contains(part))
}

fn same_name(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_macos_capture_omits_loader_overrides_without_normalizing_explicit_values() {
        let names = [
            "DYLD_INSERT_LIBRARIES",
            "DYLD_FALLBACK_LIBRARY_PATH",
            "LD_LIBRARY_PATH",
        ];
        let inherited = NativeEnvironment::capture_values(
            names.map(|name| (name.to_owned(), "loader-canary".into())),
            &[],
        )
        .unwrap();
        for name in names {
            if cfg!(target_os = "macos") {
                assert_eq!(inherited.get(name), None);
            } else {
                assert_eq!(inherited.get(name), Some("loader-canary".into()));
            }
            // Explicit values still reach the launcher's existing safety check;
            // capture must not silently rewrite an explicitly submitted command.
            let explicit = NativeEnvironment::capture_values(
                [(name.to_owned(), "loader-canary".into())],
                &[name.to_owned()],
            )
            .unwrap();
            assert_eq!(explicit.get(name), Some("loader-canary".into()));
            let frozen =
                NativeEnvironment::from_values([(name.to_owned(), "loader-canary".into())])
                    .unwrap();
            assert_eq!(frozen.get(name), Some("loader-canary".into()));
        }
    }

    #[test]
    fn normal_tool_environment_does_not_require_a_tool_catalog() {
        let values = [
            ("HOME", "host-home"),
            ("UNLISTED_COMPILER_HOME", "installed-tool"),
            ("FUTURE_BUILD_CONFIG", "custom-config"),
            ("PROVIDER_API_TOKEN", "credential-canary"),
            ("QQ_CONFIG_CONTENT", "configuration-canary"),
        ];
        let captured = NativeEnvironment::capture_values(
            values.map(|(name, value)| (name.to_owned(), value.into())),
            &[],
        )
        .unwrap();
        assert_eq!(
            captured.get("UNLISTED_COMPILER_HOME"),
            Some("installed-tool".into())
        );
        assert_eq!(
            captured.get("FUTURE_BUILD_CONFIG"),
            Some("custom-config".into())
        );
        assert_eq!(captured.get("PROVIDER_API_TOKEN"), None);
        assert_eq!(captured.get("QQ_CONFIG_CONTENT"), None);
        let home = tempfile::tempdir().unwrap();
        let captured = captured.with_task_temporary(home.path()).unwrap();
        assert_eq!(captured.get("HOME"), Some("host-home".into()));
        assert_eq!(
            captured.get("TMPDIR"),
            Some(home.path().join("tmp").into_os_string())
        );
        let explicit = NativeEnvironment::capture_values(
            [("PROVIDER_API_TOKEN".to_owned(), "credential-canary".into())],
            &["PROVIDER_API_TOKEN".to_owned()],
        )
        .unwrap();
        assert_eq!(
            explicit.get("PROVIDER_API_TOKEN"),
            Some("credential-canary".into())
        );
    }

    fn value(bytes: Vec<u8>) -> Value {
        if cfg!(unix) {
            Value::Unix(bytes)
        } else {
            Value::Windows(bytes.into_iter().map(u16::from).collect())
        }
    }

    #[test]
    fn invalid_wire_environments_are_rejected_before_execution() {
        let foreign = if cfg!(unix) {
            Value::Windows(Vec::new())
        } else {
            Value::Unix(Vec::new())
        };
        for entries in [
            vec![("BAD=NAME".to_owned(), value(Vec::new()))],
            vec![("A".to_owned(), value(vec![0]))],
            vec![
                ("A".to_owned(), value(Vec::new())),
                ("A".to_owned(), value(Vec::new())),
            ],
            vec![("A".to_owned(), foreign)],
            vec![("A".to_owned(), value(vec![b'x'; 256 * 1024 + 1]))],
        ] {
            let mut captured: NativeEnvironment =
                serde_json::from_slice(&serde_json::to_vec(&NativeEnvironment(entries)).unwrap())
                    .unwrap();
            assert!(matches!(
                captured.normalize(),
                Err(CommandOwnerError::Invalid)
            ));
        }
        let capacity = MAX_NATIVE_ENV_NAMES;
        assert!(
            NativeEnvironment::from_values((0..capacity).map(|n| (format!("VAR_{n}"), "x".into())))
                .is_ok()
        );
        assert!(
            NativeEnvironment::from_values(
                (0..=capacity).map(|n| (format!("VAR_{n}"), "x".into()))
            )
            .is_err()
        );
        assert!(
            NativeEnvironment::capture(&vec!["UNSET".to_owned(); crate::MAX_SHELL_ENV_NAMES + 1])
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_values_round_trip_without_utf8_conversion() {
        use std::os::unix::ffi::OsStringExt as _;
        let expected = OsString::from_vec(vec![b'x', 255]);
        let input =
            NativeEnvironment::from_values([("VALUE".to_owned(), expected.clone())]).unwrap();
        let mut decoded: NativeEnvironment =
            serde_json::from_slice(&serde_json::to_vec(&input).unwrap()).unwrap();
        decoded.normalize().unwrap();
        assert_eq!(decoded.get("VALUE"), Some(expected));
        assert_eq!(decoded.get("MISSING"), None);
    }
}
