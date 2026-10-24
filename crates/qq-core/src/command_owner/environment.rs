//! Captured invocation values. Persisted request identities contain only a digest.
use super::CommandOwnerError;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;

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

    /// Installed Rust toolchains explicitly present on the server's PATH.
    /// Tasks invoke their real binaries rather than the global rustup proxies,
    /// whose configuration and hardlinked images stay outside task authority.
    #[cfg(windows)]
    pub fn windows_toolchain_bins() -> Vec<std::path::PathBuf> {
        let home = std::env::var_os("RUSTUP_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("USERPROFILE")
                    .map(|home| std::path::PathBuf::from(home).join(".rustup"))
            });
        match (std::env::var_os("PATH"), home) {
            (Some(path), Some(home)) => toolchain_bins_from(&path, &home),
            _ => Vec::new(),
        }
    }

    /// Build a bounded snapshot from explicit native values. Debug output omits values.
    pub fn from_values(
        values: impl IntoIterator<Item = (String, OsString)>,
    ) -> Result<Self, CommandOwnerError> {
        let mut entries = Vec::new();
        for (name, value) in values {
            if entries.len() >= crate::BASE_ENV.len() + crate::MAX_SHELL_ENV_NAMES {
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

    pub(crate) fn with_task_home(
        mut self,
        home: &std::path::Path,
    ) -> Result<Self, CommandOwnerError> {
        let mut overrides = vec![
            ("HOME".to_owned(), home.as_os_str().to_owned()),
            ("TMPDIR".to_owned(), home.join("tmp").into_os_string()),
        ];
        // Cargo otherwise resolves its default from the private HOME/profile.
        // Replace an explicitly admitted override without consuming a new slot.
        if self.get("CARGO_HOME").is_some() {
            overrides.push((
                "CARGO_HOME".to_owned(),
                home.join(".cargo").into_os_string(),
            ));
        }
        #[cfg(windows)]
        let overrides = {
            let mut values = overrides;
            values.extend([
                ("USERPROFILE".to_owned(), home.as_os_str().to_owned()),
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
            // Ordinary Windows tools append slash-separated names to these
            // values. Keep verbatim syntax only when DOS paths change identity.
            for (_, value) in &mut values {
                *value = dunce::simplified(std::path::Path::new(value))
                    .as_os_str()
                    .to_owned();
            }
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

    /// Prefix only executable directories supplied by the admitted native scope.
    pub(crate) fn with_executable_directories(
        mut self,
        directories: &[std::path::PathBuf],
    ) -> Result<Self, CommandOwnerError> {
        if directories.is_empty() {
            return Ok(self);
        }
        if directories.len() > 16 {
            return Err(CommandOwnerError::Invalid);
        }
        let old = self.get("PATH").unwrap_or_default();
        // Command processors and SDK batch scripts need ordinary DOS paths
        // when that spelling preserves identity. Scope grants stay canonical.
        #[cfg(windows)]
        let directories = directories
            .iter()
            .map(|path| dunce::simplified(path).to_owned());
        #[cfg(not(windows))]
        let directories = directories.iter().cloned();
        let path = std::env::join_paths(directories.chain(std::env::split_paths(&old)))
            .map_err(|_| CommandOwnerError::Invalid)?;
        self.0.retain(|(name, _)| !same_name(name, "PATH"));
        self.0
            .extend(Self::from_values([("PATH".to_owned(), path)])?.0);
        self.normalize()?;
        Ok(self)
    }

    pub(super) fn normalize(&mut self) -> Result<(), CommandOwnerError> {
        if self.0.len() > crate::BASE_ENV.len() + crate::MAX_SHELL_ENV_NAMES {
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

#[cfg(windows)]
fn toolchain_bins_from(path: &std::ffi::OsStr, home: &std::path::Path) -> Vec<std::path::PathBuf> {
    use std::path::Component;
    if !home.is_absolute() {
        return Vec::new();
    }
    let mut bins = Vec::new();
    for candidate in std::env::split_paths(path) {
        let Ok(relative) = candidate.strip_prefix(home) else {
            continue;
        };
        let components = relative.components().collect::<Vec<_>>();
        if matches!(components.as_slice(),
            [Component::Normal(tools), Component::Normal(_), Component::Normal(bin)]
            if tools.eq_ignore_ascii_case("toolchains") && bin.eq_ignore_ascii_case("bin"))
            && !bins.contains(&candidate)
        {
            bins.push(candidate);
            if bins.len() == 16 {
                break;
            }
        }
    }
    bins
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
    fn admitted_tool_search_paths_precede_captured_path_without_host_mutation() {
        let old = std::env::join_paths([std::path::PathBuf::from("old-bin")]).unwrap();
        let environment = NativeEnvironment::from_values([("PATH".into(), old)]).unwrap();
        let prefixed = environment
            .with_executable_directories(&["sdk-bin".into()])
            .unwrap();
        assert_eq!(
            std::env::split_paths(&prefixed.get("PATH").unwrap()).collect::<Vec<_>>(),
            [std::path::PathBuf::from("sdk-bin"), "old-bin".into()]
        );
        assert!(
            NativeEnvironment::from_values([])
                .unwrap()
                .with_executable_directories(&vec!["bin".into(); 17])
                .is_err()
        );
    }

    #[cfg(windows)]
    #[test]
    fn admitted_tool_search_paths_use_command_compatible_windows_paths() {
        let root = tempfile::tempdir().unwrap();
        let sdk = root.path().join("SDK with spaces-λ");
        std::fs::create_dir(&sdk).unwrap();
        let canonical = sdk.canonicalize().unwrap();
        let environment = NativeEnvironment::from_values([])
            .unwrap()
            .with_executable_directories(std::slice::from_ref(&canonical))
            .unwrap();
        let first = std::env::split_paths(&environment.get("PATH").unwrap())
            .next()
            .unwrap();
        assert!(
            !first.as_os_str().to_string_lossy().starts_with(r"\\?\"),
            "Windows tool PATH must use DOS spelling when identity allows it"
        );
        assert_eq!(first.canonicalize().unwrap(), canonical);
        let extended = root.path().canonicalize().unwrap().join("SDK trailing.");
        std::fs::create_dir(&extended).unwrap();
        let environment = NativeEnvironment::from_values([])
            .unwrap()
            .with_executable_directories(std::slice::from_ref(&extended))
            .unwrap();
        assert_eq!(
            std::env::split_paths(&environment.get("PATH").unwrap())
                .next()
                .unwrap(),
            extended,
            "DOS-incompatible names must keep their original identity"
        );
    }

    #[cfg(windows)]
    #[test]
    fn toolchain_inputs_never_include_proxy_home_or_arbitrary_path_parents() {
        use std::path::Path;
        let home = Path::new(r"C:\Users\operator\.rustup");
        let admitted = home.join(r"toolchains\stable-host\bin");
        let path = std::env::join_paths([
            Path::new(r"C:\Users\operator\.cargo\bin"),
            Path::new(r"C:\Users\operator"),
            home,
            &home.join(r"toolchains\stable-host\lib"),
            &home.join(r"toolchains\stable-host\bin\private"),
            &home.join(r"toolchains\..\bin"),
            &admitted,
            &admitted,
        ])
        .unwrap();
        assert_eq!(toolchain_bins_from(&path, home), [admitted]);
        assert!(toolchain_bins_from(&path, Path::new("relative-home")).is_empty());
        let many = std::env::join_paths(
            (0..20).map(|index| home.join(format!(r"toolchains\version-{index}\bin"))),
        )
        .unwrap();
        let bounded = toolchain_bins_from(&many, home);
        assert_eq!(bounded.len(), 16);
        assert_eq!(bounded[0], home.join(r"toolchains\version-0\bin"));
        assert_eq!(bounded[15], home.join(r"toolchains\version-15\bin"));
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
        let capacity = crate::BASE_ENV.len() + crate::MAX_SHELL_ENV_NAMES;
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
    fn task_home_replaces_host_profile_and_scratch_values() {
        let snapshot = NativeEnvironment::from_values(
            crate::BASE_ENV
                .iter()
                .map(|name| ((*name).to_owned(), OsString::from("host-value")))
                .chain([(
                    "CARGO_HOME".to_owned(),
                    OsString::from("private-host-cargo"),
                )]),
        )
        .unwrap();
        let home = std::path::Path::new("private-task-home");
        let environment = snapshot.with_task_home(home).unwrap();
        assert_eq!(environment.get("HOME"), Some(home.into()));
        assert_eq!(
            environment.get("CARGO_HOME"),
            Some(home.join(".cargo").into())
        );
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

    #[cfg(windows)]
    #[test]
    fn windows_task_home_supports_shell_profile_paths_and_called_batches() {
        use std::{fs, os::windows::process::CommandExt as _, path::Path, process::Command};

        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("task space 日本語");
        let home = workspace.join("profile space 日本語");
        for directory in ["tmp", "AppData/Local", "AppData/Roaming"] {
            fs::create_dir_all(home.join(directory)).unwrap();
        }
        let canonical = home.canonicalize().unwrap();
        assert!(matches!(
            canonical.components().next(),
            Some(std::path::Component::Prefix(prefix))
                if matches!(prefix.kind(), std::path::Prefix::VerbatimDisk(_))
        ));
        assert!(matches!(
            home.components().next(),
            Some(std::path::Component::Prefix(prefix))
                if matches!(prefix.kind(), std::path::Prefix::Disk(_))
        ));
        let variables = [
            ("HOME", ""),
            ("USERPROFILE", ""),
            ("TMPDIR", "tmp"),
            ("TEMP", "tmp"),
            ("TMP", "tmp"),
            ("APPDATA", "AppData/Roaming"),
            ("LOCALAPPDATA", "AppData/Local"),
        ];
        let exercise = |label: &str, task_home: &Path, prefix: &str| {
            let system = std::env::var_os("SYSTEMROOT").expect("Windows system root");
            let environment = NativeEnvironment::from_values([
                ("SYSTEMROOT".to_owned(), system.clone()),
                (
                    "PATH".to_owned(),
                    Path::new(&system).join("System32").into_os_string(),
                ),
            ])
            .unwrap()
            .with_task_home(task_home)
            .unwrap();
            let mut script = prefix.to_owned();
            for (name, _) in variables {
                script.push_str(&format!(
                    ">\"%{name}%/proof-{name}-{label}\" echo private-{name} & "
                ));
            }
            fs::write(
                workspace.join(format!("owned {label}.cmd")),
                format!("@echo off\r\n>called-{label}.txt echo called-batch\r\nexit /b 0\r\n"),
            )
            .unwrap();
            script.push_str(&format!("call \"owned {label}.cmd\""));
            let raw = format!("/D /S /C \"{script}\"");
            eprintln!("{label} raw cmd arguments: {raw:?}");
            let program = Path::new(&system).join("System32").join("cmd.exe");
            eprintln!("{label} cmd program: {program:?}");
            let mut command = Command::new(program);
            command.env_clear().current_dir(&workspace).raw_arg(raw);
            for (name, _) in &environment.0 {
                command.env(name, environment.get(name).unwrap());
            }
            let output = command.output().unwrap();
            let proofs = variables.map(|(name, directory)| {
                let content =
                    fs::read_to_string(home.join(directory).join(format!("proof-{name}-{label}")))
                        .map(|content| content.trim().to_owned());
                (name, content)
            });
            let called = fs::read_to_string(workspace.join(format!("called-{label}.txt")));
            eprintln!(
                "{label} profile results: {proofs:?}; called batch: {called:?}; shell: {output:?}"
            );
            output.status.success()
                && output.stderr.is_empty()
                && proofs.iter().all(|(name, result)| {
                    result
                        .as_deref()
                        .is_ok_and(|value| value == format!("private-{name}"))
                })
                && called
                    .as_deref()
                    .is_ok_and(|value| value.trim() == "called-batch")
        };
        assert!(
            exercise("dos-echo", &home, "echo fixture-control & "),
            "ordinary DOS task home must validate the echo-first shell fixture",
        );
        assert!(
            exercise("canonical", &canonical, "echo fixture-regression & "),
            "canonical task home must support ordinary shell path suffixes and called batches",
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_task_home_preserves_extended_only_names() {
        let long = format!(r"\\?\C:\{}", "a".repeat(300));
        for path in [r"\\?\C:\task.", r"\\?\C:\task ", r"\\?\C:\NUL\task", &long] {
            let home = std::path::Path::new(path);
            let environment = NativeEnvironment::from_values([])
                .unwrap()
                .with_task_home(home)
                .unwrap();
            assert_eq!(environment.get("HOME"), Some(home.into()));
            assert_eq!(environment.get("TMP"), Some(home.join("tmp").into()));
        }
        // A child path can need verbatim semantics even when HOME does not.
        let home = std::path::PathBuf::from(format!(r"\\?\C:\{}", "a".repeat(245)));
        let environment = NativeEnvironment::from_values([])
            .unwrap()
            .with_task_home(&home)
            .unwrap();
        assert_eq!(
            environment.get("APPDATA"),
            Some(home.join("AppData/Roaming").into()),
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
