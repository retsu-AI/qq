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
        .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_default();
    let mut rust = false;
    for program in [
        "git", "node", "cargo", "rustc", "rustup", "python", "python3", "go", "dotnet", "rg", "gh",
        "cmake", "clang", "cl", "link", "ninja", "make",
    ] {
        let Some(image) = paths
            .iter()
            .filter(|path| path.is_absolute())
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
    let bases = ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    for base in &bases {
        for name in ["Microsoft Visual Studio", "Windows Kits", "Microsoft SDKs"] {
            include(roots, &base.join(name))?;
        }
    }
    let mut environment = Vec::<(String, OsString)>::new();
    if rust {
        let compiler = super::windows_toolchain::discover(&bases, std::env::consts::ARCH)?;
        if let Some(compiler) = &compiler {
            for root in &compiler.roots {
                include(roots, root)?;
            }
            let path = std::env::join_paths(std::iter::once(&compiler.binaries).chain(&paths))
                .map_err(io::Error::other)?;
            environment.push(("PATH".into(), path));
        }
        // Preserve explicit developer search paths. A normal desktop launch
        // instead receives the installed native compiler's library/header paths.
        let libraries = match std::env::var_os("LIB").filter(|value| !value.is_empty()) {
            Some(value) => Some(value),
            None => compiler
                .as_ref()
                .map(|compiler| std::env::join_paths(&compiler.libraries))
                .transpose()
                .map_err(io::Error::other)?,
        };
        if let Some(libraries) = libraries {
            search_paths(roots, &mut environment, "LIB", libraries)?;
        }
        let headers = match std::env::var_os("INCLUDE") {
            Some(value) => Some(value),
            None => compiler
                .as_ref()
                .filter(|compiler| !compiler.includes.is_empty())
                .map(|compiler| std::env::join_paths(&compiler.includes))
                .transpose()
                .map_err(io::Error::other)?,
        };
        if let Some(headers) = headers {
            search_paths(roots, &mut environment, "INCLUDE", headers)?;
        }
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

fn search_paths(
    roots: &mut BTreeSet<PathBuf>,
    environment: &mut Vec<(String, OsString)>,
    name: &str,
    value: OsString,
) -> io::Result<()> {
    for path in std::env::split_paths(&value).filter(|path| path.is_absolute()) {
        include(roots, &path)?;
    }
    environment.push((name.to_owned(), value));
    Ok(())
}
