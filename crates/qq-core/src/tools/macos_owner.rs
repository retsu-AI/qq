//! macOS launch isolation. The trusted helper retains its process-group identity
//! until it kills the group, and never gives payloads its owner-liveness channel.
use super::{
    dispatch::{ToolCancellation, ToolTaskLease},
    process_context::NativeExecution,
};
use crate::command_owner::{
    CommandLauncher, NativeFilesystem, NativeNetwork, NativeResources, NativeWorkspaceIdentity,
};
use command_fds::CommandFdExt as _;
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    io::{self, Read as _, Write as _},
    ops::{Deref, DerefMut},
    os::{
        fd::AsRawFd as _,
        unix::{
            ffi::{OsStrExt as _, OsStringExt as _},
            fs::MetadataExt as _,
            process::ExitStatusExt as _,
        },
    },
    process::ExitStatus,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const MAX_PACKET: usize = 4 * 1024 * 1024;
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

#[derive(Clone)]
pub(super) struct MacosOwner(CommandLauncher);

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
    pub(super) fn discover(launcher: CommandLauncher) -> io::Result<Self> {
        if rustix::process::getuid().is_root()
            || rustix::process::getuid() != rustix::process::geteuid()
        {
            return Err(io::Error::other(
                "macOS command isolation requires an unprivileged owner",
            ));
        }
        let metadata = launcher.executable.metadata()?;
        if !launcher.executable.is_absolute()
            || !metadata.is_file()
            || metadata.mode() & 0o111 == 0
            || metadata.mode() & 0o6022 != 0
        {
            return Err(io::Error::other(
                "native helper must be an absolute unprivileged executable",
            ));
        }
        Ok(Self(launcher))
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
        let mut wrapped = tokio::process::Command::new(&self.0.executable);
        wrapped
            .env_clear()
            .args(&self.0.arguments)
            .arg("qq-macos-launch:3");
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
            },
        ))
    }
}

pub(super) struct Startup {
    encoded: Vec<u8>,
    control: tokio::net::UnixStream,
}

pub(crate) struct Child {
    inner: tokio::process::Child,
    control: Option<tokio::net::UnixStream>,
    lease: Option<ToolTaskLease>,
    reported: Option<ExitStatus>,
}
impl From<tokio::process::Child> for Child {
    fn from(inner: tokio::process::Child) -> Self {
        Self {
            inner,
            control: None,
            lease: None,
            reported: None,
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
    pub(super) fn start_kill(&mut self) -> io::Result<()> {
        if self.control.is_some() {
            kill_group(self.inner.id())?;
        }
        self.inner.start_kill()
    }
    pub(super) async fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.inner.wait().await?;
        if let Some(mut control) = self.control.take() {
            let mut raw = [0; 4];
            match tokio::time::timeout(
                std::time::Duration::from_secs(2),
                control.read_exact(&mut raw),
            )
            .await
            {
                Ok(Ok(_)) => self.reported = Some(ExitStatus::from_raw(i32::from_le_bytes(raw))),
                Ok(Err(error)) if error.kind() == io::ErrorKind::UnexpectedEof => {}
                Ok(Err(error)) => return Err(error),
                Err(_) => return Err(io::Error::other("native exit observation did not close")),
            }
        }
        if let Some(lease) = &mut self.lease {
            lease.process_reaped();
        }
        Ok(self.reported.unwrap_or(status))
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        // The unreaped helper pins its group identity. Kill the entire group
        // before Tokio's kill-on-drop can terminate only the helper.
        if self.control.is_some() {
            let _ = kill_group(self.inner.id());
        }
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
    let inner = command.spawn()?;
    lease.process_started();
    drop(command);
    let mut child = Child {
        inner,
        control: Some(startup.control),
        lease: Some(lease),
        reported: None,
    };
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
            control.write_all(&[1]).await
        } => result,
    };
    if let Err(error) = authorized {
        child.start_kill()?;
        child.wait().await?;
        return Err(error);
    }
    Ok(child)
}

fn kill_group(pid: Option<u32>) -> io::Result<()> {
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
pub(crate) fn execute(encoded: &str) -> io::Result<u8> {
    if encoded != "qq-macos-launch:3" {
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
    std::thread::Builder::new()
        .name("qq-owner-liveness".into())
        .spawn(move || {
            let mut byte = [0];
            loop {
                match liveness.read(&mut byte) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    _ => {
                        let _ = kill_group(Some(std::process::id()));
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
    // No new group: the trusted helper remains its leader. The kernel profile
    // denies setsid/setpgid in every payload descendant.
    if unix_ms() >= packet.not_after_ms {
        return Err(io::Error::other("native launch authorization expired"));
    }
    let mut child = command.spawn()?;
    if let Ok(status) = child.wait() {
        // An observer can disappear before this write. Cleanup is mandatory
        // even when its result channel has closed.
        let _ = control.write_all(&status.into_raw().to_le_bytes());
    }
    kill_group(Some(std::process::id()))?;
    Err(io::Error::other(
        "native process-group termination returned",
    ))
}

fn profile(filesystem: &NativeFilesystem, network: NativeNetwork) -> io::Result<String> {
    let mut profile = String::from(
        "(version 1)\n(deny default)\n(allow process-exec process-fork)\n(allow signal (target same-sandbox))\n(deny syscall-unix (syscall-number SYS_setsid SYS_setpgid))\n(allow file-read-data file-write-data (require-all (literal \"/dev/null\") (vnode-type CHARACTER-DEVICE)))\n(allow sysctl-read (sysctl-name-prefix \"hw.\") (sysctl-name \"kern.osrelease\") (sysctl-name \"kern.ostype\") (sysctl-name \"kern.osversion\") (sysctl-name \"kern.osproductversion\") (sysctl-name \"kern.argmax\"))\n",
    );
    if network == NativeNetwork::Host {
        profile.push_str("(allow network*)\n");
    }
    match filesystem {
        NativeFilesystem::Host => profile.push_str("(allow file*)\n"),
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
                    "(allow file-read* {} (subpath {quoted}))\n",
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
    async fn helper_preparation_waits_for_readiness_and_remains_cancellable() {
        let root = tempfile::tempdir().unwrap();
        let (control, inherited) = std::os::unix::net::UnixStream::pair().unwrap();
        control.set_nonblocking(true).unwrap();
        let (gate, held) = rustix::pipe::pipe().unwrap();
        let (read, write) = rustix::pipe::pipe().unwrap();
        let mut ready = tokio::fs::File::from_std(std::fs::File::from(read));
        let mut command = tokio::process::Command::new("bash");
        command.args(["-c", "dd bs=1 count=9 <&3 >/dev/null 2>&1; printf ready; IFS= read -r -n 1 gate <&4; printf RDY! >&3; IFS= read -r -n 1 go <&3; printf executed > escaped"])
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
        cancellation.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), execution)
            .await
            .unwrap()
            .unwrap();
        let stopped = result.is_err();
        let diagnostic = result.as_ref().err().map(ToString::to_string);
        if let Ok(mut child) = result {
            child.start_kill().unwrap();
            child.wait().await.unwrap();
        }
        assert!(
            matches!(preparing, Ok(Ok(_))),
            "helper preparation marker failed: {preparing:?}; startup: {diagnostic:?}"
        );
        assert_eq!(&marker, b"ready");
        drop(held);
        tasks.drain().await.unwrap();
        assert!(
            stopped,
            "startup must wait for prepared helper readiness and honor cancellation while another stage owns the gate"
        );
        assert!(!root.path().join("escaped").exists());
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
