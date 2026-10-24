//! Resolve a conventionally installed server Rustup default, outside project state.
use std::{
    io::{self, Read as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

#[cfg(windows)]
pub(super) fn discover(control: &Path, fallback: Vec<PathBuf>) -> io::Result<Vec<PathBuf>> {
    let Some(home) = std::env::var_os("USERPROFILE").map(PathBuf::from) else {
        return Ok(fallback);
    };
    let cargo = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cargo"));
    let rustup = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".rustup"));
    let system = std::env::var_os("SYSTEMROOT")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("Windows system directory unavailable"))?;
    discover_with(control, &home, &cargo, &rustup, &system, fallback, query)
}

fn discover_with(
    control: &Path,
    home: &Path,
    cargo: &Path,
    rustup: &Path,
    system: &Path,
    fallback: Vec<PathBuf>,
    mut query: impl FnMut(Command) -> io::Result<Option<Vec<u8>>>,
) -> io::Result<Vec<PathBuf>> {
    if [control, home, cargo, rustup, system]
        .iter()
        .any(|path| !path.is_absolute())
    {
        return Err(denied());
    }
    // Custom Cargo homes are cache data, never automatic host executable authority.
    let home = home.canonicalize()?;
    let conventional = home.join(".cargo");
    let image = conventional.join("bin").join("rustup.exe");
    if !image.is_file() || !rustup.is_dir() {
        return Ok(fallback);
    }
    let rustup = rustup.canonicalize()?;
    let image = image.canonicalize()?;
    if image != conventional.join("bin").join("rustup.exe")
        || image.starts_with(control)
        || conventional.starts_with(control)
        || rustup.starts_with(control)
    {
        return Err(denied());
    }
    let command = |args: &[&str]| {
        let mut command = Command::new(&image);
        command
            .args(args)
            .current_dir(control)
            .env_clear()
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("CARGO_HOME", cargo)
            .env("RUSTUP_HOME", &rustup)
            .env("RUSTUP_AUTO_INSTALL", "0")
            .env("SYSTEMROOT", system)
            .env("PATH", system.join("System32"))
            .env("TEMP", control)
            .env("TMP", control);
        command
    };
    let Some(default) = query(command(&["default"]))? else {
        return Ok(fallback);
    };
    let default = std::str::from_utf8(&default)
        .ok()
        .and_then(|text| text.split_whitespace().next())
        .filter(|name| {
            !name.is_empty()
                && name.len() <= 255
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_+.".contains(&b))
        })
        .ok_or_else(|| io::Error::other("invalid installed default toolchain metadata"))?;
    let Some(bytes) = query(command(&["which", "--toolchain", default, "cargo"]))? else {
        return Ok(fallback);
    };
    let cargo_path = std::str::from_utf8(&bytes)
        .map_err(|_| io::Error::other("installed toolchain path is not UTF-8"))?;
    let bin = selected_bin(&rustup, Path::new(cargo_path.trim_end()), control)?;
    let mut bins = vec![bin];
    for path in fallback {
        if !bins.contains(&path) && !path.canonicalize().is_ok_and(|path| bins.contains(&path)) {
            bins.push(path);
        }
    }
    Ok(bins)
}

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "installed Rust toolchain escapes its admitted root",
    )
}

fn selected_bin(home: &Path, cargo: &Path, control: &Path) -> io::Result<PathBuf> {
    if !cargo.is_absolute() || cargo.file_name() != Some(std::ffi::OsStr::new("cargo.exe")) {
        return Err(denied());
    }
    let toolchain = cargo.parent().and_then(Path::parent).ok_or_else(denied)?;
    let name = toolchain.file_name().ok_or_else(denied)?;
    let expected = home.join("toolchains").join(name);
    if toolchain.canonicalize()? != expected
        || expected.starts_with(control)
        || control.starts_with(&expected)
        || home.join("toolchains").canonicalize()? != home.join("toolchains")
        || expected.canonicalize()? != expected
    {
        return Err(denied());
    }
    for part in ["bin", "lib"] {
        let path = expected.join(part);
        if path.canonicalize()? != path {
            return Err(denied());
        }
    }
    for name in ["cargo.exe", "rustc.exe"] {
        let path = expected.join("bin").join(name);
        if !path.is_file() || path.canonicalize()? != path {
            return Err(denied());
        }
    }
    if cargo.canonicalize()? != expected.join("bin").join("cargo.exe") {
        return Err(denied());
    }
    Ok(expected.join("bin"))
}

fn query(mut command: Command) -> io::Result<Option<Vec<u8>>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let outcome = (|| {
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("rustup stdout missing"))?;
        let (send, receive) = mpsc::sync_channel(1);
        let _keepalive = send.clone();
        let deadline = Instant::now() + Duration::from_secs(5);
        std::thread::Builder::new()
            .name("rustup-metadata".into())
            .spawn(move || {
                let mut bytes = Vec::new();
                let result = stdout.take(8193).read_to_end(&mut bytes).map(|_| bytes);
                let _ = send.send(result);
            })?;
        let bytes = receive
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "rustup output timed out"))??;
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(if status.success() && bytes.len() <= 8192 {
                    Some(bytes)
                } else {
                    None
                });
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "rustup query timed out",
                ));
            }
            let _ = receive.recv_timeout(remaining.min(Duration::from_millis(10)));
        }
    })();
    if let Err(error) = outcome {
        let killed = child.kill();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return Err(error),
                Ok(None) if Instant::now() < deadline => std::thread::yield_now(),
                state => {
                    return Err(io::Error::other(format!(
                        "{error}; metadata cleanup unconfirmed: kill={killed:?}, wait={state:?}"
                    )));
                }
            }
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "qq-windows-sdk-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
        fn installed(&self) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
            let cargo = self.0.join(".cargo");
            let rustup = self.0.join("rustup home");
            let bin = rustup.join("toolchains").join("stable-test").join("bin");
            let control = self.0.join("control");
            for path in [
                cargo.join("bin"),
                bin.clone(),
                bin.parent().unwrap().join("lib"),
                control.clone(),
            ] {
                std::fs::create_dir_all(path).unwrap();
            }
            std::fs::write(
                cargo.join("bin").join("rustup.exe"),
                b"trusted metadata image",
            )
            .unwrap();
            for name in ["cargo.exe", "rustc.exe"] {
                std::fs::write(bin.join(name), b"installed image").unwrap();
            }
            (cargo, rustup, bin, control)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn proxy_only_installation_resolves_default_without_project_overrides() {
        let fixture = Fixture::new();
        let (cargo, rustup, bin, control) = fixture.installed();
        std::fs::write(
            fixture.0.join("rust-toolchain.toml"),
            "untrusted project override",
        )
        .unwrap();
        let mut calls = Vec::new();
        let roots = discover_with(
            &control,
            &fixture.0,
            &cargo,
            &rustup,
            &fixture.0.join("Windows"),
            Vec::new(),
            |command| {
                assert_eq!(command.get_program(), cargo.join("bin").join("rustup.exe"));
                assert_eq!(command.get_current_dir(), Some(control.as_path()));
                let args = command
                    .get_args()
                    .map(|arg| arg.to_str().unwrap().to_owned())
                    .collect::<Vec<_>>();
                let env = command
                    .get_envs()
                    .map(|(key, value)| (key.to_str().unwrap(), value.unwrap().to_str().unwrap()))
                    .collect::<std::collections::BTreeMap<_, _>>();
                assert_eq!(env.get("RUSTUP_AUTO_INSTALL"), Some(&"0"));
                assert!(!env.contains_key("RUSTUP_TOOLCHAIN"));
                assert!(!env.contains_key("RUSTUP_OVERRIDE_TOOLCHAIN"));
                calls.push(args.clone());
                Ok(Some(if args == ["default"] {
                    b"stable-test (default)\n".to_vec()
                } else {
                    assert_eq!(args, ["which", "--toolchain", "stable-test", "cargo"]);
                    format!("{}\n", bin.join("cargo.exe").display()).into_bytes()
                }))
            },
        )
        .unwrap();
        assert_eq!(
            roots,
            [bin],
            "proxy-only installed Rustup must resolve the server default"
        );
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn relative_metadata_inputs_never_execute_project_images() {
        let fixture = Fixture::new();
        let (cargo, rustup, _, control) = fixture.installed();
        let system = fixture.0.join("Windows");
        for index in 0..5 {
            let mut paths = [
                control.clone(),
                fixture.0.clone(),
                cargo.clone(),
                rustup.clone(),
                system.clone(),
            ];
            paths[index] = PathBuf::from("project-relative");
            let result = discover_with(
                &paths[0],
                &paths[1],
                &paths[2],
                &paths[3],
                &paths[4],
                Vec::new(),
                |_| panic!("relative metadata authority must never run a host query"),
            );
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        }
    }

    #[test]
    fn custom_cargo_home_never_supplies_a_host_executable() {
        let fixture = Fixture::new();
        let (conventional, rustup, _, control) = fixture.installed();
        std::fs::remove_file(conventional.join("bin").join("rustup.exe")).unwrap();
        let custom = fixture.0.join("project-cache");
        std::fs::create_dir_all(custom.join("bin")).unwrap();
        std::fs::write(custom.join("bin").join("rustup.exe"), b"untrusted image").unwrap();
        let roots = discover_with(
            &control,
            &fixture.0,
            &custom,
            &rustup,
            &fixture.0.join("Windows"),
            Vec::new(),
            |_| panic!("custom Cargo cache must never supply a host query executable"),
        )
        .unwrap();
        assert!(roots.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn redirected_conventional_cargo_never_executes_a_host_query() {
        let fixture = Fixture::new();
        let (conventional, rustup, _, control) = fixture.installed();
        let redirected = fixture.0.join("project-cargo");
        std::fs::rename(&conventional, &redirected).unwrap();
        std::os::unix::fs::symlink(&redirected, &conventional).unwrap();
        let result = discover_with(
            &control,
            &fixture.0,
            &conventional,
            &rustup,
            &fixture.0.join("Windows"),
            Vec::new(),
            |_| panic!("redirected conventional Cargo directory must never supply a host image"),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn installed_roots_refuse_control_overlap_and_directory_aliases() {
        let fixture = Fixture::new();
        let (_, rustup, bin, control) = fixture.installed();
        let cargo = bin.join("cargo.exe");
        assert_eq!(selected_bin(&rustup, &cargo, &control).unwrap(), bin);
        assert!(selected_bin(&rustup, &cargo, bin.parent().unwrap()).is_err());
        #[cfg(unix)]
        {
            let lib = bin.parent().unwrap().join("lib");
            std::fs::remove_dir(&lib).unwrap();
            std::os::unix::fs::symlink(&control, &lib).unwrap();
            assert!(selected_bin(&rustup, &cargo, &control).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn metadata_queries_bound_output_and_require_success() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf installed"]);
        assert_eq!(query(command).unwrap(), Some(b"installed".to_vec()));
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf ignored; exit 1"]);
        assert_eq!(query(command).unwrap(), None);
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf '%8193s' x"]);
        assert_eq!(query(command).unwrap(), None);
    }
}
