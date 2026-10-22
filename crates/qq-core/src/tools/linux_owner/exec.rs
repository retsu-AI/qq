//! Trusted PID 1 entry point. Agent-controlled environment is applied only to
//! the final command, after the live-owner handshake.

use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{self, Read as _, Write as _},
    os::{
        fd::AsRawFd as _,
        unix::{ffi::OsStringExt as _, fs::OpenOptionsExt as _, process::CommandExt as _},
    },
    process::Stdio,
};

pub(super) const MAX_PACKET: u64 = 4 * 1024 * 1024;
pub(super) const MAX_REFUSAL_BYTES: usize = 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Packet {
    pub resource_fd: Option<i32>,
    pub filesystem: Option<super::filesystem_root::Filesystem>,
    pub isolated_network: bool,
    pub program: Vec<u8>,
    pub arguments: Vec<Vec<u8>>,
    pub environment: Vec<(Vec<u8>, Vec<u8>)>,
    pub argv0: Vec<u8>,
    pub cwd_fd: i32,
    pub image_fd: Option<i32>,
    pub namespace_program: Vec<u8>,
    pub namespace_arguments: Vec<Vec<u8>>,
    pub namespace_fd: i32,
    pub launcher_program: Vec<u8>,
    pub launcher_fd: i32,
    pub launcher_arguments: Vec<Vec<u8>>,
    pub parent_pid: u32,
}

pub(super) fn inherited(fd: i32, write: bool) -> io::Result<File> {
    let file = std::fs::OpenOptions::new()
        .read(!write)
        .write(write)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
        .open(format!("/proc/self/fd/{fd}"))?;
    // This is a single-purpose entry point before the application's runtime.
    // The descriptor was inherited without a Rust owner; the newly opened File
    // owns its CLOEXEC duplicate. Never expose the bootstrap descriptor to exec.
    nix::unistd::close(fd)?;
    let flags = rustix::fs::fcntl_getfl(&file)?;
    rustix::fs::fcntl_setfl(&file, flags & !rustix::fs::OFlags::NONBLOCK)?;
    Ok(file)
}

fn live(file: File) -> io::Result<()> {
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        return Err(io::Error::other("oversized process identity"));
    }
    let tail = bytes
        .iter()
        .rposition(|byte| *byte == b')')
        .and_then(|end| bytes.get(end + 1..))
        .ok_or_else(|| io::Error::other("invalid process identity"))?;
    let fields = tail
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    let flags = fields
        .get(6)
        .and_then(|field| std::str::from_utf8(field).ok())
        .and_then(|value| value.parse::<u64>().ok());
    if !matches!(
        fields.first().copied(),
        Some(b"R" | b"S" | b"D" | b"T" | b"t" | b"I")
    ) || flags.is_none_or(|flags| flags & 4 != 0)
    // PF_EXITING precedes parent-death notification.
    {
        return Err(io::Error::other(
            "command owner or namespace monitor has stopped",
        ));
    }
    Ok(())
}

pub(crate) fn execute(encoded: &str) -> io::Result<u8> {
    if let Some(fields) = encoded.strip_prefix("qq-native-monitor:") {
        return monitor(fields);
    }
    if rustix::process::getpid().as_raw_nonzero().get() != 1 {
        return Err(io::Error::other(
            "native launch helper requires its own PID namespace",
        ));
    }
    // Re-arm after exec, then check the original monitor while it remains
    // unreaped. A monitor that died before this call cannot authorize payloads.
    rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))?;
    let fields = encoded
        .strip_prefix("qq-native-exec:")
        .ok_or_else(|| io::Error::other("invalid native launch"))?
        .split(':')
        .map(str::parse::<i32>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(io::Error::other)?;
    let [data_fd, gate_fd, ack_fd, owner_fd, host_proc_fd] = fields.as_slice() else {
        return Err(io::Error::other("invalid native launch descriptors"));
    };
    if fields.iter().any(|fd| *fd < 3)
        || fields
            .iter()
            .enumerate()
            .any(|(index, fd)| fields[..index].contains(fd))
    {
        return Err(io::Error::other("invalid native launch descriptors"));
    }
    let mut ack = inherited(*ack_fd, true)?;
    let mut host_identity = Vec::new();
    File::open(format!("/proc/self/fd/{host_proc_fd}/self/stat"))?
        .take(4096)
        .read_to_end(&mut host_identity)?;
    let host_pid = host_identity
        .split(|byte| byte.is_ascii_whitespace())
        .next()
        .and_then(|pid| std::str::from_utf8(pid).ok())
        .and_then(|pid| pid.parse::<i32>().ok())
        .ok_or_else(|| io::Error::other("invalid namespace identity"))?;
    ack.write_all(&host_pid.to_le_bytes())?;
    let mut gate = inherited(*gate_fd, false)?;
    let mut monitor = [0; 4];
    gate.read_exact(&mut monitor)?;
    let monitor = i32::from_le_bytes(monitor);
    if monitor < 3 {
        return Err(io::Error::other("invalid namespace monitor descriptor"));
    }
    // The pinned worker directory cannot resolve to a replacement worker.
    // The monitor remains unreaped until this PID namespace has stopped. Thus
    // the worker and monitor checks cannot accept recycled integer PIDs.
    let owner = File::open(format!("/proc/self/fd/{owner_fd}/stat"))?;
    let monitor = File::open(format!("/proc/self/fd/{host_proc_fd}/{monitor}/stat"))?;
    nix::unistd::close(*owner_fd)?;
    nix::unistd::close(*host_proc_fd)?;
    let data = inherited(*data_fd, false)?;
    let seals = rustix::fs::fcntl_get_seals(&data)?;
    if !seals.contains(super::seals()) {
        return Err(io::Error::other("native launch data is not immutable"));
    }
    let mut bytes = Vec::new();
    data.take(MAX_PACKET + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PACKET {
        return Err(io::Error::other("native launch data exceeds limit"));
    }
    let packet: Packet = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    nix::unistd::close(packet.launcher_fd)?;
    nix::unistd::close(packet.namespace_fd)?;
    let image = packet.image_fd.map(|fd| inherited(fd, false)).transpose()?;
    nix::unistd::close(packet.cwd_fd)?;
    let program = image.as_ref().map_or_else(
        || std::ffi::OsString::from_vec(packet.program),
        |image| format!("/proc/self/fd/{}", image.as_raw_fd()).into(),
    );
    let mut command = std::process::Command::new(program);
    command
        .env_clear()
        .arg0(std::ffi::OsString::from_vec(packet.argv0))
        .args(
            packet
                .arguments
                .into_iter()
                .map(std::ffi::OsString::from_vec),
        )
        .envs(packet.environment.into_iter().map(|(name, value)| {
            (
                std::ffi::OsString::from_vec(name),
                std::ffi::OsString::from_vec(value),
            )
        }))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(fd) = packet.resource_fd
        && let Err(error) = super::resources::join(fd)
    {
        return refuse(&mut ack, b'l', error);
    }
    if let Some(filesystem) = &packet.filesystem
        && let Err(error) = super::filesystem_root::enter(filesystem)
    {
        return refuse(&mut ack, b'r', error);
    }
    protect_supervisor()?;
    rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))?;
    live(monitor)?;
    live(owner)?;
    drop(gate);
    let mut retained = vec![ack.as_raw_fd()];
    retained.extend(image.iter().map(|image| image.as_raw_fd()));
    if let Some(filesystem) = &packet.filesystem {
        retained.extend(filesystem.directories.iter().map(|directory| directory.fd));
    }
    close_ambient_descriptors(&retained)?;
    if packet.isolated_network {
        super::network::enforce()?;
    }
    if let Some(filesystem) = packet.filesystem {
        super::filesystem::enforce(filesystem.directories)?;
    }
    // PID 1 remains trusted: commands retain ordinary signal/reaping semantics.
    // Exiting PID 1 makes the kernel kill every remaining namespace member.
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            ack.write_all(b"e")?;
            ack.write_all(
                &error
                    .raw_os_error()
                    .unwrap_or(rustix::io::Errno::IO.raw_os_error())
                    .to_le_bytes(),
            )?;
            return Err(error);
        }
    };
    ack.write_all(b"g")?;
    let leader = i32::try_from(child.id()).map_err(io::Error::other)?;
    drop(image);
    loop {
        match rustix::process::wait(rustix::process::WaitOptions::empty()) {
            Ok(Some((pid, status))) if pid.as_raw_nonzero().get() == leader => {
                // The outer monitor's exit code cannot represent a signal or
                // core dump. Report the original wait status through a private
                // descriptor, then finish kernel namespace teardown as usual.
                ack.write_all(&status.as_raw().to_le_bytes())?;
                return Ok(status
                    .exit_status()
                    .unwrap_or_else(|| 128 + status.terminating_signal().unwrap_or(0))
                    as u8);
            }
            Ok(Some(_)) => {}
            Ok(None) => return Err(io::Error::other("native command was not waitable")),
            Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

fn close_ambient_descriptors(retained: &[i32]) -> io::Result<()> {
    // This entry point is single-threaded and runs before Tokio. Finish the
    // directory scan before closing anything, including its own scan fd. No
    // descriptor allocation occurs in the close loop, so EBADF can only refer
    // to the already-dropped directory iterator, not a replacement object.
    let descriptors = std::fs::read_dir("/proc/self/fd")?
        .take(4097)
        .map(|entry| {
            entry?
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i32>().ok())
                .ok_or_else(|| io::Error::other("invalid inherited descriptor"))
        })
        .collect::<io::Result<Vec<_>>>()?;
    if descriptors.len() > 4096 {
        return Err(io::Error::other("too many inherited descriptors"));
    }
    for fd in descriptors {
        if fd > 2 && !retained.contains(&fd) {
            match nix::unistd::close(fd) {
                Ok(()) | Err(nix::errno::Errno::EBADF) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

pub(super) fn protect_supervisor() -> io::Result<()> {
    use rustix::thread::{CapabilitiesSecureBits, CapabilitySet, CapabilitySets};
    let capabilities = rustix::thread::capabilities(None)?;
    if capabilities.effective.contains(CapabilitySet::SETPCAP) {
        let bits = rustix::thread::capabilities_secure_bits()?;
        rustix::thread::set_capabilities_secure_bits(
            bits | CapabilitiesSecureBits::NO_ROOT
                | CapabilitiesSecureBits::NO_ROOT_LOCKED
                | CapabilitiesSecureBits::NO_CAP_AMBIENT_RAISE
                | CapabilitiesSecureBits::NO_CAP_AMBIENT_RAISE_LOCKED,
        )?;
    }
    rustix::thread::clear_ambient_capability_set()?;
    rustix::thread::set_no_new_privs(true)?;
    rustix::thread::set_capabilities(
        None,
        CapabilitySets {
            effective: CapabilitySet::empty(),
            permitted: CapabilitySet::empty(),
            inheritable: CapabilitySet::empty(),
        },
    )?;
    // A payload must not ptrace PID 1 to remove its parent-death guard. The
    // child inherits empty capabilities and no-new-privileges, including UID 0.
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)?;
    Ok(())
}

fn monitor(fields: &str) -> io::Result<u8> {
    let descriptors = fields
        .split(':')
        .map(str::parse::<i32>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(io::Error::other)?;
    let [data_fd, _gate_fd, _ack_fd, owner_fd, _host_proc_fd] = descriptors.as_slice() else {
        return Err(io::Error::other("invalid native monitor descriptors"));
    };
    let data = File::open(format!("/proc/self/fd/{data_fd}"))?;
    if !rustix::fs::fcntl_get_seals(&data)?.contains(super::seals()) {
        return Err(io::Error::other("native launch data is not immutable"));
    }
    let mut bytes = Vec::new();
    data.take(MAX_PACKET + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PACKET {
        return Err(io::Error::other("native launch data exceeds limit"));
    }
    let packet: Packet = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))?;
    let parent = rustix::process::getppid().map(|pid| pid.as_raw_nonzero().get() as u32);
    if parent != Some(packet.parent_pid) {
        return Err(io::Error::other(
            "native monitor lost its parent during startup",
        ));
    }
    live(File::open(format!("/proc/self/fd/{owner_fd}/stat"))?)?;
    let error = std::process::Command::new(std::ffi::OsString::from_vec(packet.namespace_program))
        .env_clear()
        .args(
            packet
                .namespace_arguments
                .into_iter()
                .map(std::ffi::OsString::from_vec),
        )
        .args([
            "--user",
            "--map-current-user",
            "--pid",
            "--fork",
            "--kill-child=KILL",
            "--mount-proc",
            "--propagation",
            "private",
        ])
        .args(packet.isolated_network.then_some("--net"))
        .args(packet.filesystem.is_some().then_some("--keep-caps"))
        .arg("--")
        .arg(std::ffi::OsString::from_vec(packet.launcher_program))
        .args(
            packet
                .launcher_arguments
                .into_iter()
                .map(std::ffi::OsString::from_vec),
        )
        .arg(format!("qq-native-exec:{fields}"))
        .exec();
    Err(error)
}

fn refuse(ack: &mut File, kind: u8, error: io::Error) -> io::Result<u8> {
    let message = error.to_string();
    let bytes = &message.as_bytes()[..message.len().min(MAX_REFUSAL_BYTES)];
    ack.write_all(&[kind])?;
    ack.write_all(&(bytes.len() as u16).to_le_bytes())?;
    ack.write_all(bytes)?;
    Err(error)
}
