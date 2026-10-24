//! Selected Apple compiler and SDK inputs, without project environment overrides.
use std::{
    io,
    path::{Path, PathBuf},
};

#[cfg(target_os = "macos")]
pub(super) fn discover(control: &Path) -> io::Result<super::macos_sdk::Inputs> {
    if !control.is_absolute() || control.canonicalize()? != control {
        return Err(denied());
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let command = |image: &str, args: &[&str], developer: Option<&Path>| {
        let mut command = std::process::Command::new(image);
        command
            .args(args)
            .current_dir(control)
            .env_clear()
            .env("PATH", "/usr/bin:/bin");
        if let Some(developer) = developer {
            command.env("DEVELOPER_DIR", developer);
        }
        command
    };
    let Some(selected) =
        super::unix_sdk::query_until(command("/usr/bin/xcode-select", &["-p"], None), deadline)?
    else {
        return Ok(Default::default());
    };
    let developer = validate_developer(
        &path(&selected)?,
        Path::new("/Applications"),
        Path::new("/Library/Developer/CommandLineTools"),
        control,
    )?;
    let Some(clang) = super::unix_sdk::query_until(
        command("/usr/bin/xcrun", &["--find", "clang"], Some(&developer)),
        deadline,
    )?
    else {
        return Ok(Default::default());
    };
    let Some(sdk) = super::unix_sdk::query_until(
        command(
            "/usr/bin/xcrun",
            &["--sdk", "macosx", "--show-sdk-path"],
            Some(&developer),
        ),
        deadline,
    )?
    else {
        return Ok(Default::default());
    };
    roots(&developer, &path(&clang)?, &path(&sdk)?, control)
}

#[cfg(target_os = "macos")]
fn path(bytes: &[u8]) -> io::Result<PathBuf> {
    let text = std::str::from_utf8(bytes).map_err(|_| denied())?.trim_end();
    if text.is_empty() || text.chars().any(char::is_control) {
        return Err(denied());
    }
    Ok(PathBuf::from(text))
}

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "selected Apple compiler or SDK escapes its developer installation",
    )
}

fn validate_developer(
    selected: &Path,
    applications: &Path,
    command_line_tools: &Path,
    control: &Path,
) -> io::Result<PathBuf> {
    if !selected.is_absolute() || !control.is_absolute() || control.canonicalize()? != control {
        return Err(denied());
    }
    let developer = selected.canonicalize()?;
    let conventional = if developer == command_line_tools {
        true
    } else {
        developer
            .strip_prefix(applications)
            .ok()
            .is_some_and(|suffix| {
                let parts = suffix.iter().collect::<Vec<_>>();
                parts.len() == 3
                    && parts[1] == "Contents"
                    && parts[2] == "Developer"
                    && parts[0]
                        .to_str()
                        .is_some_and(|name| name.starts_with("Xcode") && name.ends_with(".app"))
            })
    };
    if !conventional
        || developer.starts_with(control)
        || control.starts_with(&developer)
        || !developer.is_dir()
    {
        return Err(denied());
    }
    Ok(developer)
}

fn roots(
    developer: &Path,
    clang: &Path,
    sdk: &Path,
    control: &Path,
) -> io::Result<super::macos_sdk::Inputs> {
    if !clang.is_absolute() || !sdk.is_absolute() {
        return Err(denied());
    }
    let clang = clang.canonicalize()?;
    let sdk = sdk.canonicalize()?;
    let bin = clang.parent().ok_or_else(denied)?.to_owned();
    let lib = bin
        .parent()
        .ok_or_else(denied)?
        .join("lib")
        .canonicalize()?;
    if !clang.is_file()
        || !sdk.is_dir()
        || !lib.is_dir()
        || [&bin, &lib, &sdk].iter().any(|root| {
            !root.starts_with(developer) || root.starts_with(control) || control.starts_with(root)
        })
    {
        return Err(denied());
    }
    Ok(super::macos_sdk::Inputs {
        executable_directories: vec![bin.clone()],
        read_only: vec![bin, lib, sdk],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_apple_inputs_exclude_the_rest_of_the_installation() {
        let fixture = tempfile::tempdir().unwrap();
        let base = fixture.path().canonicalize().unwrap();
        let applications = base.join("Applications");
        let developer = applications.join("Xcode_26.app/Contents/Developer");
        let bin = developer.join("Toolchains/XcodeDefault.xctoolchain/usr/bin");
        let sdk = developer.join("Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk");
        let control = base.join("control");
        for directory in [&bin, &bin.parent().unwrap().join("lib"), &sdk, &control] {
            std::fs::create_dir_all(directory).unwrap();
        }
        let clang = bin.join("clang");
        std::fs::write(&clang, b"compiler").unwrap();
        let selected = validate_developer(
            &developer,
            &applications,
            &base.join("CommandLineTools"),
            &control,
        )
        .unwrap();
        let inputs = roots(&selected, &clang, &sdk, &control).unwrap();
        assert_eq!(
            inputs.executable_directories.as_slice(),
            std::slice::from_ref(&bin)
        );
        assert_eq!(inputs.read_only.len(), 3);
        assert!(!inputs.read_only.contains(&developer));
        assert!(!inputs.read_only.contains(&applications));
        assert!(
            validate_developer(
                Path::new("relative"),
                &applications,
                &base.join("CommandLineTools"),
                &control
            )
            .is_err()
        );
        assert!(
            validate_developer(
                &developer,
                &applications,
                &base.join("CommandLineTools"),
                &developer
            )
            .is_err()
        );
        let private = base.join("private");
        std::fs::create_dir(&private).unwrap();
        std::fs::write(private.join("credential"), b"private").unwrap();
        assert!(
            validate_developer(
                &private,
                &applications,
                &base.join("CommandLineTools"),
                &control
            )
            .is_err()
        );
        let escaped_developer = applications.join("Xcode_escape.app/Contents/Developer");
        std::fs::create_dir_all(escaped_developer.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&private, &escaped_developer).unwrap();
        assert!(
            validate_developer(
                &escaped_developer,
                &applications,
                &base.join("CommandLineTools"),
                &control
            )
            .is_err()
        );
        let escaped_sdk = developer.join("escaped-sdk");
        std::os::unix::fs::symlink(&private, &escaped_sdk).unwrap();
        assert_eq!(
            roots(&selected, &clang, &escaped_sdk, &control)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        let escaped_clang = bin.join("escaped-clang");
        std::os::unix::fs::symlink(private.join("credential"), &escaped_clang).unwrap();
        assert!(roots(&selected, &escaped_clang, &sdk, &control).is_err());
        assert_eq!(
            std::fs::read(private.join("credential")).unwrap(),
            b"private"
        );
    }
}
