//! Fixed, job-custodied MSVC metadata query. No project commands or ambient PATH lookup.
use qq_core::command_owner::NativeSdkEnvironment;
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    io::{self, Read as _, Write as _},
    os::windows::io::AsRawHandle as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    linker: PathBuf,
    path: Vec<PathBuf>,
    libraries: Vec<PathBuf>,
    includes: Vec<PathBuf>,
}

pub(super) struct Inputs {
    pub bins: Vec<PathBuf>,
    pub read_only: Vec<PathBuf>,
    pub environment: NativeSdkEnvironment,
}

fn target() -> io::Result<&'static str> {
    match std::env::consts::ARCH {
        "aarch64" => Ok("aarch64-pc-windows-msvc"),
        "x86_64" => Ok("x86_64-pc-windows-msvc"),
        _ => Err(io::Error::other(
            "unsupported Windows compiler architecture",
        )),
    }
}

// No developer-command-prompt overrides, custom SDK root, or arbitrary PATH is consulted.
struct ConventionalEnvironment;
impl find_msvc_tools::EnvGetter for ConventionalEnvironment {
    fn get_env(&self, name: &'static str) -> Option<find_msvc_tools::Env> {
        matches!(name, "ProgramFiles" | "ProgramFiles(x86)")
            .then(|| std::env::var_os(name))
            .flatten()
            .map(find_msvc_tools::Env::Owned)
    }
}

pub(crate) fn metadata_main() -> io::Result<()> {
    // The parent owns the original process handle and assigns the kill-on-close job
    // before this release. In particular, the library cannot start vswhere earlier.
    let mut release = [0];
    io::stdin().read_exact(&mut release)?;
    if release != *b"G" {
        return Err(denied());
    }
    let Some(tool) =
        find_msvc_tools::find_tool_with_env(target()?, "link.exe", &ConventionalEnvironment)
    else {
        println!("\nQQ_MSVC_METADATA:null");
        return Ok(());
    };
    let paths = |key: &str| {
        tool.env()
            .into_iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| std::env::split_paths(value).collect())
            .unwrap_or_default()
    };
    let metadata = Metadata {
        linker: tool.path().to_owned(),
        path: paths("PATH"),
        libraries: paths("LIB"),
        includes: paths("INCLUDE"),
    };
    let encoded = serde_json::to_vec(&Some(metadata)).map_err(io::Error::other)?;
    if encoded.len() > 32768 {
        return Err(io::Error::other("compiler metadata exceeds its bound"));
    }
    io::stdout().write_all(b"\nQQ_MSVC_METADATA:")?;
    io::stdout().write_all(&encoded)?;
    io::stdout().write_all(b"\n")
}

pub(super) fn discover(control: &Path) -> io::Result<Option<Inputs>> {
    let system =
        PathBuf::from(std::env::var_os("SYSTEMROOT").ok_or_else(denied)?).canonicalize()?;
    let drive = system.parent().ok_or_else(denied)?;
    if drive.parent().is_some()
        || !system
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("Windows"))
    {
        return Err(denied());
    }
    let mut program_files = Vec::new();
    let mut command = Command::new(std::env::current_exe()?);
    #[cfg(not(test))]
    command.arg("_windows-sdk-metadata");
    #[cfg(test)]
    command.args([
        "--exact",
        "runtime::windows_msvc::tests::metadata_process",
        "--nocapture",
    ]);
    command
        .current_dir(control)
        .env_clear()
        .env("SYSTEMROOT", &system)
        .env("PATH", system.join("System32"))
        .env("TEMP", control)
        .env("TMP", control);
    for (name, leaf) in [
        ("ProgramFiles", "Program Files"),
        ("ProgramFiles(x86)", "Program Files (x86)"),
    ] {
        let Some(value) = std::env::var_os(name) else {
            continue;
        };
        let value = PathBuf::from(value);
        if !value.is_absolute() {
            return Err(denied());
        }
        let value = value.canonicalize()?;
        if value != drive.join(leaf) {
            return Err(denied());
        }
        command.env(name, &value);
        program_files.push(value);
    }
    #[cfg(test)]
    command.env("QQ_MSVC_METADATA_TEST", "1");
    if program_files.is_empty() {
        return Ok(None);
    }
    // If ARM needs vswhere, its exact conventional image must not be redirected.
    for root in &program_files {
        let image = root
            .join("Microsoft Visual Studio")
            .join("Installer")
            .join("vswhere.exe");
        if image.exists() && image.canonicalize()? != image {
            return Err(denied());
        }
    }
    let mut limits = win32job::ExtendedLimitInfo::default();
    limits.limit_kill_on_job_close();
    let job = win32job::Job::create_with_limit_info(&limits).map_err(io::Error::other)?;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    if let Err(error) = job.assign_process(child.as_raw_handle() as isize) {
        // A helper denied release cannot launch a descendant; close stdin and let
        // the existing bounded collector reap it rather than leave a blocked helper.
        drop(child.stdin.take());
        let cleanup = super::windows_sdk::collect_query(child, 32768);
        return Err(io::Error::other(format!(
            "compiler helper custody failed: {error}; cleanup={cleanup:?}"
        )));
    }
    let release = child
        .stdin
        .take()
        .ok_or_else(denied)
        .and_then(|mut stdin| stdin.write_all(b"G"));
    if let Err(error) = release {
        drop(job);
        let cleanup = super::windows_sdk::collect_query(child, 32768);
        return Err(io::Error::other(format!(
            "compiler helper release failed: {error}; cleanup={cleanup:?}"
        )));
    }
    let result = super::windows_sdk::collect_query(child, 32768);
    // No helper/metadata descendants remain admitted beyond this one query.
    drop(job);
    let Some(bytes) = result? else {
        return Err(io::Error::other("compiler metadata helper failed"));
    };
    let metadata: Option<Metadata> = serde_json::from_slice(
        bytes
            .split(|byte| *byte == b'\n')
            .find_map(|line| line.strip_prefix(b"QQ_MSVC_METADATA:"))
            .ok_or_else(|| io::Error::other("compiler metadata frame missing"))?,
    )
    .map_err(io::Error::other)?;
    metadata
        .map(|metadata| validate(metadata, &program_files, control))
        .transpose()
}

#[derive(Clone, Copy)]
enum InputRole {
    Linker,
    Binary,
    Library,
    Include,
}

fn validate(metadata: Metadata, roots: &[PathBuf], control: &Path) -> io::Result<Inputs> {
    let arch = if target()?.starts_with("aarch64") {
        "arm64"
    } else {
        "x64"
    };
    let validate_path = |path: PathBuf, role: InputRole| -> io::Result<PathBuf> {
        if !path.is_absolute()
            || path.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
        {
            return Err(denied());
        }
        let canonical = path.canonicalize()?;
        if !dunce::simplified(&canonical)
            .as_os_str()
            .eq_ignore_ascii_case(dunce::simplified(&path).as_os_str())
            || canonical.starts_with(control)
            || control.starts_with(&canonical)
        {
            return Err(denied());
        }
        let relative = roots
            .iter()
            .find_map(|root| canonical.strip_prefix(root).ok())
            .ok_or_else(denied)?;
        let parts: Vec<_> = relative
            .iter()
            .map(|p| p.to_string_lossy().to_ascii_lowercase())
            .collect();
        let msvc = parts.len() >= 8
            && parts[0] == "microsoft visual studio"
            && parts[3..6] == ["vc", "tools", "msvc"]
            && parts[6].bytes().all(|b| b.is_ascii_digit() || b == b'.')
            && match parts[7].as_str() {
                "bin" => {
                    matches!(role, InputRole::Linker | InputRole::Binary)
                        && parts.len() == 10
                        && matches!(parts[8].as_str(), "hostarm64" | "hostx64" | "hostx86")
                        && (parts[9] == arch
                            || matches!(role, InputRole::Binary)
                                && parts[9] == parts[8].trim_start_matches("host"))
                }
                "lib" => matches!(role, InputRole::Library) && parts.len() == 9 && parts[8] == arch,
                "include" => matches!(role, InputRole::Include) && parts.len() == 8,
                "atlmfc" => {
                    matches!(role, InputRole::Include)
                        && matches!(parts.get(8).map(String::as_str), Some("include"))
                        && parts.len() == 9
                        || matches!(role, InputRole::Library)
                            && parts.len() == 10
                            && parts[8] == "lib"
                            && parts[9] == arch
                }
                _ => false,
            };
        let kits = parts.len() >= 5
            && parts[0..2] == ["windows kits", "10"]
            && parts[3].bytes().all(|b| b.is_ascii_digit() || b == b'.')
            && match parts[2].as_str() {
                "bin" => {
                    matches!(role, InputRole::Binary)
                        && parts.len() == 5
                        && matches!(parts[4].as_str(), "arm64" | "x64" | "x86")
                }
                "lib" => {
                    matches!(role, InputRole::Library)
                        && parts.len() == 6
                        && matches!(parts[4].as_str(), "ucrt" | "um")
                        && parts[5] == arch
                }
                "include" => {
                    matches!(role, InputRole::Include)
                        && parts.len() == 5
                        && matches!(
                            parts[4].as_str(),
                            "ucrt" | "um" | "shared" | "winrt" | "cppwinrt"
                        )
                }
                _ => false,
            };
        if !msvc && !kits {
            return Err(denied());
        }
        Ok(canonical)
    };
    if metadata.path.len() > 4
        || metadata.libraries.is_empty()
        || metadata.libraries.len() > 8
        || metadata.includes.len() > 8
    {
        return Err(denied());
    }
    if metadata.linker.file_name() != Some(OsStr::new("link.exe")) {
        return Err(denied());
    }
    let bin = validate_path(
        metadata.linker.parent().ok_or_else(denied)?.to_owned(),
        InputRole::Linker,
    )?;
    let linker = metadata.linker.canonicalize()?;
    if linker != bin.join("link.exe") {
        return Err(denied());
    }
    let mut bins = vec![bin];
    for path in metadata.path {
        // Some SDK versions report an absent legacy SDK bin directory.
        if !path.exists() {
            continue;
        }
        let path = validate_path(path, InputRole::Binary)?;
        if !bins.contains(&path) {
            bins.push(path);
        }
    }
    let libraries = metadata
        .libraries
        .into_iter()
        .map(|path| validate_path(path, InputRole::Library))
        .collect::<io::Result<Vec<_>>>()?;
    let includes = metadata
        .includes
        .into_iter()
        .map(|path| validate_path(path, InputRole::Include))
        .collect::<io::Result<Vec<_>>>()?;
    let read_only = bins
        .iter()
        .chain(&libraries)
        .chain(&includes)
        .cloned()
        .collect();
    Ok(Inputs {
        bins,
        read_only,
        environment: NativeSdkEnvironment {
            linker,
            libraries,
            includes,
            target: target()?.to_owned(),
        },
    })
}

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "compiler inputs escape conventional admitted SDK roots",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_msvc_inputs_reject_unrelated_and_control_roots() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        let program_files = root.join("Program Files");
        let arch = if target().unwrap().starts_with("aarch64") {
            "arm64"
        } else {
            "x64"
        };
        let sdk = program_files
            .join("Microsoft Visual Studio")
            .join("2022")
            .join("BuildTools")
            .join("VC")
            .join("Tools")
            .join("MSVC")
            .join("14.44.0");
        let bin = sdk.join("bin").join(format!("Host{arch}")).join(arch);
        let lib = sdk.join("lib").join(arch);
        let include = sdk.join("include");
        let control = root.join("private-control");
        for path in [&bin, &lib, &include, &control] {
            std::fs::create_dir_all(path).unwrap();
        }
        let linker = bin.join("link.exe");
        std::fs::write(&linker, b"owned image fixture").unwrap();
        let metadata = || Metadata {
            linker: linker.clone(),
            path: vec![bin.clone()],
            libraries: vec![lib.clone()],
            includes: vec![include.clone()],
        };
        assert!(validate(metadata(), std::slice::from_ref(&program_files), &control).is_ok());
        let normal = || Metadata {
            linker: dunce::simplified(&linker).to_owned(),
            path: vec![dunce::simplified(&bin).to_owned()],
            libraries: vec![dunce::simplified(&lib).to_owned()],
            includes: vec![dunce::simplified(&include).to_owned()],
        };
        assert!(validate(normal(), std::slice::from_ref(&program_files), &control).is_ok());
        let mut wrong_role = normal();
        wrong_role.libraries = vec![bin.clone()];
        assert!(validate(wrong_role, std::slice::from_ref(&program_files), &control).is_err());
        let mut wrong_role = normal();
        wrong_role.linker = lib.join("link.exe");
        std::fs::write(&wrong_role.linker, b"owned wrong-role fixture").unwrap();
        assert!(validate(wrong_role, std::slice::from_ref(&program_files), &control).is_err());
        let host = if arch == "arm64" { "x64" } else { "x86" };
        let cross_bin = sdk.join("bin").join(format!("Host{host}")).join(arch);
        let host_dlls = sdk.join("bin").join(format!("Host{host}")).join(host);
        for path in [&cross_bin, &host_dlls] {
            std::fs::create_dir_all(path).unwrap();
        }
        let cross_linker = cross_bin.join("link.exe");
        std::fs::write(&cross_linker, b"owned target linker").unwrap();
        let mut cross = metadata();
        cross.linker = cross_linker;
        cross.path = vec![cross_bin, host_dlls.clone()];
        assert!(validate(cross, std::slice::from_ref(&program_files), &control).is_ok());
        let mut wrong_host_linker = metadata();
        wrong_host_linker.linker = host_dlls.join("link.exe");
        std::fs::write(&wrong_host_linker.linker, b"owned host linker").unwrap();
        assert!(
            validate(
                wrong_host_linker,
                std::slice::from_ref(&program_files),
                &control
            )
            .is_err()
        );
        assert!(validate(metadata(), std::slice::from_ref(&program_files), &sdk).is_err());
        let mut unrelated = metadata();
        unrelated.libraries = vec![control];
        assert!(validate(unrelated, &[program_files], &root.join("other-control")).is_err());
    }

    #[test]
    #[ignore = "requires an installed MSVC toolchain; mandatory in the native Windows SDK workflow"]
    fn installed_msvc_metadata_selects_real_bounded_inputs() {
        let control = tempfile::tempdir().unwrap();
        let control = control.path().canonicalize().unwrap();
        let selected = discover(&control)
            .unwrap()
            .expect("installed MSVC toolchain");
        eprintln!(
            "Selected trusted linker: {}",
            selected.environment.linker.display()
        );
        eprintln!(
            "Selected SDK roots: bins={} libraries={} includes={}",
            selected.bins.len(),
            selected.environment.libraries.len(),
            selected.environment.includes.len()
        );
        let mut image = std::fs::File::open(&selected.environment.linker).unwrap();
        let mut signature = [0; 2];
        image.read_exact(&mut signature).unwrap();
        assert_eq!(&signature, b"MZ");
        assert!(!selected.environment.libraries.is_empty());
        assert!(
            selected
                .bins
                .iter()
                .all(|path| selected.read_only.contains(path))
        );
    }

    #[test]
    fn metadata_process() {
        if std::env::var_os("QQ_MSVC_METADATA_TEST").is_some() {
            metadata_main().unwrap();
        }
    }
}
