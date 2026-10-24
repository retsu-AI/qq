//! macOS launch isolation. The guardian stays outside the payload group and
//! retains its waitable leader until its owned group stops, even after owner loss.
//! Detached descendants retain Seatbelt restrictions but are outside group cleanup.
use super::{
    dispatch::{ToolCancellation, ToolTaskLease},
    process_context::NativeExecution,
};
use crate::command_owner::{
    CommandLauncher, NativeFilesystem, NativeNetwork, NativeResources, NativeWorkspaceIdentity,
};
use command_fds::CommandFdExt as _;
use serde::{Deserialize, Serialize};
#[cfg(target_os = "macos")]
use std::{
    ffi::OsString,
    os::{fd::AsRawFd as _, unix::ffi::OsStringExt as _},
};
use std::{
    io::{self, Read as _, Write as _},
    ops::{Deref, DerefMut},
    os::unix::{ffi::OsStrExt as _, fs::MetadataExt as _, process::ExitStatusExt as _},
    path::{Path, PathBuf},
    process::ExitStatus,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[cfg(any(target_os = "macos", test))]
mod resources;

const RESOURCE_STOPPED: i32 = i32::MIN;
const LAUNCH_FAILED: i32 = i32::MIN + 1;
const MAX_PACKET: usize = 4 * 1024 * 1024;
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

#[derive(Clone)]
pub(super) struct MacosOwner {
    launcher: CommandLauncher,
    _image: std::sync::Arc<HelperImage>,
}

struct HelperImage(Option<tempfile::TempDir>);
impl Drop for HelperImage {
    fn drop(&mut self) {
        let directory = self.0.take();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            drop(runtime.spawn_blocking(move || drop(directory)));
        } else {
            drop(directory);
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Packet {
    program: Vec<u8>,
    arguments: Vec<Vec<u8>>,
    environment: Vec<(Vec<u8>, Vec<u8>)>,
    cwd: std::path::PathBuf,
    cwd_identity: NativeWorkspaceIdentity,
    filesystem: NativeFilesystem,
    network: NativeNetwork,
    resources: NativeResources,
    not_after_ms: u64,
}

impl MacosOwner {
    pub(super) fn discover(mut launcher: CommandLauncher) -> io::Result<Self> {
        use std::os::unix::fs::PermissionsExt as _;
        if rustix::process::getuid().is_root()
            || rustix::process::getuid() != rustix::process::geteuid()
        {
            return Err(io::Error::other(
                "macOS command isolation requires an unprivileged owner",
            ));
        }
        let source = std::fs::File::open(&launcher.executable)?;
        let metadata = source.metadata()?;
        if !launcher.executable.is_absolute()
            || !metadata.is_file()
            || metadata.mode() & 0o111 == 0
            || metadata.mode() & 0o6022 != 0
        {
            return Err(io::Error::other(
                "native helper must be an absolute unprivileged executable",
            ));
        }
        // Never execute the original pathname again: a task may replace it,
        // an ancestor, or a hardlink to its inode after discovery. A fresh
        // private copy has no pre-existing aliases in task inputs.
        let image = tempfile::Builder::new()
            .prefix("qq-macos-helper-")
            .tempdir()?;
        crate::storage::check_path_blocking(image.path(), metadata.len())
            .map_err(io::Error::other)?;
        let path = image.path().join("helper");
        let mut copy = std::fs::File::options()
            .write(true)
            .create_new(true)
            .open(&path)?;
        if io::copy(
            &mut source.take(metadata.len().saturating_add(1)),
            &mut copy,
        )? != metadata.len()
        {
            return Err(io::Error::other("native helper changed while copying"));
        }
        copy.set_permissions(std::fs::Permissions::from_mode(0o500))?;
        drop(copy);
        launcher.executable = path.canonicalize()?;
        Ok(Self {
            launcher,
            _image: std::sync::Arc::new(HelperImage(Some(image))),
        })
    }

    pub(super) fn wrap(
        &self,
        command: tokio::process::Command,
        cwd: &cap_std::fs::Dir,
        native: Option<&NativeExecution>,
        deadline: Instant,
    ) -> io::Result<(tokio::process::Command, Startup)> {
        if let Some(native) = native {
            validate_resources(&native.filesystem, native.resources)?;
        }
        if let Some(NativeExecution {
            filesystem: NativeFilesystem::Restricted { read_write, .. },
            ..
        }) = native
        {
            for grant in read_write {
                if self
                    .launcher
                    .executable
                    .starts_with(grant.path.canonicalize()?)
                {
                    return Err(io::Error::other(
                        "writable authority includes the trusted native helper",
                    ));
                }
            }
        }
        let command = command.as_std();
        let packet = Packet {
            program: command.get_program().as_bytes().to_vec(),
            arguments: command
                .get_args()
                .map(|arg| arg.as_bytes().to_vec())
                .collect(),
            environment: command
                .get_envs()
                .filter_map(|(name, value)| {
                    value.map(|value| (name.as_bytes().to_vec(), value.as_bytes().to_vec()))
                })
                .collect(),
            cwd: command
                .get_current_dir()
                .ok_or_else(|| io::Error::other("missing prepared working directory"))?
                .to_owned(),
            cwd_identity: NativeWorkspaceIdentity::held(cwd)?,
            filesystem: native.map_or(NativeFilesystem::Host, |native| native.filesystem.clone()),
            network: native.map_or(NativeNetwork::Host, |native| native.network),
            resources: native.map_or(NativeResources::Host, |native| native.resources),
            not_after_ms: unix_ms().saturating_add(
                u64::try_from(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis(),
                )
                .unwrap_or(u64::MAX),
            ),
        };
        validate_environment(&packet)?;
        let encoded = serde_json::to_vec(&packet).map_err(io::Error::other)?;
        if encoded.len() > MAX_PACKET {
            return Err(io::Error::other("native launch packet exceeds its bound"));
        }
        let (control, inherited) = std::os::unix::net::UnixStream::pair()?;
        control.set_nonblocking(true)?;
        let mut wrapped = tokio::process::Command::new(&self.launcher.executable);
        wrapped
            .env_clear()
            .args(&self.launcher.arguments)
            .arg("qq-macos-launch:5");
        wrapped
            .as_std_mut()
            .fd_mappings(vec![command_fds::FdMapping {
                parent_fd: inherited.into(),
                child_fd: 3,
            }])
            .map_err(io::Error::other)?;
        Ok((
            wrapped,
            Startup {
                encoded,
                control: tokio::net::UnixStream::from_std(control)?,
                _image: self._image.clone(),
            },
        ))
    }
}

pub(super) struct Startup {
    encoded: Vec<u8>,
    control: tokio::net::UnixStream,
    _image: std::sync::Arc<HelperImage>,
}

pub(crate) struct Child {
    inner: tokio::process::Child,
    control: Option<tokio::net::UnixStream>,
    lease: Option<ToolTaskLease>,
    reported: Option<ExitStatus>,
    result_bytes: [u8; 4],
    result_read: usize,
    stopping: bool,
    resource_stopped: bool,
}
impl From<tokio::process::Child> for Child {
    fn from(inner: tokio::process::Child) -> Self {
        Self {
            inner,
            control: None,
            lease: None,
            reported: None,
            result_bytes: [0; 4],
            result_read: 0,
            stopping: false,
            resource_stopped: false,
        }
    }
}
impl Deref for Child {
    type Target = tokio::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl DerefMut for Child {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
impl Child {
    pub(super) fn resource_stopped(&self) -> bool {
        self.resource_stopped
    }

    pub(super) fn owns_payload_group(&self) -> bool {
        self.lease.is_some()
    }

    pub(super) fn start_kill(&mut self) -> io::Result<()> {
        if !self.owns_payload_group() {
            return self.inner.start_kill();
        }
        self.stopping = true;
        if let Some(control) = &self.control {
            // EOF requests cleanup without killing the guardian or losing the
            // read half carrying its post-cleanup payload status.
            match rustix::net::shutdown(control, rustix::net::Shutdown::Write) {
                Ok(()) | Err(rustix::io::Errno::NOTCONN | rustix::io::Errno::PIPE) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub(super) async fn wait(&mut self) -> io::Result<ExitStatus> {
        if !self.owns_payload_group() {
            return self.inner.wait().await;
        }
        if self.stopping
            && let Some(pid) = self.inner.id()
        {
            let pid = rustix::process::Pid::from_raw(i32::try_from(pid).map_err(io::Error::other)?)
                .ok_or_else(|| io::Error::other("invalid guardian pid"))?;
            // Use the OS clock, independently of simulated task-budget clocks.
            // A timed-out guardian keeps the owned group; dropping this observer only
            // closes its channel, never kills it before cleanup finishes.
            super::native_exit::confirm(move || {
                rustix::process::waitid(
                    rustix::process::WaitId::Pid(pid),
                    rustix::process::WaitIdOptions::EXITED
                        | rustix::process::WaitIdOptions::NOHANG
                        | rustix::process::WaitIdOptions::NOWAIT,
                )
                .map(|status| status.is_some())
                .or_else(|error| {
                    if error == rustix::io::Errno::INTR {
                        Ok(false)
                    } else {
                        Err(error)
                    }
                })
                .map_err(Into::into)
            })
            .await?;
        }
        let status = self.inner.wait().await?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "native guardian exited without confirming owned-group cleanup: {status}"
            )));
        }
        if self.reported.is_none() {
            let control = self
                .control
                .as_mut()
                .ok_or_else(|| io::Error::other("missing native result channel"))?;
            while self.result_read < self.result_bytes.len() {
                let count = control
                    .read(&mut self.result_bytes[self.result_read..])
                    .await?;
                if count == 0 {
                    return Err(io::Error::other(
                        "native guardian omitted its owned-group cleanup result",
                    ));
                }
                // Keep partial reads on the Child: cancelling a wait must not
                // discard consumed bytes or turn a failed payload into success.
                self.result_read += count;
            }
            let raw = i32::from_le_bytes(self.result_bytes);
            if raw == LAUNCH_FAILED {
                if let Some(lease) = &mut self.lease {
                    lease.process_reaped();
                }
                return Err(io::Error::other(super::native_exit::LaunchNotStarted));
            }
            self.resource_stopped = raw == RESOURCE_STOPPED;
            self.reported = Some(ExitStatus::from_raw(if self.resource_stopped {
                9 // SIGKILL after the guardian confirms group cleanup.
            } else {
                raw
            }));
        }
        if let Some(lease) = &mut self.lease {
            lease.process_reaped();
        }
        Ok(self.reported.expect("complete native result"))
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        // The guardian owns the payload group. Closing this channel requests
        // cleanup even on panic or coordinator/worker death; killing the
        // guardian here could orphan a payload racing with process creation.
        drop(self.control.take());
    }
}

pub(super) async fn spawn(
    mut command: tokio::process::Command,
    startup: Startup,
    process: &ToolTaskLease,
    cancelled: &ToolCancellation,
    deadline: Instant,
) -> io::Result<Child> {
    if cancelled.is_cancelled() || Instant::now() >= deadline {
        return Err(io::Error::other("native startup stopped"));
    }
    let mut lease = process.fork();
    command.kill_on_drop(false);
    let inner = command.spawn()?;
    lease.process_started();
    drop(command);
    let mut child = Child {
        inner,
        control: Some(startup.control),
        lease: Some(lease),
        reported: None,
        result_bytes: [0; 4],
        result_read: 0,
        stopping: false,
        resource_stopped: false,
    };
    let mut released = false;
    let authorized = tokio::select! {
        biased;
        () = cancelled.cancelled() => Err(io::Error::other("native startup cancelled")),
        () = tokio::time::sleep_until(deadline.into()) => Err(io::Error::other("native startup timed out")),
        result = async {
            let control = child.control.as_mut().expect("owned startup control");
            control.write_all(&(startup.encoded.len() as u64).to_le_bytes()).await?;
            control.write_all(&startup.encoded).await?;
            let mut ready = [0; 4];
            control.read_exact(&mut ready).await?;
            if &ready != b"RDY!" {
                return Err(io::Error::other("invalid native preparation response"));
            }
            if cancelled.is_cancelled() || Instant::now() >= deadline {
                return Err(io::Error::other("native startup stopped"));
            }
            // One byte is the entire authorization. There is no yield
            // between its successful write and recording the release.
            control.write_all(&[1]).await?;
            released = true;
            Ok::<_, io::Error>(())
        } => result,
    };
    if let Err(error) = authorized {
        if released {
            child.start_kill()?;
        } else {
            // The trusted helper cannot fork a payload before authorization.
            // Reap this sole child directly: Darwin's killpg rejects a group
            // containing only its exited leader. A denied group signal must
            // not abandon an already-exited, unreleased helper.
            if child.inner.try_wait()?.is_none() {
                match child.inner.start_kill() {
                    Ok(()) => {}
                    Err(error)
                        if error.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) => {
                    }
                    Err(error) => return Err(error),
                }
            }
            // An unreleased helper has no payload result. Its actual process
            // exit below is the cleanup proof; do not wait for a result frame.
            drop(child.control.take());
        }
        if released {
            child.wait().await?;
        } else {
            child.inner.wait().await?;
            child
                .lease
                .as_mut()
                .expect("owned startup lease")
                .process_reaped();
        }
        return Err(error);
    }
    Ok(child)
}

#[cfg(test)]
static GROUP_KILL_DENIALS: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

fn kill_group(pid: Option<u32>) -> io::Result<()> {
    #[cfg(test)]
    if pid.is_some_and(|pid| GROUP_KILL_DENIALS.lock().unwrap().contains(&pid)) {
        return Err(rustix::io::Errno::PERM.into());
    }
    let Some(pid) = pid
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(rustix::process::Pid::from_raw)
    else {
        return Ok(());
    };
    match rustix::process::kill_process_group(pid, rustix::process::Signal::KILL) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(error) => Err(error.into()),
    }
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
fn validate_resources(filesystem: &NativeFilesystem, resources: NativeResources) -> io::Result<()> {
    resources.validate()?;
    match resources {
        NativeResources::Host => Ok(()),
        NativeResources::Guarded { .. } if filesystem.is_restricted() => Ok(()),
        NativeResources::Guarded { .. } => Err(io::Error::other(
            "resource safeguards require restricted filesystem authority",
        )),
        NativeResources::Limited { .. } | NativeResources::Committed { .. } => Err(
            io::Error::other("macOS aggregate resource enforcement is unavailable"),
        ),
    }
}

#[cfg(any(target_os = "macos", test))]
fn prepare_profile(
    filesystem: &NativeFilesystem,
    resources: NativeResources,
    network: NativeNetwork,
    pressure: impl FnOnce() -> io::Result<i32>,
) -> io::Result<String> {
    validate_resources(filesystem, resources)?;
    resources::check_pressure(resources, pressure)?;
    profile(filesystem, network)
}

// `None` means system pressure refused launch. Only `Some` may contain a
// created payload; errors here must be known to precede process creation.
#[cfg(any(target_os = "macos", test))]
fn confirm_launch<T>(
    control: &mut impl io::Write,
    attempted: io::Result<Option<T>>,
) -> io::Result<Option<T>> {
    let receipt = match attempted {
        Ok(Some(payload)) => return Ok(Some(payload)),
        Ok(None) => RESOURCE_STOPPED,
        Err(error) => {
            eprintln!("native payload did not start: {error}");
            LAUNCH_FAILED
        }
    };
    // One receipt only: retrying with a different receipt after partial
    // delivery could turn a corrupt frame into a false cleanup confirmation.
    control.write_all(&receipt.to_le_bytes())?;
    Ok(None)
}

fn validate_environment(packet: &Packet) -> io::Result<()> {
    if packet
        .environment
        .iter()
        .any(|(name, _)| name.starts_with(b"DYLD_") || name.starts_with(b"LD_"))
    {
        return Err(io::Error::other(
            "loader environment cannot execute before macOS isolation",
        ));
    }
    Ok(())
}

/// Single-purpose entry, before Tokio or any application initialization.
#[cfg(target_os = "macos")]
pub(crate) fn execute(encoded: &str) -> io::Result<u8> {
    if encoded != "qq-macos-launch:5" {
        return Err(io::Error::other("invalid native launch descriptor"));
    }
    if rustix::process::getpgrp() != rustix::process::getpid() {
        return Err(io::Error::other(
            "native helper requires its own process group",
        ));
    }
    let mut control = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fd/3")?;
    rustix::io::fcntl_setfd(&control, rustix::io::FdFlags::CLOEXEC)?;
    let keep = control.as_raw_fd();
    // Only this freshly opened CLOEXEC duplicate has a Rust owner. Collect and
    // drop the directory iterator before closing all inherited descriptors.
    let inherited = std::fs::read_dir("/dev/fd")?
        .map(|entry| {
            entry?
                .file_name()
                .to_str()
                .ok_or_else(|| io::Error::other("invalid descriptor"))?
                .parse::<i32>()
                .map_err(io::Error::other)
        })
        .take(65_537)
        .collect::<io::Result<Vec<_>>>()?;
    if inherited.len() > 65_536 {
        return Err(io::Error::other("inherited descriptor bound exceeded"));
    }
    for fd in inherited {
        if fd > 2 && fd != keep {
            match nix::unistd::close(fd) {
                Ok(()) | Err(nix::errno::Errno::EBADF) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    let mut length = [0; 8];
    control.read_exact(&mut length)?;
    let length = usize::try_from(u64::from_le_bytes(length)).map_err(io::Error::other)?;
    if length > MAX_PACKET {
        return Err(io::Error::other("oversized native launch packet"));
    }
    let mut encoded = vec![0; length];
    control.read_exact(&mut encoded)?;
    let packet: Packet = serde_json::from_slice(&encoded).map_err(io::Error::other)?;
    validate_environment(&packet)?;
    if unix_ms() >= packet.not_after_ms {
        return Err(io::Error::other("native preparation expired"));
    }
    let cwd = crate::workspace::Workspace::open(&packet.cwd)?;
    if NativeWorkspaceIdentity::held(cwd.root())? != packet.cwd_identity {
        return Err(io::Error::other("native working directory changed"));
    }
    let profile = prepare_profile(
        &packet.filesystem,
        packet.resources,
        packet.network,
        resources::memory_pressure_level,
    )?;
    rustix::process::fchdir(cwd.root())?;
    control.write_all(b"RDY!")?;
    // Every failure in this phase proves that no payload was created. Keep
    // that proof separate from failures after spawn, which retain custody.
    let attempted = (|| {
        let mut authorization = [0];
        control.read_exact(&mut authorization)?;
        if authorization != [1] || unix_ms() >= packet.not_after_ms {
            return Err(io::Error::other("native launch authorization expired"));
        }
        let liveness: std::os::fd::OwnedFd = control.try_clone()?.into();
        rustix::io::fcntl_setfd(&liveness, rustix::io::FdFlags::CLOEXEC)?;
        let mut liveness = std::os::unix::net::UnixStream::from(liveness);
        let guarded = matches!(packet.resources, NativeResources::Guarded { .. });
        if guarded {
            liveness.set_read_timeout(Some(std::time::Duration::from_millis(250)))?;
        }
        let policy = packet.resources;
        let custody = std::sync::Arc::new(PayloadGroup::default());
        let stop = custody.clone();
        std::thread::Builder::new()
            .name("qq-owner-liveness".into())
            .spawn(move || {
                let mut byte = [0];
                loop {
                    match liveness.read(&mut byte) {
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error)
                            if guarded
                                && matches!(
                                    error.kind(),
                                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                                ) =>
                        {
                            stop.check_resources(policy);
                        }
                        _ => {
                            stop.request_stop();
                            return;
                        }
                    }
                }
            })?;
        let mut command = if guarded {
            let mut command = std::process::Command::new("/usr/sbin/taskpolicy");
            command.args(["-b", SANDBOX_EXEC]);
            command
        } else {
            std::process::Command::new(SANDBOX_EXEC)
        };
        command
            .env_clear()
            .arg("-p")
            .arg(profile)
            .arg(OsString::from_vec(packet.program))
            .args(packet.arguments.into_iter().map(OsString::from_vec))
            .envs(
                packet
                    .environment
                    .into_iter()
                    .map(|(name, value)| (OsString::from_vec(name), OsString::from_vec(value))),
            );
        // Keep this trusted guardian outside the payload group it must terminate.
        // Seatbelt authority is inherited even when a descendant leaves this group;
        // process-group cleanup alone does not account for detached descendants.
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
        // Authorization may have waited since preparation. Refuse fresh pressure
        // before creating even a short-lived payload, not at its first sample.
        if resources::check_pressure(packet.resources, resources::memory_pressure_level).is_err() {
            return Ok(None);
        }
        if unix_ms() >= packet.not_after_ms {
            return Err(io::Error::other("native launch authorization expired"));
        }
        let child = command.spawn()?;
        Ok(Some((child, custody)))
    })();
    let Some((mut child, custody)) = confirm_launch(&mut control, attempted)? else {
        return Ok(0);
    };
    let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).map_err(io::Error::other)?)
        .ok_or_else(|| io::Error::other("invalid payload pid"))?;
    custody.publish(pid);
    // Observe without reaping: this leader pins the group identity while
    // cleanup catches descendants racing with the first termination signal.
    loop {
        match rustix::process::waitid(
            rustix::process::WaitId::Pid(pid),
            rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOWAIT,
        ) {
            Ok(Some(_)) => break,
            Err(rustix::io::Errno::INTR) => continue,
            result => {
                custody.disarm();
                return Err(io::Error::other(format!(
                    "payload exit observation failed: {result:?}"
                )));
            }
        }
    }
    loop {
        let _ = kill_group(Some(child.id()));
        if group_is_quiescent(pid).unwrap_or(false) {
            break;
        }
        // The observer has a bounded cleanup wait. If the OS cannot yet prove
        // group exit, this guardian retains that group after the observer leaves.
        std::thread::park_timeout(std::time::Duration::from_millis(10));
    }
    // Serialize disarming against the liveness thread before the only reap.
    // No code may signal this numeric group after its leader can be reused.
    let resource_stopped = custody.disarm();
    let status = child.wait()?;
    let raw = if resource_stopped {
        RESOURCE_STOPPED
    } else {
        status.into_raw()
    };
    let _ = control.write_all(&raw.to_le_bytes());
    Ok(0)
}

#[cfg(target_os = "macos")]
#[derive(Default)]
struct PayloadGroup(std::sync::Mutex<PayloadGroupState>);

#[cfg(target_os = "macos")]
#[derive(Default)]
struct PayloadGroupState {
    leader: Option<rustix::process::Pid>,
    stopping: bool,
    resource_stopped: bool,
}

#[cfg(target_os = "macos")]
impl PayloadGroup {
    fn request_stop(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.stopping = true;
        if let Some(pid) = state.leader {
            let _ = kill_group(Some(pid.as_raw_nonzero().get() as u32));
        }
    }

    fn publish(&self, pid: rustix::process::Pid) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.leader = Some(pid);
        if state.stopping {
            let _ = kill_group(Some(pid.as_raw_nonzero().get() as u32));
        }
    }

    fn check_resources(&self, policy: NativeResources) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(pid) = state.leader
            && resources::check(policy, pid).is_err()
        {
            state.resource_stopped = true;
            state.stopping = true;
            let _ = kill_group(Some(pid.as_raw_nonzero().get() as u32));
        }
    }

    fn disarm(&self) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.leader = None;
        state.resource_stopped
    }
}

/// Called with the original leader still waitable. Permission denial alone is
/// never proof: inspect the kernel's complete group list, including zombies.
#[cfg(target_os = "macos")]
pub(super) fn group_is_quiescent(pgid: rustix::process::Pid) -> io::Result<bool> {
    use libproc::{
        bsd_info::BSDInfo,
        proc_pid::pidinfo,
        processes::{ProcFilter, pids_by_type},
    };
    let group = u32::try_from(pgid.as_raw_nonzero().get()).map_err(io::Error::other)?;
    let filter = ProcFilter::ByProgramGroup { pgrpid: group };
    let members = pids_by_type(filter)?;
    if members.len() >= members.capacity() || members.len() > 4096 {
        return Err(io::Error::other(
            "native group enumeration exceeded its bound",
        ));
    }
    if !members.contains(&group) {
        return Ok(false); // The original waitable leader must pin this group.
    }
    let mut stopped = Vec::with_capacity(members.len());
    for pid in members {
        let native_pid = i32::try_from(pid).map_err(io::Error::other)?;
        // Darwin's nonzero argument explicitly includes exited processes.
        let Ok(info) = pidinfo::<BSDInfo>(native_pid, 1) else {
            return Ok(false);
        };
        if info.pbi_pgid != group || info.pbi_status != nix::libc::SZOMB {
            return Ok(false);
        }
        stopped.push((pid, info.pbi_start_tvsec, info.pbi_start_tvusec));
    }
    stopped.sort_unstable();
    // A member may finish a fork between enumeration and its exit observation.
    // Once every observed identity is exited, take another complete snapshot:
    // any new/reused identity requires another termination pass. Exited
    // identities cannot fork again, so this closes the creation race.
    let after = pids_by_type(filter)?;
    if after.len() >= after.capacity() || after.len() > 4096 || !after.contains(&group) {
        return Ok(false);
    }
    for pid in after {
        let Ok(index) = stopped.binary_search_by_key(&pid, |entry| entry.0) else {
            return Ok(false);
        };
        let native_pid = i32::try_from(pid).map_err(io::Error::other)?;
        let Ok(info) = pidinfo::<BSDInfo>(native_pid, 1) else {
            return Ok(false);
        };
        if info.pbi_pgid != group
            || info.pbi_status != nix::libc::SZOMB
            || stopped[index] != (pid, info.pbi_start_tvsec, info.pbi_start_tvusec)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn literal_read(
    profile: &mut String,
    path: &Path,
    executable: bool,
    metadata: &mut std::collections::BTreeSet<PathBuf>,
) -> io::Result<()> {
    let quote = |path: &Path| {
        let path = path
            .to_str()
            .filter(|path| !path.chars().any(char::is_control))
            .ok_or_else(|| io::Error::other("invalid native library pathname"))?;
        serde_json::to_string(path).map_err(io::Error::other)
    };
    profile.push_str(&format!(
        "(allow file-read*{} (literal {}))\n",
        if executable {
            " file-map-executable"
        } else {
            ""
        },
        quote(path)?
    ));
    for ancestor in path.ancestors().skip(1) {
        if metadata.insert(ancestor.to_owned()) {
            profile.push_str(&format!(
                "(allow file-read-metadata (literal {}))\n",
                quote(ancestor)?
            ));
        }
    }
    if profile.len() > 64 * 1024 {
        return Err(io::Error::other("native sandbox profile exceeds its bound"));
    }
    Ok(())
}

fn homebrew_library_aliases(
    directory: &Path,
    prefix: &Path,
    remaining: &mut usize,
) -> io::Result<Vec<PathBuf>> {
    let Ok(suffix) = directory.strip_prefix(prefix.join("Cellar")) else {
        return Ok(Vec::new());
    };
    let parts = suffix.iter().collect::<Vec<_>>();
    if parts.len() < 3 || parts[2] != "lib" {
        return Ok(Vec::new());
    }
    let mut alias_directory = prefix.join("opt").join(parts[0]);
    for part in &parts[2..] {
        alias_directory.push(part);
    }
    if directory.canonicalize()? != directory || alias_directory.canonicalize()? != directory {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "installed library alias changed",
        ));
    }
    let mut aliases = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        *remaining = remaining
            .checked_sub(1)
            .ok_or_else(|| io::Error::other("too many installed library entries"))?;
        let entry = entry?;
        if entry
            .path()
            .extension()
            .is_none_or(|extension| extension != "dylib")
        {
            continue;
        }
        let target = entry.path().canonicalize()?;
        let alias = alias_directory.join(entry.file_name());
        if !target.starts_with(directory) || !target.is_file() || alias.canonicalize()? != target {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "installed library file alias changed",
            ));
        }
        aliases.push(alias);
    }
    Ok(aliases)
}

fn homebrew_openssl_config(directory: &Path, prefix: &Path) -> io::Result<Option<PathBuf>> {
    let Ok(suffix) = directory.strip_prefix(prefix.join("Cellar")) else {
        return Ok(None);
    };
    let parts = suffix.iter().collect::<Vec<_>>();
    if parts.len() < 3 || parts[0] != "openssl@3" || parts[2] != "lib" {
        return Ok(None);
    }
    let config = prefix.join("etc/openssl@3/openssl.cnf");
    let metadata = std::fs::symlink_metadata(&config)?;
    if config.canonicalize()? != config || !metadata.is_file() || metadata.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "installed OpenSSL configuration is aliased",
        ));
    }
    Ok(Some(config))
}

fn profile(filesystem: &NativeFilesystem, network: NativeNetwork) -> io::Result<String> {
    let mut profile = String::from(
        "(version 1)\n(deny default)\n(allow process-fork)\n(allow signal (target same-sandbox))\n(deny syscall-unix (syscall-number SYS_setsid SYS_setpgid))\n(allow file-read-data file-write-data (require-all (literal \"/dev/null\") (vnode-type CHARACTER-DEVICE)))\n(allow sysctl-read (sysctl-name-prefix \"hw.\") (sysctl-name \"kern.osrelease\") (sysctl-name \"kern.ostype\") (sysctl-name \"kern.osversion\") (sysctl-name \"kern.osproductversion\") (sysctl-name \"kern.argmax\"))\n",
    );
    // The system /bin/sh shim reads this root-owned selector before launching
    // the selected system shell. This grants one file, not its parent tree.
    profile.push_str("(allow file-read* (literal \"/private/var/select/sh\"))\n");
    let mut literal_metadata = std::collections::BTreeSet::new();
    // LibreSSL loads this OS-public configuration even for Cargo offline work.
    // User configuration and other files under /private/etc remain excluded.
    literal_read(
        &mut profile,
        Path::new("/private/etc/ssl/openssl.cnf"),
        false,
        &mut literal_metadata,
    )?;
    for selector in [
        "/var/select/developer_dir",
        "/private/var/select/developer_dir",
    ] {
        literal_read(
            &mut profile,
            Path::new(selector),
            false,
            &mut literal_metadata,
        )?;
    }
    // dyld opens the root directory while resolving its shared cache. This
    // grants directory entries only, never reads of descendant file contents.
    profile
        .push_str("(allow file-read-data (require-all (literal \"/\") (vnode-type DIRECTORY)))\n");
    if network == NativeNetwork::Host {
        profile.push_str("(allow network*)\n");
    }
    match filesystem {
        NativeFilesystem::Host => profile.push_str("(allow file* process-exec)\n"),
        NativeFilesystem::Restricted {
            read_only,
            read_write,
        } => {
            filesystem.validate()?;
            let mut protected_directories = std::collections::BTreeSet::new();
            let mut library_entries = 4096;
            for (grant, writable) in read_only
                .iter()
                .map(|grant| (grant, false))
                .chain(read_write.iter().map(|grant| (grant, true)))
            {
                if NativeWorkspaceIdentity::capture(&grant.path)? != grant.identity {
                    return Err(io::Error::other("native directory grant changed"));
                }
                let path = grant
                    .path
                    .to_str()
                    .ok_or_else(|| io::Error::other("non UTF-8 native grant"))?;
                if path.chars().any(char::is_control) {
                    return Err(io::Error::other("control character in native grant"));
                }
                let quoted = serde_json::to_string(path).map_err(io::Error::other)?;
                profile.push_str(&format!(
                    "(allow file-read* file-map-executable process-exec {} (subpath {quoted}))\n",
                    if writable { "file-write*" } else { "" }
                ));
                if writable {
                    // Moving an ancestor also relocates the authority root.
                    // Unrelated directories and grant contents stay writable.
                    protected_directories.extend(grant.path.ancestors());
                }
                for ancestor in grant.path.ancestors().skip(1) {
                    let quoted = serde_json::to_string(
                        ancestor
                            .to_str()
                            .ok_or_else(|| io::Error::other("non UTF-8 native grant"))?,
                    )
                    .map_err(io::Error::other)?;
                    profile.push_str(&format!("(allow file-read-metadata (literal {quoted}))\n"));
                    if profile.len() > 64 * 1024 {
                        return Err(io::Error::other("native sandbox profile exceeds its bound"));
                    }
                }
                if !writable {
                    for prefix in [Path::new("/opt/homebrew"), Path::new("/usr/local")] {
                        if let Some(config) = homebrew_openssl_config(&grant.path, prefix)? {
                            literal_read(&mut profile, &config, false, &mut literal_metadata)?;
                        }
                        for alias in
                            homebrew_library_aliases(&grant.path, prefix, &mut library_entries)?
                        {
                            literal_read(&mut profile, &alias, true, &mut literal_metadata)?;
                        }
                    }
                }
            }
            // Emit denials after every allow so overlapping grants cannot
            // restore permission to replace an authority root or its ancestors.
            for directory in protected_directories {
                let quoted = serde_json::to_string(
                    directory
                        .to_str()
                        .ok_or_else(|| io::Error::other("non UTF-8 native grant"))?,
                )
                .map_err(io::Error::other)?;
                profile.push_str(&format!(
                    "(deny file-write-unlink (require-all (vnode-type DIRECTORY) (literal {quoted})))\n"
                ));
                if profile.len() > 64 * 1024 {
                    return Err(io::Error::other("native sandbox profile exceeds its bound"));
                }
            }
        }
    }
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loader_literals_preserve_parent_content_denial() {
        let text = profile(
            &NativeFilesystem::Restricted {
                read_only: vec![],
                read_write: vec![],
            },
            NativeNetwork::Isolated,
        )
        .unwrap();
        assert!(text.contains("(allow file-read* (literal \"/private/etc/ssl/openssl.cnf\"))"));
        assert!(text.contains("(allow file-read* (literal \"/var/select/developer_dir\"))"));
        assert!(
            text.contains("(allow file-read* (literal \"/private/var/select/developer_dir\"))")
        );
        assert!(!text.contains("(subpath \"/private/var/select\")"));
        for parent in ["/private/etc/ssl", "/private/etc", "/private"] {
            assert!(text.contains(&format!("(allow file-read-metadata (literal {parent:?}))")));
            assert!(!text.contains(&format!("(allow file-read* (literal {parent:?}))")));
            assert!(!text.contains(&format!("(subpath {parent:?})")));
        }
        let mut literal = String::new();
        literal_read(
            &mut literal,
            Path::new("/owned/opt/libuv/lib/libuv.1.dylib"),
            true,
            &mut Default::default(),
        )
        .unwrap();
        assert!(literal.contains("(allow file-read* file-map-executable (literal \"/owned/opt/libuv/lib/libuv.1.dylib\"))"));
        assert!(literal.contains("(allow file-read-metadata (literal \"/owned/opt/libuv/lib\"))"));
        assert!(!literal.contains("(subpath "));
    }

    #[test]
    fn homebrew_aliases_require_the_admitted_library_target() {
        let temporary = tempfile::tempdir().unwrap();
        let prefix = temporary.path().canonicalize().unwrap();
        let directory = prefix.join("Cellar/libuv/1/lib");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::create_dir(prefix.join("opt")).unwrap();
        std::fs::write(directory.join("libuv.1.dylib"), b"library").unwrap();
        std::os::unix::fs::symlink("libuv.1.dylib", directory.join("libuv.dylib")).unwrap();
        let alias = prefix.join("opt/libuv");
        std::os::unix::fs::symlink(directory.parent().unwrap(), &alias).unwrap();
        let aliases = homebrew_library_aliases(&directory, &prefix, &mut 8).unwrap();
        assert_eq!(aliases.len(), 2);
        assert!(aliases.contains(&alias.join("lib/libuv.1.dylib")));
        assert!(aliases.contains(&alias.join("lib/libuv.dylib")));
        assert!(homebrew_library_aliases(&directory, &prefix, &mut 0).is_err());
        let outside = prefix.join("outside.dylib");
        std::fs::write(&outside, b"private").unwrap();
        let escape = directory.join("escape.dylib");
        std::os::unix::fs::symlink(&outside, &escape).unwrap();
        assert!(homebrew_library_aliases(&directory, &prefix, &mut 8).is_err());
        assert_eq!(std::fs::read(outside).unwrap(), b"private");
        std::fs::remove_file(escape).unwrap();
        std::fs::remove_file(&alias).unwrap();
        assert!(homebrew_library_aliases(&directory, &prefix, &mut 8).is_err());
        let private = prefix.join("private");
        std::fs::create_dir_all(private.join("lib")).unwrap();
        std::fs::write(private.join("lib/credential"), b"private").unwrap();
        std::os::unix::fs::symlink(&private, &alias).unwrap();
        assert_eq!(
            homebrew_library_aliases(&directory, &prefix, &mut 8)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            std::fs::read(private.join("lib/credential")).unwrap(),
            b"private"
        );
    }

    #[test]
    fn homebrew_openssl_config_excludes_private_aliases() {
        let temporary = tempfile::tempdir().unwrap();
        let prefix = temporary.path().canonicalize().unwrap();
        let directory = prefix.join("Cellar/openssl@3/3/lib");
        let config = prefix.join("etc/openssl@3/openssl.cnf");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, b"installation configuration").unwrap();
        assert_eq!(
            homebrew_openssl_config(&directory, &prefix).unwrap(),
            Some(config.clone())
        );
        assert!(
            homebrew_openssl_config(&prefix.join("Cellar/private/1/lib"), &prefix)
                .unwrap()
                .is_none()
        );
        let private = prefix.join("credential");
        std::fs::write(&private, b"private").unwrap();
        std::fs::remove_file(&config).unwrap();
        std::os::unix::fs::symlink(&private, &config).unwrap();
        assert!(homebrew_openssl_config(&directory, &prefix).is_err());
        std::fs::remove_file(&config).unwrap();
        std::fs::hard_link(&private, &config).unwrap();
        assert!(homebrew_openssl_config(&directory, &prefix).is_err());
        assert_eq!(std::fs::read(private).unwrap(), b"private");
    }
    #[tokio::test]
    async fn resource_admission_requires_restricted_filesystem_authority() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let cwd = cap_std::fs::Dir::open_ambient_dir(&root, cap_std::ambient_authority()).unwrap();
        let owner = MacosOwner::discover(CommandLauncher::new("/bin/sh".into())).unwrap();
        let restricted = NativeFilesystem::Restricted {
            read_only: vec![],
            read_write: vec![crate::command_owner::NativeDirectoryGrant::capture(&root).unwrap()],
        };
        let guarded = NativeResources::Guarded {
            memory_bytes: 1024,
            max_tasks: 2,
        };
        for (filesystem, resources, admitted) in [
            (NativeFilesystem::Host, NativeResources::Host, true),
            (NativeFilesystem::Host, guarded, false),
            (restricted.clone(), guarded, true),
            (
                restricted.clone(),
                NativeResources::Committed {
                    memory_bytes: 1024,
                    max_processes: 2,
                    cpu_percent: 50,
                },
                false,
            ),
            (
                restricted,
                NativeResources::Guarded {
                    memory_bytes: 0,
                    max_tasks: 2,
                },
                false,
            ),
        ] {
            let mut command = tokio::process::Command::new("/bin/sh");
            command.current_dir(&root);
            let result = owner.wrap(
                command,
                &cwd,
                Some(&NativeExecution {
                    environment: crate::command_owner::NativeEnvironment::from_values([]).unwrap(),
                    filesystem,
                    network: NativeNetwork::Isolated,
                    resources,
                }),
                Instant::now() + std::time::Duration::from_secs(60),
            );
            assert_eq!(
                result.is_ok(),
                admitted,
                "{resources:?}: {:?}",
                result.err()
            );
        }
    }

    #[tokio::test]
    async fn writable_launcher_cannot_replace_the_trusted_helper() {
        use std::os::unix::fs::PermissionsExt as _;

        for attack in ["replace", "symlink", "hardlink", "ancestor"] {
            let root = tempfile::tempdir().unwrap();
            let root_path = root.path().canonicalize().unwrap();
            let task = root_path.join("task");
            std::fs::create_dir(&task).unwrap();
            let source = root_path.join("original");
            std::fs::write(&source, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o700)).unwrap();
            let executable = task.join("helper");
            if attack == "symlink" {
                std::os::unix::fs::symlink(&source, &executable).unwrap();
            } else if attack == "hardlink" {
                std::fs::hard_link(&source, &executable).unwrap();
            } else {
                std::fs::copy(&source, &executable).unwrap();
            }
            let owner = MacosOwner::discover(CommandLauncher::new(executable.clone())).unwrap();
            let escaped = root_path.join("escaped");
            // Replacing the launcher runs before the sandbox or readiness
            // handshake, so even a helper that refuses startup can escape.
            if attack == "ancestor" {
                std::fs::rename(&task, root_path.join("old-task")).unwrap();
                std::fs::create_dir(&task).unwrap();
            } else if attack != "hardlink" {
                std::fs::remove_file(&executable).unwrap();
            }
            std::fs::write(
                &executable,
                format!(
                    "#!/bin/sh\nprintf escaped > '{}'\nexit 1\n",
                    escaped.display().to_string().replace('\'', "'\\''")
                ),
            )
            .unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
            let mut native = NativeExecution {
                environment: crate::command_owner::NativeEnvironment::from_values([]).unwrap(),
                filesystem: NativeFilesystem::Restricted {
                    read_only: vec![],
                    read_write: vec![
                        crate::command_owner::NativeDirectoryGrant::capture(&task).unwrap(),
                    ],
                },
                network: NativeNetwork::Isolated,
                resources: NativeResources::Host,
            };
            let cwd =
                cap_std::fs::Dir::open_ambient_dir(&task, cap_std::ambient_authority()).unwrap();
            let mut payload = tokio::process::Command::new("/bin/sh");
            payload.arg("-c").arg("exit 0").current_dir(&task);
            let (mut command, _startup) = owner
                .wrap(
                    payload,
                    &cwd,
                    Some(&native),
                    Instant::now() + std::time::Duration::from_secs(60),
                )
                .expect("an isolated task must remain launchable");
            command.kill_on_drop(true);
            let status = tokio::time::timeout(std::time::Duration::from_secs(5), command.status())
                .await
                .unwrap()
                .unwrap();
            assert!(
                status.success(),
                "{attack}: the captured helper must execute"
            );
            assert!(
                !escaped.exists(),
                "{attack}: a replacement helper ran outside the sandbox"
            );
            native.filesystem = NativeFilesystem::Restricted {
                read_only: vec![],
                read_write: vec![
                    crate::command_owner::NativeDirectoryGrant::capture(
                        owner.launcher.executable.parent().unwrap(),
                    )
                    .unwrap(),
                ],
            };
            let mut payload = tokio::process::Command::new("/bin/sh");
            payload.current_dir(&task);
            let error = owner
                .wrap(
                    payload,
                    &cwd,
                    Some(&native),
                    Instant::now() + std::time::Duration::from_secs(60),
                )
                .err()
                .expect("the private helper must never enter a writable grant");
            assert!(error.to_string().contains("writable authority includes"));
        }
    }

    #[tokio::test]
    async fn interrupted_result_reads_retain_bytes_and_require_the_cleanup_frame() {
        for (complete, raw_status) in [
            (true, 7_i32 << 8),
            (false, 7_i32 << 8),
            (true, RESOURCE_STOPPED),
        ] {
            let mut inner = tokio::process::Command::new("/bin/sh")
                .args(["-c", "exit 0"])
                .spawn()
                .unwrap();
            assert!(inner.wait().await.unwrap().success());
            let (control, mut peer) = tokio::net::UnixStream::pair().unwrap();
            let tasks = crate::tools::dispatch::ToolTasks::default();
            let mut lease = tasks.enter();
            lease.process_started();
            let mut child = Child {
                inner,
                control: Some(control),
                lease: Some(lease),
                reported: None,
                result_bytes: [0; 4],
                result_read: 0,
                stopping: false,
                resource_stopped: false,
            };
            let raw = raw_status.to_le_bytes();
            peer.write_all(&raw[..2]).await.unwrap();
            child.control.as_ref().unwrap().readable().await.unwrap();
            let mut waiting = Box::pin(child.wait());
            assert!(futures_util::poll!(waiting.as_mut()).is_pending());
            drop(waiting);
            assert_eq!(child.result_read, 2);
            if complete {
                peer.write_all(&raw[2..]).await.unwrap();
                let status = child.wait().await.unwrap();
                if raw_status == RESOURCE_STOPPED {
                    assert_eq!(status.signal(), Some(9));
                    assert!(child.resource_stopped());
                } else {
                    assert_eq!(status.code(), Some(7));
                    assert!(!child.resource_stopped());
                }
            } else {
                drop(peer);
                assert!(
                    child.wait().await.is_err(),
                    "an exit without a complete frame is not cleanup proof"
                );
            }
            drop(child);
            assert_eq!(tasks.drain().await.is_ok(), complete);
        }
    }

    #[tokio::test]
    async fn helper_preparation_waits_for_readiness_and_remains_cancellable() {
        for authorize in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let (control, inherited) = std::os::unix::net::UnixStream::pair().unwrap();
            control.set_nonblocking(true).unwrap();
            let (gate, held) = rustix::pipe::pipe().unwrap();
            let (read, write) = rustix::pipe::pipe().unwrap();
            let mut ready = tokio::fs::File::from_std(std::fs::File::from(read));
            let mut command = tokio::process::Command::new("bash");
            command.args(["-c", "dd bs=1 count=9 <&3 >/dev/null 2>&1; printf ready; IFS= read -r -n 1 gate <&4; printf RDY! >&3; IFS= read -r -n 1 go <&3; printf executed > escaped; printf '\\000\\000\\000\\000' >&3"])
            .current_dir(root.path()).process_group(0).kill_on_drop(true)
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::from(write)).stderr(std::process::Stdio::inherit());
            command
                .as_std_mut()
                .fd_mappings(vec![
                    command_fds::FdMapping {
                        parent_fd: inherited.into(),
                        child_fd: 3,
                    },
                    command_fds::FdMapping {
                        parent_fd: gate,
                        child_fd: 4,
                    },
                ])
                .unwrap();
            let startup = Startup {
                encoded: vec![b'J'],
                control: tokio::net::UnixStream::from_std(control).unwrap(),
                _image: std::sync::Arc::new(HelperImage(Some(tempfile::tempdir().unwrap()))),
            };
            let tasks = crate::tools::dispatch::ToolTasks::default();
            let lease = tasks.enter();
            let cancellation = crate::RunCancellation::new();
            let cancelled = ToolCancellation::new(cancellation.clone());
            let execution = tokio::spawn(async move {
                spawn(
                    command,
                    startup,
                    &lease,
                    &cancelled,
                    Instant::now() + std::time::Duration::from_secs(60),
                )
                .await
            });
            let mut marker = [0; 5];
            let preparing = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                ready.read_exact(&mut marker),
            )
            .await;
            if authorize {
                std::fs::File::from(held).write_all(b"1").unwrap();
            } else {
                cancellation.cancel();
                drop(held);
            }
            let result = tokio::time::timeout(std::time::Duration::from_secs(5), execution)
                .await
                .unwrap()
                .unwrap();
            let stopped = result.is_err();
            let diagnostic = result.as_ref().err().map(ToString::to_string);
            if let Ok(mut child) = result {
                if !authorize {
                    child.start_kill().unwrap();
                }
                assert!(child.wait().await.unwrap().success() || !authorize);
            }
            assert!(
                matches!(preparing, Ok(Ok(_))),
                "helper preparation marker failed: {preparing:?}; startup: {diagnostic:?}"
            );
            assert_eq!(&marker, b"ready");
            tasks.drain().await.unwrap();
            assert_eq!(
                stopped, !authorize,
                "only authorized startup may finish: {diagnostic:?}"
            );
            assert_eq!(root.path().join("escaped").exists(), authorize);
        }
    }

    #[tokio::test]
    async fn unreleased_exited_helper_is_reaped_when_group_kill_would_be_denied() {
        // Darwin returns EPERM for an existing group containing only zombies.
        // Keep its exited leader waitable, and reproduce that kernel response
        // on Linux too so this startup cleanup regression is deterministic.
        struct DeniedGroup(u32);
        impl Drop for DeniedGroup {
            fn drop(&mut self) {
                GROUP_KILL_DENIALS
                    .lock()
                    .unwrap()
                    .retain(|pid| *pid != self.0);
            }
        }
        let (control, inherited) = std::os::unix::net::UnixStream::pair().unwrap();
        control.set_nonblocking(true).unwrap();
        // This peer prevents EOF until the fixture has observed the helper's
        // exit. It is never inherited by the helper or any payload.
        let held_peer = inherited.try_clone().unwrap();
        let mut command = tokio::process::Command::new("bash");
        command
            .args(["-c", "printf '%s\n' \"$$\"; exit 7"])
            .process_group(0)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        command
            .as_std_mut()
            .fd_mappings(vec![command_fds::FdMapping {
                parent_fd: inherited.into(),
                child_fd: 3,
            }])
            .unwrap();
        // Capture the helper PID without polling/reaping the Child owned by
        // spawn. Use a separate output pipe because startup owns stdio.
        let (read, write) = rustix::pipe::pipe().unwrap();
        command.stdout(std::process::Stdio::from(write));
        let mut pid_output =
            tokio::io::BufReader::new(tokio::fs::File::from_std(std::fs::File::from(read)));
        let startup = Startup {
            encoded: vec![b'J'],
            control: tokio::net::UnixStream::from_std(control).unwrap(),
            _image: std::sync::Arc::new(HelperImage(Some(tempfile::tempdir().unwrap()))),
        };
        let tasks = crate::tools::dispatch::ToolTasks::default();
        let lease = tasks.enter();
        let cancelled = ToolCancellation::new(crate::RunCancellation::new());
        let execution = tokio::spawn(async move {
            spawn(
                command,
                startup,
                &lease,
                &cancelled,
                Instant::now() + std::time::Duration::from_secs(15),
            )
            .await
        });
        let mut text = String::new();
        use tokio::io::AsyncBufReadExt as _;
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            pid_output.read_line(&mut text),
        )
        .await
        .unwrap()
        .unwrap();
        let pid: u32 = text.trim().parse().unwrap();
        tokio::task::spawn_blocking(move || {
            let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
            let status = rustix::process::waitid(
                rustix::process::WaitId::Pid(pid),
                rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOWAIT,
            )
            .unwrap();
            assert!(
                status.is_some(),
                "the helper must have exited before startup fails"
            );
        })
        .await
        .unwrap();
        #[cfg(target_os = "linux")]
        GROUP_KILL_DENIALS.lock().unwrap().push(pid);
        #[cfg(target_os = "macos")]
        {
            let group = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
            assert_eq!(
                rustix::process::test_kill_process_group(group),
                Err(rustix::io::Errno::PERM)
            );
        }
        let denied = DeniedGroup(pid);
        drop(held_peer);
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), execution)
            .await
            .unwrap()
            .unwrap();
        let drained = tasks.drain().await;
        drop(denied);
        // Reap only this fixture if the broken path abandoned its Child. This
        // cannot change the already-recorded production cleanup verdict.
        crate::tools::assert_panicked_process_exits(pid).await;
        assert!(
            result.is_err(),
            "an exited, unauthorized helper must remain a refused launch"
        );
        assert_eq!(
            drained,
            Ok(()),
            "the sole unreleased helper must be reaped even when killpg cannot signal its zombie"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn writable_grant_anchors_survive_content_edits_and_replacement_attempts() {
        use crate::command_owner::NativeDirectoryGrant;

        for nested_first in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let workspace = root.join("workspace");
            let intermediate = workspace.join("intermediate");
            let anchor = intermediate.join("anchor");
            std::fs::create_dir_all(&anchor).unwrap();
            let outside = root.join("outside");
            std::fs::write(&outside, "outside-original").unwrap();
            let read_only = ["/bin", "/usr", "/System/Library", "/System/Cryptexes/OS"]
                .into_iter()
                .map(|path| {
                    NativeDirectoryGrant::capture(&std::fs::canonicalize(path).unwrap()).unwrap()
                })
                .collect();
            let mut read_write = vec![
                NativeDirectoryGrant::capture(&workspace).unwrap(),
                NativeDirectoryGrant::capture(&anchor).unwrap(),
            ];
            if nested_first {
                read_write.reverse();
            }
            let policy = profile(
                &NativeFilesystem::Restricted {
                    read_only,
                    // The enclosing grant would otherwise permit replacing the
                    // nested anchor even though its identity was checked at launch.
                    read_write,
                },
                NativeNetwork::Isolated,
            )
            .unwrap();
            let output = std::process::Command::new(SANDBOX_EXEC)
                .args(["-p", &policy, "/bin/sh", "-c"])
                .arg(
                    r#"set -eu
printf original > "$1/keep"
printf editable > "$1/edit"
/bin/mv "$1/edit" "$1/renamed"
/bin/rm "$1/renamed"
/bin/mkdir "$1/ordinary"
/bin/rmdir "$1/ordinary"
/bin/mkdir ordinary
/bin/mv ordinary ordinary-renamed
/bin/rmdir ordinary-renamed
if /bin/mv "$3" "$3-moved"; then printf ancestor-relocated; exit 94; fi
if /bin/mv "$1" "$1-moved"; then exit 91; fi
/bin/rm "$1/keep"
if /bin/rmdir "$1"; then exit 92; fi
printf retained > "$1/kept"
if printf changed > "$2"; then exit 93; fi
printf anchored"#,
                )
                .arg("anchor-probe")
                .arg(&anchor)
                .arg(&outside)
                .arg(&intermediate)
                .current_dir(&workspace)
                .env_clear()
                .output()
                .unwrap();
            assert_ne!(
                output.stdout, b"ancestor-relocated",
                "an intermediate directory carrying a grant root was relocated (nested_first={nested_first})"
            );
            assert!(
                output.status.success(),
                "status={:?}, stdout={}, stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"anchored");
            assert_eq!(
                std::fs::read_to_string(anchor.join("kept")).unwrap(),
                "retained"
            );
            assert!(!intermediate.join("anchor-moved").exists());
            assert!(!workspace.join("intermediate-moved").exists());
            assert_eq!(
                std::fs::read_to_string(outside).unwrap(),
                "outside-original"
            );
        }
    }

    #[test]
    fn replaced_directory_is_refused_before_profile_construction() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().canonicalize().unwrap().join("task");
        std::fs::create_dir(&path).unwrap();
        let grant = crate::command_owner::NativeDirectoryGrant::capture(&path).unwrap();
        std::fs::rename(&path, path.with_file_name("old")).unwrap();
        std::fs::create_dir(&path).unwrap();
        let error = profile(
            &NativeFilesystem::Restricted {
                read_only: vec![],
                read_write: vec![grant],
            },
            NativeNetwork::Isolated,
        )
        .unwrap_err();
        assert!(error.to_string().contains("directory grant changed"));
    }

    #[test]
    fn guarded_preparation_checks_pressure_before_authorizing_payloads() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let filesystem = NativeFilesystem::Restricted {
            read_only: vec![],
            read_write: vec![crate::command_owner::NativeDirectoryGrant::capture(&root).unwrap()],
        };
        let guarded = NativeResources::Guarded {
            memory_bytes: 1024 * 1024,
            max_tasks: 2,
        };
        for (pressure, accepted) in [
            (Ok(1), true),
            (Ok(2), false),
            (Ok(4), false),
            (Ok(0), false),
            (Err(io::Error::other("pressure query unavailable")), false),
        ] {
            let mut queried = false;
            let result = prepare_profile(&filesystem, guarded, NativeNetwork::Isolated, || {
                queried = true;
                pressure
            });
            assert!(
                queried,
                "guarded preparation must query pressure before authorizing execution"
            );
            assert_eq!(result.is_ok(), accepted, "{result:?}");
        }
        assert!(
            prepare_profile(
                &filesystem,
                NativeResources::Host,
                NativeNetwork::Isolated,
                || panic!("host resources must not require a pressure query"),
            )
            .is_ok()
        );
    }

    struct LimitedWriter {
        stream: std::os::unix::net::UnixStream,
        remaining: usize,
    }
    impl io::Write for LimitedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "injected receipt failure",
                ));
            }
            let end = bytes.len().min(self.remaining);
            let written = io::Write::write(&mut self.stream, &bytes[..end])?;
            self.remaining -= written;
            Ok(written)
        }
        fn flush(&mut self) -> io::Result<()> {
            io::Write::flush(&mut self.stream)
        }
    }

    #[tokio::test]
    async fn post_authorization_pressure_refusal_confirms_no_payload_before_settlement() {
        for (pressure, budget) in [
            (Ok(2), usize::MAX),
            (Ok(4), usize::MAX),
            (
                Err(io::Error::other("pressure query unavailable")),
                usize::MAX,
            ),
            (Ok(2), 0),
            (Ok(4), 2),
        ] {
            let root = tempfile::tempdir().unwrap();
            let marker = root.path().join("payload-ran");
            let mut inner = tokio::process::Command::new("/bin/sh")
                .args(["-c", "exit 0"])
                .spawn()
                .unwrap();
            assert!(inner.wait().await.unwrap().success());
            let (control, stream) = std::os::unix::net::UnixStream::pair().unwrap();
            control.set_nonblocking(true).unwrap();
            let mut writer = LimitedWriter {
                stream,
                remaining: budget,
            };
            let tasks = crate::tools::dispatch::ToolTasks::default();
            let mut lease = tasks.enter();
            lease.process_started();
            let mut child = Child {
                inner,
                control: Some(tokio::net::UnixStream::from_std(control).unwrap()),
                lease: Some(lease),
                reported: None,
                result_bytes: [0; 4],
                result_read: 0,
                stopping: false,
                resource_stopped: false,
            };
            let pressure = resources::check_pressure(
                NativeResources::Guarded {
                    memory_bytes: 1024 * 1024,
                    max_tasks: 2,
                },
                || pressure,
            );
            let admitted = confirm_launch(&mut writer, Ok(pressure.ok()));
            if matches!(admitted, Ok(Some(()))) {
                std::fs::write(&marker, b"payload started").unwrap();
            }
            drop(writer);
            let result = child.wait().await;
            let resource_stopped = child.resource_stopped();
            drop(child);
            let drained = tasks.drain().await;
            assert!(
                !marker.exists(),
                "pressure refusal must never launch a payload"
            );
            let delivered = budget == usize::MAX;
            assert_eq!(admitted.is_ok(), delivered, "{admitted:?}");
            assert_eq!(result.is_ok(), delivered, "{result:?}");
            assert_eq!(resource_stopped, delivered);
            assert_eq!(drained.is_ok(), delivered, "{drained:?}");
        }
    }
    #[tokio::test]
    async fn confirmed_launch_failure_releases_custody_without_reporting_an_exit() {
        for (kind, budget, guardian_exit) in [
            (io::ErrorKind::TimedOut, usize::MAX, 0),
            (io::ErrorKind::NotFound, usize::MAX, 0),
            (io::ErrorKind::PermissionDenied, usize::MAX, 0),
            (io::ErrorKind::TimedOut, 0, 0),
            (io::ErrorKind::NotFound, 2, 0),
            (io::ErrorKind::TimedOut, usize::MAX, 1),
        ] {
            let mut inner = tokio::process::Command::new("/bin/sh")
                .args(["-c", &format!("exit {guardian_exit}")])
                .spawn()
                .unwrap();
            inner.wait().await.unwrap();
            let (control, stream) = std::os::unix::net::UnixStream::pair().unwrap();
            control.set_nonblocking(true).unwrap();
            let mut writer = LimitedWriter {
                stream,
                remaining: budget,
            };
            let tasks = crate::tools::dispatch::ToolTasks::default();
            let mut caller = tasks.enter();
            caller.process_started();
            let mut lease = caller.fork();
            lease.process_started();
            let mut child = Child {
                inner,
                control: Some(tokio::net::UnixStream::from_std(control).unwrap()),
                lease: Some(lease),
                reported: None,
                result_bytes: [0; 4],
                result_read: 0,
                stopping: false,
                resource_stopped: false,
            };
            let refused = confirm_launch::<()>(
                &mut writer,
                Err(io::Error::new(kind, "launch refused before spawn")),
            );
            assert_eq!(refused.is_ok(), budget == usize::MAX);
            drop(writer);
            let confirmed = budget == usize::MAX && guardian_exit == 0;
            let mut group = super::super::shell::ProcessGroupGuard::new(None);
            for _ in 0..2 {
                let status = child.wait().await;
                let error =
                    super::super::shell::confirm_child_exit(status, &mut group, &mut caller)
                        .await
                        .expect_err("a refused launch has no payload exit");
                assert_eq!(
                    super::super::native_exit::confirmed_launch_failure(&error),
                    confirmed
                );
                if confirmed {
                    assert!(
                        error.to_string().contains("before process creation"),
                        "{error}"
                    );
                }
                assert!(!child.resource_stopped());
            }
            drop(child);
            drop(caller);
            let drained = tasks.drain().await;
            assert_eq!(drained.is_ok(), confirmed, "{kind:?}: {drained:?}");
        }
        let mut receipt = Vec::new();
        assert_eq!(
            confirm_launch(&mut receipt, Ok(Some(42))).unwrap(),
            Some(42)
        );
        assert!(
            receipt.is_empty(),
            "a started process still needs real cleanup"
        );
    }
}
