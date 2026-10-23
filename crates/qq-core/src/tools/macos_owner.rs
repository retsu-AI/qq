//! macOS launch isolation. The guardian stays outside the payload group and
//! retains its waitable leader until every member stops, even after owner loss.
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
    process::ExitStatus,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

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
        if native.is_some_and(|native| native.resources != NativeResources::Host) {
            return Err(io::Error::other(
                "macOS aggregate resource enforcement is unavailable",
            ));
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
            .arg("qq-macos-launch:4");
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
            // A timed-out guardian keeps custody; dropping this observer only
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
                "native guardian exited without confirming payload cleanup: {status}"
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
                        "native guardian omitted its cleanup result",
                    ));
                }
                // Keep partial reads on the Child: cancelling a wait must not
                // discard consumed bytes or turn a failed payload into success.
                self.result_read += count;
            }
            self.reported = Some(ExitStatus::from_raw(i32::from_le_bytes(self.result_bytes)));
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
    if encoded != "qq-macos-launch:4" {
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
    let profile = profile(&packet.filesystem, packet.network)?;
    rustix::process::fchdir(cwd.root())?;
    control.write_all(b"RDY!")?;
    let mut authorization = [0];
    control.read_exact(&mut authorization)?;
    if authorization != [1] || unix_ms() >= packet.not_after_ms {
        return Err(io::Error::other("native launch authorization expired"));
    }
    let mut liveness = control.try_clone()?;
    rustix::io::fcntl_setfd(&liveness, rustix::io::FdFlags::CLOEXEC)?;
    let custody = std::sync::Arc::new(PayloadGroup::default());
    let stop = custody.clone();
    std::thread::Builder::new()
        .name("qq-owner-liveness".into())
        .spawn(move || {
            let mut byte = [0];
            loop {
                match liveness.read(&mut byte) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    _ => {
                        stop.request_stop();
                        return;
                    }
                }
            }
        })?;
    let mut command = std::process::Command::new(SANDBOX_EXEC);
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
    // Keep this trusted guardian outside the group it must terminate. The
    // sandboxed child leads a new group and cannot change it or create another.
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
    if unix_ms() >= packet.not_after_ms {
        return Err(io::Error::other("native launch authorization expired"));
    }
    let mut child = command.spawn()?;
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
        // exit, this small guardian keeps custody after that observer leaves.
        std::thread::park_timeout(std::time::Duration::from_millis(10));
    }
    // Serialize disarming against the liveness thread before the only reap.
    // No code may signal this numeric group after its leader can be reused.
    custody.disarm();
    let status = child.wait()?;
    let _ = control.write_all(&status.into_raw().to_le_bytes());
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

    fn disarm(&self) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .leader = None;
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

fn profile(filesystem: &NativeFilesystem, network: NativeNetwork) -> io::Result<String> {
    let mut profile = String::from(
        "(version 1)\n(deny default)\n(allow process-fork)\n(allow signal (target same-sandbox))\n(deny syscall-unix (syscall-number SYS_setsid SYS_setpgid))\n(allow file-read-data file-write-data (require-all (literal \"/dev/null\") (vnode-type CHARACTER-DEVICE)))\n(allow sysctl-read (sysctl-name-prefix \"hw.\") (sysctl-name \"kern.osrelease\") (sysctl-name \"kern.ostype\") (sysctl-name \"kern.osversion\") (sysctl-name \"kern.osproductversion\") (sysctl-name \"kern.argmax\"))\n",
    );
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
            }
        }
    }
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;
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
        for complete in [true, false] {
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
            };
            let raw = (7_i32 << 8).to_le_bytes();
            peer.write_all(&raw[..2]).await.unwrap();
            child.control.as_ref().unwrap().readable().await.unwrap();
            let mut waiting = Box::pin(child.wait());
            assert!(futures_util::poll!(waiting.as_mut()).is_pending());
            drop(waiting);
            assert_eq!(child.result_read, 2);
            if complete {
                peer.write_all(&raw[2..]).await.unwrap();
                assert_eq!(child.wait().await.unwrap().code(), Some(7));
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
}
