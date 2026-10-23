//! Captured invocation values. Persisted request identities contain only a digest.
use super::CommandOwnerError;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;

#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NativeEnvironment(Vec<(String, Value)>);

// Reserve bounded slots for task-owned tool settings and private Cargo storage.
const MAX_ENTRIES: usize = crate::BASE_ENV.len() + crate::MAX_SHELL_ENV_NAMES + 4;

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
    /// Capture the base environment and explicitly requested names from the caller.
    /// Missing names stay absent; the worker never supplies replacement values.
    pub fn capture(requested: &[String]) -> Result<Self, CommandOwnerError> {
        if requested.len() > crate::MAX_SHELL_ENV_NAMES
            || requested
                .iter()
                .any(|name| !crate::runtime::valid_env_name(name))
        {
            return Err(CommandOwnerError::Invalid);
        }
        let mut values = Vec::new();
        for name in crate::BASE_ENV
            .iter()
            .copied()
            .chain(requested.iter().map(String::as_str))
        {
            if values
                .iter()
                .any(|(saved, _): &(String, OsString)| same_name(saved, name))
            {
                continue;
            }
            if let Some(value) = std::env::var_os(name) {
                values.push((name.to_owned(), value));
            }
        }
        Self::from_values(values)
    }

    /// Build a bounded snapshot from explicit native values. Debug output omits values.
    pub fn from_values(
        values: impl IntoIterator<Item = (String, OsString)>,
    ) -> Result<Self, CommandOwnerError> {
        let mut entries = Vec::new();
        for (name, value) in values {
            if entries.len() >= MAX_ENTRIES {
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

    /// Apply host-supplied tool settings before the final private-home overrides.
    pub(crate) fn with_overrides(mut self, trusted: &Self) -> Result<Self, CommandOwnerError> {
        let mut trusted = trusted.clone();
        trusted.normalize()?;
        self.0
            .retain(|(name, _)| !trusted.0.iter().any(|(other, _)| same_name(name, other)));
        self.0.extend(trusted.0);
        self.normalize()?;
        Ok(self)
    }

    pub(crate) fn with_task_home(
        mut self,
        home: &std::path::Path,
    ) -> Result<Self, CommandOwnerError> {
        let overrides = vec![
            ("HOME".to_owned(), home.as_os_str().to_owned()),
            ("TMPDIR".to_owned(), home.join("tmp").into_os_string()),
        ];
        #[cfg(windows)]
        let overrides = {
            let mut values = overrides;
            values.extend([
                ("USERPROFILE".to_owned(), home.as_os_str().to_owned()),
                (
                    "CARGO_HOME".to_owned(),
                    home.join(".cargo").into_os_string(),
                ),
                ("TEMP".to_owned(), home.join("tmp").into_os_string()),
                ("TMP".to_owned(), home.join("tmp").into_os_string()),
                (
                    "APPDATA".to_owned(),
                    home.join("AppData/Roaming").into_os_string(),
                ),
                (
                    "LOCALAPPDATA".to_owned(),
                    home.join("AppData/Local").into_os_string(),
                ),
            ]);
            values
        };
        self.0.retain(|(name, _)| {
            !overrides
                .iter()
                .any(|(replacement, _)| same_name(name, replacement))
        });
        self.0.extend(Self::from_values(overrides)?.0);
        self.normalize()?;
        Ok(self)
    }

    pub(super) fn normalize(&mut self) -> Result<(), CommandOwnerError> {
        if self.0.len() > MAX_ENTRIES {
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

    pub(super) fn fingerprint(&self) -> Result<String, CommandOwnerError> {
        use sha2::{Digest as _, Sha256};
        Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(self)?)))
    }
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
        let capacity = MAX_ENTRIES;
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

    #[test]
    fn trusted_tool_settings_survive_encoding_and_cannot_replace_the_private_home() {
        let caller = NativeEnvironment::from_values([
            ("PATH".to_owned(), "caller-code-path".into()),
            ("HOME".to_owned(), "caller-home".into()),
            ("RUSTUP_HOME".to_owned(), "caller-rustup".into()),
        ])
        .unwrap();
        let trusted = NativeEnvironment::from_values([
            ("HOME".to_owned(), "trusted-host-home".into()),
            ("RUSTUP_HOME".to_owned(), "captured-tool-inputs".into()),
        ])
        .unwrap();
        let encoded = serde_json::to_vec(&trusted).unwrap();
        let restored = serde_json::from_slice(&encoded).unwrap();
        let private = std::path::Path::new("private-task-home");
        let invocation = caller
            .with_overrides(&restored)
            .unwrap()
            .with_task_home(private)
            .unwrap();
        assert_eq!(
            invocation.get("RUSTUP_HOME"),
            Some("captured-tool-inputs".into())
        );
        assert_eq!(invocation.get("PATH"), Some("caller-code-path".into()));
        assert_eq!(invocation.get("HOME"), Some(private.as_os_str().to_owned()));
        assert!(!format!("{invocation:?}").contains("captured-tool-inputs"));
    }

    #[cfg(windows)]
    #[test]
    fn private_tool_settings_preserve_the_full_per_call_environment_allowance() {
        let environment = NativeEnvironment::from_values(
            crate::BASE_ENV
                .iter()
                .map(|name| ((*name).to_owned(), "host".into()))
                .chain(
                    (0..crate::MAX_SHELL_ENV_NAMES)
                        .map(|n| (format!("CUSTOM_{n}"), "per-call".into())),
                ),
        )
        .unwrap();
        let trusted = NativeEnvironment::from_values([
            ("RUSTUP_HOME".to_owned(), "trusted-toolchain".into()),
            ("RUSTUP_TOOLCHAIN".to_owned(), "selected-toolchain".into()),
        ])
        .unwrap();
        let private = std::path::Path::new(r"C:\private-task");
        let environment = environment
            .with_overrides(&trusted)
            .unwrap()
            .with_task_home(private)
            .unwrap();
        assert_eq!(
            environment.get("CARGO_HOME"),
            Some(private.join(".cargo").into_os_string())
        );
        assert_eq!(
            environment.get("RUSTUP_HOME"),
            Some("trusted-toolchain".into())
        );
        for n in 0..crate::MAX_SHELL_ENV_NAMES {
            assert_eq!(
                environment.get(&format!("CUSTOM_{n}")),
                Some("per-call".into())
            );
        }
    }

    #[test]
    fn task_home_replaces_host_profile_and_scratch_values() {
        let snapshot = NativeEnvironment::from_values(
            crate::BASE_ENV
                .iter()
                .map(|name| ((*name).to_owned(), OsString::from("host-value"))),
        )
        .unwrap();
        let home = std::path::Path::new("private-task-home");
        let environment = snapshot.with_task_home(home).unwrap();
        assert_eq!(environment.get("HOME"), Some(home.into()));
        assert_eq!(environment.get("TMPDIR"), Some(home.join("tmp").into()));
        assert_eq!(environment.get("PATH"), Some("host-value".into()));
        #[cfg(windows)]
        for (name, path) in [
            ("USERPROFILE", home.to_path_buf()),
            ("TEMP", home.join("tmp")),
            ("TMP", home.join("tmp")),
            ("APPDATA", home.join("AppData/Roaming")),
            ("LOCALAPPDATA", home.join("AppData/Local")),
        ] {
            assert_eq!(environment.get(name), Some(path.into_os_string()));
        }
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
