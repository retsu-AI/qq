//! Installed software inputs supplied by the host, never by tool arguments.
use qq_core::command_owner::NativeEnvironment;
use std::{
    collections::BTreeSet,
    ffi::OsString,
    io,
    path::{Path, PathBuf},
};

fn include(roots: &mut BTreeSet<PathBuf>, path: &Path) -> io::Result<bool> {
    match std::fs::canonicalize(path) {
        Ok(path) if path.is_dir() => {
            roots.insert(path);
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub(super) fn capture(roots: &mut BTreeSet<PathBuf>) -> io::Result<Option<NativeEnvironment>> {
    let paths = std::env::var_os("PATH")
        .map(|value| {
            std::env::split_paths(&value)
                .filter(|path| path.is_absolute())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut rust = false;
    for program in [
        "git", "node", "cargo", "rustc", "rustup", "python", "python3", "go", "dotnet", "rg", "gh",
        "cmake", "clang", "ninja", "make",
    ] {
        let Some(image) = paths
            .iter()
            .map(|path| path.join(format!("{program}.exe")))
            .find(|path| path.is_file())
        else {
            continue;
        };
        let image = image.canonicalize()?;
        let Some(directory) = image.parent() else {
            continue;
        };
        include(roots, directory)?;
        if program == "git"
            && directory.parent().is_some_and(|root| {
                root.file_name().is_some_and(|name| {
                    name.to_str()
                        .is_some_and(|name| name.eq_ignore_ascii_case("Git"))
                })
            })
        {
            include(roots, directory.parent().expect("checked Git installation"))?;
        }
        rust |= matches!(program, "cargo" | "rustc" | "rustup");
    }
    // Native linkers and SDK libraries are discovered from Windows' registry by
    // Rust/MSVC. Grant their public installation trees, not the user profile.
    for base in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        if let Some(base) = std::env::var_os(base) {
            for name in ["Microsoft Visual Studio", "Windows Kits", "Microsoft SDKs"] {
                include(roots, &PathBuf::from(&base).join(name))?;
            }
        }
    }
    let mut environment = Vec::<(String, OsString)>::new();
    if rust {
        let home = std::env::var_os("RUSTUP_HOME")
            .map(PathBuf::from)
            .or_else(|| directories::BaseDirs::new().map(|base| base.home_dir().join(".rustup")));
        if let Some(home) = home
            && include(roots, &home)?
        {
            environment.push(("RUSTUP_HOME".into(), home.canonicalize()?.into_os_string()));
        }
        if let Some(toolchain) = std::env::var_os("RUSTUP_TOOLCHAIN") {
            environment.push(("RUSTUP_TOOLCHAIN".into(), toolchain));
        }
    }
    if environment.is_empty() {
        Ok(None)
    } else {
        NativeEnvironment::from_values(environment)
            .map(Some)
            .map_err(io::Error::other)
    }
}
