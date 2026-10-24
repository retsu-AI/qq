//! AppContainer access restrictions inside the existing kernel-owned command job.
//! The trusted helper consumes a framed request before handing its stdio to the
//! payload. No payload handle can refer to the worker or its command journal.
use super::{
    dispatch::{ToolCancellation, ToolTaskLease},
    process_context::NativeExecution,
};
use crate::command_owner::{
    CommandLauncher, NativeFilesystem, NativeNetwork, NativeResources, NativeWorkspaceIdentity,
};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    fs::File,
    io::{self, Read as _, Seek as _, Write as _},
    os::windows::fs::OpenOptionsExt as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

mod access;
mod arguments;
mod debugger;
mod desktop;
mod directory;
pub(super) mod fleet;
mod resources;
mod token;
const MAX_PACKET: usize = 4 * 1024 * 1024;
const READY: &[u8; 8] = b"QQWIN001";

#[derive(Clone)]
pub(super) struct WindowsOwner(Arc<Owner>);
struct Owner {
    launcher: CommandLauncher,
    source: File,
    image: Mutex<Option<Arc<HelperImage>>>,
    fleet: Mutex<Option<Arc<fleet::Fleet>>>,
}
pub(super) struct HelperImage {
    path: PathBuf,
    local_app_data: PathBuf,
    directory: Option<File>,
    temporary: Option<tempfile::TempDir>,
}
impl Drop for HelperImage {
    fn drop(&mut self) {
        let temporary = self.temporary.take();
        let directory = self.directory.take();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            drop(runtime.spawn_blocking(move || {
                drop(directory);
                drop(temporary);
            }));
        } else {
            drop(directory);
            drop(temporary);
        }
    }
}
pub(super) struct Startup {
    encoded: Vec<u8>,
    image: Arc<HelperImage>,
    fleet: Option<Arc<fleet::Fleet>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Packet {
    program: PathBuf,
    arguments: Vec<OsString>,
    environment: Vec<(OsString, OsString)>,
    cwd: PathBuf,
    cwd_identity: NativeWorkspaceIdentity,
    filesystem: NativeFilesystem,
    resources: NativeResources,
    cpu_parent_percent: Option<u32>,
    not_after_ms: u64,
}
impl WindowsOwner {
    pub(super) fn discover(launcher: CommandLauncher) -> io::Result<Self> {
        if !launcher.executable.is_absolute() {
            return Err(io::Error::other("native helper must be absolute"));
        }
        // Keep the original bytes readable without following its pathname again.
        // Lazy copying avoids work for Full commands that do not use this adapter.
        let source = File::options()
            .read(true)
            .share_mode(5)
            .open(&launcher.executable)?;
        if !source.metadata()?.is_file() {
            return Err(io::Error::other(
                "native helper must be a regular executable",
            ));
        }
        Ok(Self(Arc::new(Owner {
            launcher,
            source,
            image: Mutex::new(None),
            fleet: Mutex::new(None),
        })))
    }
    fn image(&self) -> io::Result<Arc<HelperImage>> {
        self.image_in(&std::env::temp_dir())
    }

    fn image_in(&self, temporary_root: &Path) -> io::Result<Arc<HelperImage>> {
        // This runs on the bounded preparation pool. Serialize the copy, but
        // retain only success: disk pressure and temporary I/O failures must
        // not disable restricted execution for this worker's lifetime.
        let mut cached = self
            .0
            .image
            .lock()
            .map_err(|_| io::Error::other("native helper image cache poisoned"))?;
        if let Some(image) = cached.as_ref() {
            return Ok(Arc::clone(image));
        }
        let size = self.0.source.metadata()?.len();
        crate::storage::check_path_blocking(temporary_root, size).map_err(io::Error::other)?;
        let temporary = tempfile::Builder::new()
            .prefix("qq-windows-helper-")
            .tempdir_in(temporary_root)?;
        let directory = crate::command_owner::windows_state::directory(temporary.path())
            .map_err(io::Error::other)?;
        let path = temporary.path().join("helper.exe");
        let mut destination = File::options().write(true).create_new(true).open(&path)?;
        // Clones share a file position on Windows. A previous partial copy
        // must not turn a retry into a truncated executable.
        let mut source = self.0.source.try_clone()?;
        source.rewind()?;
        if io::copy(&mut source.take(size.saturating_add(1)), &mut destination)? != size {
            return Err(io::Error::other("native helper changed while copying"));
        }
        drop(destination);
        let image = Arc::new(HelperImage {
            path,
            // Profile lookup belongs to the trusted helper; payload profile
            // paths still come only from the frozen command environment.
            local_app_data: directories::BaseDirs::new()
                .ok_or_else(|| {
                    io::Error::other("Windows local application-data directory is unavailable")
                })?
                .data_local_dir()
                .to_owned(),
            directory: Some(directory),
            temporary: Some(temporary),
        });
        *cached = Some(Arc::clone(&image));
        Ok(image)
    }
    pub(super) fn wrap(
        &self,
        command: tokio::process::Command,
        cwd: &cap_std::fs::Dir,
        native: Option<&NativeExecution>,
        deadline: Instant,
    ) -> io::Result<(tokio::process::Command, Option<Startup>)> {
        let Some(native) = native else {
            return Ok((command, None));
        };
        if !matches!(
            native.resources,
            NativeResources::Host | NativeResources::Committed { .. }
        ) {
            return Err(io::Error::other(
                "resource enforcement is unavailable for this accounting policy on Windows",
            ));
        }
        if !native.filesystem.is_restricted() {
            if native.network != NativeNetwork::Host || native.resources != NativeResources::Host {
                return Err(io::Error::other(
                    "network enforcement is unavailable without restricted file authority on Windows",
                ));
            }
            return Ok((command, None));
        }
        if native.network != NativeNetwork::Isolated {
            return Err(io::Error::other(
                "filesystem enforcement is unavailable without isolated network authority on Windows",
            ));
        }
        let fleet = if matches!(native.resources, NativeResources::Committed { .. }) {
            let mut cached = self
                .0
                .fleet
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(fleet) = cached.as_ref() {
                Some(fleet.clone())
            } else {
                let fleet = Arc::new(fleet::Fleet::discover()?);
                *cached = Some(fleet.clone());
                Some(fleet)
            }
        } else {
            None
        };
        let image = self.image()?;
        if let NativeFilesystem::Restricted { read_write, .. } = &native.filesystem {
            for ancestor in image
                .path
                .parent()
                .into_iter()
                .flat_map(|path| path.ancestors())
            {
                let identity = NativeWorkspaceIdentity::capture(ancestor)?;
                if read_write.iter().any(|grant| grant.identity == identity) {
                    return Err(io::Error::other(
                        "writable authority includes the trusted native helper",
                    ));
                }
            }
        }
        let original = command.as_std();
        let packet = Packet {
            program: resolve_program(original)?,
            arguments: original.get_args().map(OsString::from).collect(),
            environment: original
                .get_envs()
                .filter_map(|(name, value)| value.map(|value| (name.to_owned(), value.to_owned())))
                .collect(),
            cwd: original
                .get_current_dir()
                .ok_or_else(|| io::Error::other("missing prepared working directory"))?
                .into(),
            cwd_identity: NativeWorkspaceIdentity::held(cwd)?,
            filesystem: native.filesystem.clone(),
            resources: native.resources,
            cpu_parent_percent: fleet.as_ref().map(|_| fleet::CPU_PERCENT),
            not_after_ms: unix_ms().saturating_add(
                u64::try_from(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis(),
                )
                .unwrap_or(u64::MAX),
            ),
        };
        let encoded = serde_json::to_vec(&packet).map_err(io::Error::other)?;
        if encoded.len() > MAX_PACKET {
            return Err(io::Error::other("native launch packet exceeds its bound"));
        }
        let mut wrapped = tokio::process::Command::new(&image.path);
        wrapped
            .env_clear()
            .env("LOCALAPPDATA", &image.local_app_data)
            .current_dir(
                image
                    .path
                    .parent()
                    .expect("private helper image has a directory"),
            )
            .args(&self.0.launcher.arguments)
            .arg("qq-windows-launch:1");
        for key in ["SYSTEMROOT", "WINDIR"] {
            if let Some(value) = std::env::var_os(key) {
                wrapped.env(key, value);
            }
        }
        Ok((
            wrapped,
            Some(Startup {
                encoded,
                image,
                fleet,
            }),
        ))
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn spawn(
    mut command: tokio::process::Command,
    startup: Option<Startup>,
    piped_stdin: bool,
    directory: cap_std::fs::Dir,
    process: &ToolTaskLease,
    cancelled: &ToolCancellation,
    deadline: Instant,
) -> io::Result<super::windows_job::Child> {
    if startup.is_some() {
        command.stdin(std::process::Stdio::piped());
    }
    let mut child = super::windows_job::spawn(
        command,
        directory,
        process,
        cancelled,
        deadline,
        startup.as_ref().and_then(|startup| startup.fleet.clone()),
    )
    .await?;
    let Some(startup) = startup else {
        return Ok(child);
    };
    child._native_image = Some(startup.image);
    let authorized = tokio::select! {
        biased;
        () = cancelled.cancelled() => Err(io::Error::other("native startup cancelled")),
        () = tokio::time::sleep_until(deadline.into()) => Err(io::Error::other("native startup timed out")),
        result = async {
            let stdin = child.stdin.as_mut().ok_or_else(|| io::Error::other("missing native request stream"))?;
            stdin.write_all(&(startup.encoded.len() as u64).to_le_bytes()).await?;
            stdin.write_all(&startup.encoded).await?;
            let mut ready = [0; 8];
            let stderr = child.stderr.as_mut().ok_or_else(|| io::Error::other("missing native readiness stream"))?;
            let mut received = 0;
            while received < ready.len() {
                let count = stderr.read(&mut ready[received..]).await?;
                if count == 0 { break; }
                received += count;
            }
            if received != READY.len() || &ready != READY {
                let mut report = ready[..received].to_vec();
                stderr.take((4096 - received) as u64).read_to_end(&mut report).await?;
                return Err(io::Error::other(format!("native preparation failed: {}", String::from_utf8_lossy(&report).trim())));
            }
            if cancelled.is_cancelled() || Instant::now() >= deadline { return Err(io::Error::other("native startup stopped")); }
            child.stdin.as_mut().expect("owned request stream").write_all(&[1]).await
        } => result,
    };
    if let Err(error) = authorized {
        child.start_kill()?;
        child.wait().await?;
        return Err(error);
    }
    if !piped_stdin {
        drop(child.stdin.take());
    }
    Ok(child)
}

fn resolve_program(command: &std::process::Command) -> io::Result<PathBuf> {
    let program = PathBuf::from(command.get_program());
    if program.is_absolute() {
        return program.canonicalize();
    }
    if program.components().count() > 1 {
        return command
            .get_current_dir()
            .ok_or_else(|| io::Error::other("missing prepared cwd"))?
            .join(program)
            .canonicalize();
    }
    let name = program
        .to_str()
        .ok_or_else(|| io::Error::other("native program is not Unicode"))?;
    if name.eq_ignore_ascii_case("cmd") || name.eq_ignore_ascii_case("cmd.exe") {
        return std::env::var_os("SYSTEMROOT")
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::other("Windows system directory unavailable"))?
            .join("System32/cmd.exe")
            .canonicalize();
    }
    let path = command
        .get_envs()
        .find(|(key, _)| key.to_string_lossy().eq_ignore_ascii_case("PATH"))
        .and_then(|(_, value)| value)
        .ok_or_else(|| io::Error::other("native executable lookup requires captured PATH"))?;
    for directory in std::env::split_paths(path) {
        if !directory.is_absolute() {
            return Err(io::Error::other("native executable PATH must be absolute"));
        }
        let mut candidate = directory.join(&program);
        if candidate.extension().is_none() {
            candidate.set_extension("exe");
        }
        if candidate.is_file() {
            return candidate.canonicalize();
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "native executable not found in captured PATH",
    ))
}

fn unix_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

/// Dedicated pre-runtime entry. Success exits this helper with the payload's
/// full Windows exit code; returning an error never starts a host fallback.
pub(crate) fn execute(encoded: &str) -> io::Result<u8> {
    use std::os::windows::io::AsHandle as _;
    if encoded != "qq-windows-launch:1" {
        return Err(io::Error::other("invalid native launch descriptor"));
    }
    let image = std::env::current_exe()?;
    let directory = image
        .parent()
        .ok_or_else(|| io::Error::other("native helper image has no directory"))?;
    let _private =
        crate::command_owner::windows_state::directory(directory).map_err(io::Error::other)?;
    if NativeWorkspaceIdentity::capture(&std::env::current_dir()?)?
        != NativeWorkspaceIdentity::capture(directory)?
    {
        return Err(io::Error::other(
            "native helper working directory is not its private image directory",
        ));
    }
    // A buffered StdinLock could prefetch user input while reading the final
    // authorization byte. Keep the protocol reads unbuffered: the payload
    // inherits the same pipe after this exact prefix has been consumed.
    let mut input = File::from(std::io::stdin().as_handle().try_clone_to_owned()?);
    let mut length = [0; 8];
    input.read_exact(&mut length)?;
    let length = usize::try_from(u64::from_le_bytes(length)).map_err(io::Error::other)?;
    if length > MAX_PACKET {
        return Err(io::Error::other("native launch packet exceeds its bound"));
    }
    let mut body = vec![0; length];
    input.read_exact(&mut body)?;
    let packet: Packet = serde_json::from_slice(&body).map_err(io::Error::other)?;
    let cwd = crate::workspace::Workspace::open(&packet.cwd)?;
    if NativeWorkspaceIdentity::held(cwd.root())? != packet.cwd_identity {
        return Err(io::Error::other("native working directory changed"));
    }
    if unix_ms() >= packet.not_after_ms {
        return Err(io::Error::other("native preparation expired"));
    }
    let launch_cwd = directory::working_directory(&packet.program, &packet.cwd)?;
    if NativeWorkspaceIdentity::capture(launch_cwd)? != packet.cwd_identity {
        return Err(io::Error::other(
            "native launch directory changed during normalization",
        ));
    }
    let name = access::identity(&packet.filesystem)?;
    let profile =
        rappct::AppContainerProfile::ensure(&name, "QQ task", Some("QQ native task scope"))
            .map_err(io::Error::other)?;
    let _grants = access::prepare(&packet.filesystem, &profile.sid, packet.not_after_ms)?;
    let cpu_rate_percent = resources::apply(packet.resources, packet.cpu_parent_percent)?;
    let capabilities = access::capabilities(&packet.filesystem, &profile.sid)?;
    let write_sid = access::task_write_sid(&packet.filesystem)?;
    let primary = token::create(&write_sid)?;
    let ui_name = format!(
        "qq-native-{}",
        qq_protocol::ToolCallId::generate().map_err(io::Error::other)?
    );
    let ui = desktop::ProcessDesktop::create(
        &ui_name,
        &win_custody::current_user_sid_string()?,
        &write_sid,
        profile.sid.as_string(),
    )?;
    let options = rappct::LaunchOptions {
        exe: packet.program.clone(),
        cmdline: Some(arguments::command_line(&packet.program, &packet.arguments)?),
        cwd: Some(launch_cwd.to_owned()),
        desktop: Some(ui.path.clone().into()),
        env: Some(packet.environment),
        suspended: true,
        join_job: Some(rappct::JobLimits {
            kill_on_job_close: true,
            cpu_rate_percent,
            ..Default::default()
        }),
        ..Default::default()
    }
    .try_with_primary_token(primary.as_handle())
    .map_err(io::Error::other)?
    .try_with_stdio_inherit(
        Some(std::io::stdin().as_handle()),
        Some(std::io::stdout().as_handle()),
        Some(std::io::stderr().as_handle()),
    )
    .map_err(io::Error::other)?;
    // The outer QQ command job already owns this suspended process and all
    // descendants. AppContainer and its explicit handle list are in force now.
    let mut child =
        rappct::launch_in_container_with_io(&capabilities, &options).map_err(io::Error::other)?;
    // Creating the suspended lowbox materializes its private object namespace.
    // Grant only this task SID object creation before any payload code runs.
    profile
        .grant_named_object_creation(&write_sid)
        .map_err(io::Error::other)?;
    let debugger = debugger::Debugger::start(child.pid, &packet.program)?;
    std::io::stderr().write_all(READY)?;
    let mut release = [0];
    input.read_exact(&mut release)?;
    drop(input);
    if release != [1] || unix_ms() >= packet.not_after_ms {
        child.terminate(1).map_err(io::Error::other)?;
        return Err(io::Error::other("native authorization expired"));
    }
    child.resume().map_err(io::Error::other)?;
    let code = child.wait(None).map_err(io::Error::other)?;
    if let Some(debugger) = debugger {
        debugger.finish();
    }

    // Closing the AppContainer job terminates descendants. Private UI handles
    // remain process-owned until this single-shot helper exits, including error
    // paths; the worker independently proves the outer job empty before results
    // and artifact publication. Never release UI from the long-lived worker.
    drop(child);
    std::process::exit(code as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_image_retries_after_storage_pressure_and_reuses_only_success() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.exe");
        let bytes = b"complete trusted helper image";
        std::fs::write(&source, bytes).unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let temporary = temporary.path().canonicalize().unwrap();
        let owner =
            WindowsOwner::discover(CommandLauncher::new(source.canonicalize().unwrap())).unwrap();
        let space = crate::storage::SpaceOverride::new(&temporary, crate::storage::RESERVE_BYTES);
        let error = match owner.image_in(&temporary) {
            Ok(_) => panic!("helper copy must preserve the reserve"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("disk_space_low"), "{error}");
        assert_eq!(std::fs::read_dir(&temporary).unwrap().count(), 0);
        // Model the shared position left by an interrupted copy, too.
        owner
            .0
            .source
            .try_clone()
            .unwrap()
            .seek(io::SeekFrom::Start(3))
            .unwrap();
        space.set(crate::storage::RESERVE_BYTES + bytes.len() as u64);
        let image = owner.image_in(&temporary).unwrap();
        assert_eq!(std::fs::read(&image.path).unwrap(), bytes);
        space.set(0);
        let reused = owner.image_in(&temporary).unwrap();
        assert!(Arc::ptr_eq(&image, &reused));
        assert_eq!(std::fs::read_dir(&temporary).unwrap().count(), 1);
    }

    #[test]
    fn helper_image_concurrent_initialization_keeps_one_complete_copy() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.exe");
        let bytes = b"complete trusted helper image";
        std::fs::write(&source, bytes).unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let owner =
            WindowsOwner::discover(CommandLauncher::new(source.canonicalize().unwrap())).unwrap();
        std::thread::scope(|threads| {
            let handles: Vec<_> = (0..4)
                .map(|_| threads.spawn(|| owner.image_in(temporary.path()).unwrap()))
                .collect();
            let images: Vec<_> = handles
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect();
            for image in &images {
                assert!(Arc::ptr_eq(&images[0], image));
                assert_eq!(std::fs::read(&image.path).unwrap(), bytes);
            }
        });
        assert_eq!(std::fs::read_dir(temporary.path()).unwrap().count(), 1);
    }
}
