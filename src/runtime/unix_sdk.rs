//! Resolve the server's installed Rustup default without project overrides.
use std::{
    io,
    path::{Path, PathBuf},
};
#[cfg(unix)]
use std::{
    io::Read,
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

#[cfg(unix)]
pub(super) fn discover(control: &Path) -> io::Result<Vec<PathBuf>> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Ok(Vec::new());
    };
    let cargo = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cargo"));
    let rustup = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".rustup"));
    // A relative server setting resolves through its launch directory, which
    // may be a project. It must not authorize a Host metadata executable.
    if !home.is_absolute() || !cargo.is_absolute() || !rustup.is_absolute() {
        return Ok(Vec::new());
    }
    if !control.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SDK metadata requires absolute private control storage",
        ));
    }
    let image = cargo.join("bin/rustup");
    if !image.is_file() {
        return Ok(Vec::new());
    }
    let cargo = cargo.canonicalize()?;
    let rustup = rustup.canonicalize()?;
    let image = image.canonicalize()?;
    if image != cargo.join("bin/rustup") || image.starts_with(control) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "server rustup image aliases a control path",
        ));
    }
    let command = |args: &[&str]| {
        let mut command = Command::new(&image);
        command
            .args(args)
            .current_dir(control)
            .env_clear()
            .env("HOME", &home)
            .env("CARGO_HOME", &cargo)
            .env("RUSTUP_HOME", &rustup)
            .env("RUSTUP_AUTO_INSTALL", "0")
            .env("PATH", "/usr/bin:/bin");
        command
    };
    let Some(default) = query(command(&["default"]))? else {
        return Ok(Vec::new());
    };
    let default = std::str::from_utf8(&default)
        .ok()
        .and_then(|text| text.split_whitespace().next())
        .filter(|name| {
            name.len() <= 255
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_+.".contains(&b))
        })
        .ok_or_else(|| io::Error::other("invalid default Rust toolchain metadata"))?;
    let Some(bytes) = query(command(&["which", "--toolchain", default, "cargo"]))? else {
        return Ok(Vec::new());
    };
    let text =
        std::str::from_utf8(&bytes).map_err(|_| io::Error::other("rustup path is not UTF-8"))?;
    roots(&rustup, Path::new(text.trim_end()), control)
}

#[cfg(unix)]
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
            .ok_or_else(|| io::Error::other("rustup metadata output missing"))?;
        let (send, receive) = mpsc::sync_channel(1);
        let keepalive = send.clone();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = stdout.take(8193).read_to_end(&mut bytes).map(|_| bytes);
            let _ = send.send(result);
        });
        let result = receive.recv_timeout(Duration::from_secs(5));
        // Query stdout closing is not process completion.
        let mut status = child.try_wait()?;
        while status.is_none() && std::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let _ = receive.recv_timeout(remaining.min(Duration::from_millis(10)));
            status = child.try_wait()?;
        }
        drop(keepalive);
        if status.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "rustup metadata query timed out",
            ));
        }
        result.map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "rustup metadata output timed out")
        })?
    })();
    // Retain ownership through every observer/read error; Child::drop alone
    // would leave an outstanding trusted metadata process.
    if outcome.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let bytes = outcome?;
    if !status.success() || bytes.len() > 8192 {
        return Ok(None);
    }
    Ok(Some(bytes))
}

fn roots(home: &Path, cargo: &Path, control: &Path) -> io::Result<Vec<PathBuf>> {
    let denied = || {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "server toolchain escapes its installed root",
        )
    };
    let toolchain = cargo.parent().and_then(Path::parent).ok_or_else(denied)?;
    let name = toolchain.file_name().ok_or_else(denied)?;
    let expected = home.join("toolchains").join(name);
    if toolchain != expected
        || !cargo.is_absolute()
        || cargo.file_name() != Some(std::ffi::OsStr::new("cargo"))
        || expected.starts_with(control)
        || control.starts_with(&expected)
        || home.join("toolchains").canonicalize()? != home.join("toolchains")
        || expected.canonicalize()? != expected
    {
        return Err(denied());
    }
    let mut roots = Vec::new();
    for part in ["bin", "lib"] {
        let path = expected.join(part);
        if path.canonicalize()? != path {
            return Err(denied());
        }
        roots.push(path);
    }
    for program in ["cargo", "rustc"] {
        let path = roots[0].join(program);
        if !path.is_file() || path.canonicalize()? != path {
            return Err(denied());
        }
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relative_server_homes_never_execute_project_rustup() {
        const CHILD: &str = "QQ_UNIX_SDK_RELATIVE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let directory = std::env::current_dir().unwrap();
            let result = discover(&directory.join("control"));
            assert!(
                !directory.join("metadata-executed").exists(),
                "relative server home executed project-controlled rustup"
            );
            assert!(result.unwrap().is_empty());
            return;
        }
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().canonicalize().unwrap();
        for path in [".cargo/bin", ".rustup", "control"] {
            std::fs::create_dir_all(home.join(path)).unwrap();
        }
        let image = home.join(".cargo/bin/rustup");
        std::fs::write(
            &image,
            "#!/bin/sh\n: > ../metadata-executed\nprintf 'stable-test (default)\\n'\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o700)).unwrap();
        for relative in ["HOME", "CARGO_HOME", "RUSTUP_HOME"] {
            let value = match relative {
                "HOME" => ".",
                "CARGO_HOME" => ".cargo",
                _ => ".rustup",
            };
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "runtime::unix_sdk::tests::relative_server_homes_never_execute_project_rustup",
                    "--nocapture",
                ])
                .current_dir(&home)
                .env_clear()
                .env("HOME", &home)
                .env("CARGO_HOME", home.join(".cargo"))
                .env("RUSTUP_HOME", home.join(".rustup"))
                .env(relative, value)
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "relative {relative} queried project metadata: {output:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn conventional_rustup_works_when_client_starts_in_home() {
        const CHILD: &str = "QQ_UNIX_SDK_HOME_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let home = PathBuf::from(std::env::var_os("HOME").unwrap());
            assert_eq!(std::env::current_dir().unwrap(), home);
            let result = discover(&home.join("control"));
            assert!(
                !home.join("custom-metadata-executed").exists(),
                "custom Cargo cache selected a Host executable"
            );
            let roots = result.unwrap();
            assert_eq!(
                roots,
                vec![
                    home.join(".rustup/toolchains/stable-test/bin"),
                    home.join(".rustup/toolchains/stable-test/lib")
                ]
            );
            return;
        }
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().canonicalize().unwrap();
        for path in [
            ".cargo/bin",
            ".rustup/toolchains/stable-test/bin",
            ".rustup/toolchains/stable-test/lib",
            "control",
        ] {
            std::fs::create_dir_all(home.join(path)).unwrap();
        }
        for name in ["cargo", "rustc"] {
            std::fs::write(
                home.join(".rustup/toolchains/stable-test/bin").join(name),
                b"native fixture",
            )
            .unwrap();
        }
        let rustup = home.join(".cargo/bin/rustup");
        std::fs::write(&rustup, "#!/bin/sh\ncase \"$1\" in\ndefault) printf '%s\\n' 'stable-test (default)';;\nwhich) printf '%s\\n' \"$RUSTUP_HOME/toolchains/stable-test/bin/cargo\";;\nesac\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&rustup, std::fs::Permissions::from_mode(0o700)).unwrap();
        let custom = home.join("project-cargo");
        std::fs::create_dir_all(custom.join("bin")).unwrap();
        let malicious = custom.join("bin/rustup");
        std::fs::write(
            &malicious,
            "#!/bin/sh\n: > ../custom-metadata-executed\nprintf 'project-controlled\\n'\n",
        )
        .unwrap();
        std::fs::set_permissions(&malicious, std::fs::Permissions::from_mode(0o700)).unwrap();
        for custom_home in [None, Some(custom)] {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
            .args([
                "--exact",
                "runtime::unix_sdk::tests::conventional_rustup_works_when_client_starts_in_home",
                "--nocapture",
            ])
            .current_dir(&home)
            .env_clear()
            .env("HOME", &home)
            .env(CHILD, "1");
            if let Some(custom) = custom_home {
                command.env("CARGO_HOME", custom);
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "child home-start resolver failed: {output:?}"
            );
        }
    }
    #[test]
    fn installed_default_roots_reject_aliases_and_control_overlap() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("rustup");
        let version = home.join("toolchains/stable");
        std::fs::create_dir_all(version.join("bin")).unwrap();
        std::fs::create_dir(version.join("lib")).unwrap();
        for name in ["cargo", "rustc"] {
            std::fs::write(version.join("bin").join(name), b"native").unwrap();
        }
        let home = home.canonicalize().unwrap();
        let version = home.join("toolchains/stable");
        let cargo = home.join("toolchains/stable/bin/cargo");
        let control = fixture.path().join("private");
        assert_eq!(
            roots(&home, &cargo, &control).unwrap(),
            vec![version.join("bin"), version.join("lib")]
        );
        assert!(roots(&home, &cargo, &version).is_err());
        std::fs::remove_dir(version.join("lib")).unwrap();
        let outside = fixture.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, version.join("lib")).unwrap();
        assert!(roots(&home, &cargo, &control).is_err());
    }
}
