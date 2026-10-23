//! Discover installed compiler inputs without executing host programs.
use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub(super) struct Toolchain {
    pub roots: [PathBuf; 2],
    pub binaries: PathBuf,
    pub libraries: [PathBuf; 3],
    pub includes: Vec<PathBuf>,
}

fn directories(path: &Path, remaining: &mut usize) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut paths = Vec::new();
    for entry in entries {
        *remaining = remaining.checked_sub(1).ok_or_else(|| {
            io::Error::other("installed compiler discovery exceeds its entry bound")
        })?;
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() && !kind.is_symlink() {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn version(path: &Path) -> Option<Vec<u32>> {
    path.file_name()?
        .to_str()?
        .split('.')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()
}

pub(super) fn discover(bases: &[PathBuf], architecture: &str) -> io::Result<Option<Toolchain>> {
    let (target, hosts) = match architecture {
        "aarch64" => ("arm64", ["Hostarm64", "Hostx64"]),
        "x86_64" => ("x64", ["Hostx64", "Hostx86"]),
        "x86" => ("x86", ["Hostx86", "Hostx64"]),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported MSVC host architecture",
            ));
        }
    };
    let mut remaining = 4096;
    let mut compiler: Option<(Vec<u32>, PathBuf, PathBuf, PathBuf)> = None;
    let mut sdk: Option<(Vec<u32>, PathBuf, PathBuf)> = None;
    for base in bases {
        for release in directories(&base.join("Microsoft Visual Studio"), &mut remaining)? {
            if version(&release).is_none() {
                continue;
            }
            for edition in ["Enterprise", "Professional", "Community", "BuildTools"] {
                let installation = release.join(edition);
                for toolset in directories(
                    &installation.join("VC").join("Tools").join("MSVC"),
                    &mut remaining,
                )? {
                    let Some(version) = version(&toolset) else {
                        continue;
                    };
                    if !toolset
                        .join("lib")
                        .join(target)
                        .join("msvcrt.lib")
                        .is_file()
                    {
                        continue;
                    }
                    let Some(binaries) = hosts
                        .iter()
                        .map(|host| toolset.join("bin").join(host).join(target))
                        .find(|path| path.join("link.exe").is_file())
                    else {
                        continue;
                    };
                    if compiler
                        .as_ref()
                        .is_none_or(|(current, ..)| &version > current)
                    {
                        compiler = Some((version, installation.clone(), toolset, binaries));
                    }
                }
            }
        }
        let root = base.join("Windows Kits").join("10");
        for library in directories(&root.join("Lib"), &mut remaining)? {
            let Some(version) = version(&library) else {
                continue;
            };
            if !library
                .join("um")
                .join(target)
                .join("kernel32.lib")
                .is_file()
                || !library.join("ucrt").join(target).join("ucrt.lib").is_file()
            {
                continue;
            }
            if sdk.as_ref().is_none_or(|(current, ..)| &version > current) {
                sdk = Some((version, root.clone(), library));
            }
        }
    }
    let (Some((_, installation, toolset, binaries)), Some((_, sdk, library))) = (compiler, sdk)
    else {
        return Ok(None);
    };
    let includes = std::iter::once(toolset.join("include"))
        .chain(["ucrt", "um", "shared", "winrt", "cppwinrt"].map(|name| {
            sdk.join("Include")
                .join(library.file_name().expect("version directory"))
                .join(name)
        }))
        .filter(|path| path.is_dir())
        .collect();
    Ok(Some(Toolchain {
        roots: [installation, sdk],
        binaries,
        libraries: [
            toolset.join("lib").join(target),
            library.join("ucrt").join(target),
            library.join("um").join(target),
        ],
        includes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compiler(base: &Path, release: &str, version: &str, target: &str, host: &str) -> PathBuf {
        let root = base
            .join("Microsoft Visual Studio")
            .join(release)
            .join("Community/VC/Tools/MSVC")
            .join(version);
        let binaries = root.join("bin").join(host).join(target);
        fs::create_dir_all(&binaries).unwrap();
        // Deliberately non-executable files: discovery must never run them.
        fs::write(binaries.join("link.exe"), "not an executable").unwrap();
        fs::create_dir_all(root.join("lib").join(target)).unwrap();
        fs::write(root.join("lib").join(target).join("msvcrt.lib"), "library").unwrap();
        fs::create_dir_all(root.join("include")).unwrap();
        root
    }

    fn sdk(base: &Path, version: &str, target: &str) -> PathBuf {
        let root = base.join("Windows Kits/10");
        for (kind, file) in [("um", "kernel32.lib"), ("ucrt", "ucrt.lib")] {
            let directory = root.join("Lib").join(version).join(kind).join(target);
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join(file), "library").unwrap();
        }
        for kind in ["ucrt", "um", "shared"] {
            fs::create_dir_all(root.join("Include").join(version).join(kind)).unwrap();
        }
        root
    }

    #[test]
    fn discovers_native_inputs_by_numeric_version_and_architecture() {
        for (architecture, target, host) in [
            ("aarch64", "arm64", "Hostarm64"),
            ("x86_64", "x64", "Hostx64"),
            ("x86", "x86", "Hostx86"),
        ] {
            let root = tempfile::tempdir().unwrap();
            compiler(root.path(), "2022", "14.9.1", target, host);
            let toolset = compiler(root.path(), "18", "14.10.2", target, host);
            // Newer but incomplete installations must not hide a working one.
            fs::create_dir_all(
                root.path()
                    .join("Microsoft Visual Studio/18/Community/VC/Tools/MSVC/14.11.0"),
            )
            .unwrap();
            sdk(root.path(), "10.0.9.0", target);
            let sdk = sdk(root.path(), "10.0.10.0", target);
            fs::create_dir_all(sdk.join("Lib/10.0.11.0")).unwrap();
            let found = discover(&[root.path().to_owned()], architecture)
                .unwrap()
                .unwrap();
            assert_eq!(found.binaries, toolset.join("bin").join(host).join(target));
            assert_eq!(
                found.libraries,
                [
                    toolset.join("lib").join(target),
                    sdk.join("Lib/10.0.10.0/ucrt").join(target),
                    sdk.join("Lib/10.0.10.0/um").join(target)
                ]
            );
            assert_eq!(
                found.roots,
                [
                    root.path().join("Microsoft Visual Studio/18/Community"),
                    sdk.clone()
                ]
            );
            assert_eq!(
                found.includes,
                [
                    toolset.join("include"),
                    sdk.join("Include/10.0.10.0/ucrt"),
                    sdk.join("Include/10.0.10.0/um"),
                    sdk.join("Include/10.0.10.0/shared")
                ]
            );
        }
    }

    #[test]
    fn absent_or_incompatible_installations_add_no_inputs() {
        for (compiler_target, sdk_target) in [
            (None, None),
            (None, Some("x64")),
            (Some("x64"), None),
            (Some("arm64"), Some("arm64")),
        ] {
            let root = tempfile::tempdir().unwrap();
            if let Some(target) = compiler_target {
                compiler(
                    root.path(),
                    "18",
                    "14.10.2",
                    target,
                    if target == "arm64" {
                        "Hostarm64"
                    } else {
                        "Hostx64"
                    },
                );
            }
            if let Some(target) = sdk_target {
                sdk(root.path(), "10.0.10.0", target);
            }
            assert!(
                discover(&[root.path().to_owned()], "x86_64")
                    .unwrap()
                    .is_none()
            );
        }
    }
}
