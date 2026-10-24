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
    selected_inputs(
        &path(&selected)?,
        Path::new("/Applications"),
        Path::new("/Library/Developer/CommandLineTools"),
        control,
        |developer| {
            let Some(clang) = super::unix_sdk::query_until(
                command("/usr/bin/xcrun", &["--find", "clang"], Some(developer)),
                deadline,
            )?
            else {
                return Ok(None);
            };
            let Some(sdk) = super::unix_sdk::query_until(
                command(
                    "/usr/bin/xcrun",
                    &["--sdk", "macosx", "--show-sdk-path"],
                    Some(developer),
                ),
                deadline,
            )?
            else {
                return Ok(None);
            };
            Ok(Some((path(&clang)?, path(&sdk)?)))
        },
    )
}

#[cfg(target_os = "macos")]
fn path(bytes: &[u8]) -> io::Result<PathBuf> {
    let text = std::str::from_utf8(bytes).map_err(|_| denied())?.trim_end();
    if text.is_empty() || text.chars().any(char::is_control) {
        return Err(denied());
    }
    Ok(PathBuf::from(text))
}

fn selected_inputs(
    selected: &Path,
    applications: &Path,
    command_line_tools: &Path,
    control: &Path,
    query: impl FnOnce(&Path) -> io::Result<Option<(PathBuf, PathBuf)>>,
) -> io::Result<super::macos_sdk::Inputs> {
    let developer = validate_developer(selected, applications, command_line_tools, control)?;
    let Some((clang, sdk)) = query(&developer)? else {
        return Ok(Default::default());
    };
    roots(&developer, &clang, &sdk, control)
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
    let developer_bin = developer.join("usr/bin").canonicalize()?;
    let installation = if developer
        .file_name()
        .is_some_and(|name| name == "Developer")
        && developer
            .parent()
            .is_some_and(|parent| parent.file_name().is_some_and(|name| name == "Contents"))
    {
        developer.parent().ok_or_else(denied)?
    } else {
        developer
    };
    if installation.canonicalize()? != installation
        || !installation.is_dir()
        || installation.starts_with(control)
        || control.starts_with(installation)
    {
        return Err(denied());
    }
    if !clang.is_file()
        || [&bin, &developer_bin, &sdk].iter().any(|root| {
            !root.is_dir()
                || !root.starts_with(developer)
                || root.starts_with(control)
                || control.starts_with(root)
        })
    {
        return Err(denied());
    }
    Ok(super::macos_sdk::Inputs {
        executable_directories: vec![bin.clone(), developer_bin.clone()],
        // xcrun opens the selected application's Info.plist and shared
        // frameworks while finding tools/SDKs. Keep that public installation
        // readable as a unit; unrelated applications and user files stay out.
        // Native scopes require an exact read grant for each PATH entry.
        read_only: vec![installation.to_owned(), bin, developer_bin],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unsupported_apple_selection_skips_metadata_without_blocking_startup() {
        let fixture = tempfile::tempdir().unwrap();
        let base = fixture.path().canonicalize().unwrap();
        let control = base.join("control");
        let selected = base.join("Volumes/Custom/Xcode.app/Contents/Developer");
        std::fs::create_dir(&control).unwrap();
        std::fs::create_dir_all(&selected).unwrap();
        let inputs = selected_inputs(
            &selected,
            &base.join("Applications"),
            &base.join("CommandLineTools"),
            &control,
            |_| panic!("unsupported selection must not query xcrun"),
        )
        .unwrap();
        assert!(inputs.read_only.is_empty());
        assert!(inputs.executable_directories.is_empty());
    }

    #[test]
    fn selected_command_line_tools_do_not_admit_their_parent() {
        let fixture = tempfile::tempdir().unwrap();
        let base = fixture.path().canonicalize().unwrap();
        let developer = base.join("Library/Developer/CommandLineTools");
        let bin = developer.join("usr/bin");
        let sdk = developer.join("SDKs/MacOSX.sdk");
        let control = base.join("control");
        for directory in [&bin, &sdk, &control] {
            std::fs::create_dir_all(directory).unwrap();
        }
        let clang = bin.join("clang");
        std::fs::write(&clang, b"compiler").unwrap();
        let selected =
            validate_developer(&developer, &base.join("Applications"), &developer, &control)
                .unwrap();
        let inputs = roots(&selected, &clang, &sdk, &control).unwrap();
        assert!(inputs.read_only.contains(&developer));
        assert!(
            !inputs
                .read_only
                .contains(&developer.parent().unwrap().to_owned())
        );
        assert!(
            inputs
                .executable_directories
                .iter()
                .all(|path| inputs.read_only.contains(path))
        );
    }
    #[test]
    fn selected_apple_inputs_exclude_other_installations_and_private_roots() {
        let fixture = tempfile::tempdir().unwrap();
        let base = fixture.path().canonicalize().unwrap();
        let applications = base.join("Applications");
        let developer = applications.join("Xcode_26.app/Contents/Developer");
        let bin = developer.join("Toolchains/XcodeDefault.xctoolchain/usr/bin");
        let sdk = developer.join("Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk");
        let control = base.join("control");
        for directory in [
            &bin,
            &bin.parent().unwrap().join("lib"),
            &sdk,
            &control,
            &developer.join("usr/bin"),
            &developer.join("usr/lib"),
        ] {
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
        assert_eq!(inputs.executable_directories.len(), 2);
        assert_eq!(inputs.executable_directories.first(), Some(&bin));
        assert!(
            inputs
                .executable_directories
                .contains(&developer.join("usr/bin"))
        );
        assert!(
            inputs
                .executable_directories
                .iter()
                .all(|path| inputs.read_only.contains(path)),
            "native scopes require explicit read grants for every executable directory"
        );
        let contents = developer.parent().unwrap();
        assert!(inputs.read_only.contains(&contents.to_owned()));
        assert_eq!(inputs.read_only.len(), 3);
        assert!(!inputs.read_only.contains(&applications));
        let sibling_control = contents.join("private-control");
        std::fs::create_dir(&sibling_control).unwrap();
        assert_eq!(
            roots(&selected, &clang, &sdk, &sibling_control)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        std::fs::remove_dir(developer.join("usr/lib")).unwrap();
        assert_eq!(
            roots(&selected, &clang, &sdk, &control)
                .unwrap()
                .read_only
                .len(),
            3
        );
        std::fs::create_dir(developer.join("usr/lib")).unwrap();
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
        let implementation = developer.join("usr/bin");
        std::fs::rename(&implementation, developer.join("usr/bin.saved")).unwrap();
        std::os::unix::fs::symlink(&private, &implementation).unwrap();
        assert_eq!(
            roots(&selected, &clang, &sdk, &control)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        std::fs::remove_file(&implementation).unwrap();
        std::fs::rename(developer.join("usr/bin.saved"), &implementation).unwrap();
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
