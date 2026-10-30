use std::collections::HashSet;

use rustix::{
    io::Errno,
    process::{Pid, Signal},
};

const MAX_ROUNDS: usize = 64;
const MAX_TRACKED: usize = 4096;
const MAX_PROCESSES: usize = 65_536;

#[cfg(target_os = "linux")]
pub(super) type Tracked = rustix::fd::OwnedFd;
#[cfg(not(target_os = "linux"))]
pub(super) type Tracked = Pid;

struct Entry {
    pid: i32,
    parent: i32,
    group: i32,
    #[cfg(target_os = "linux")]
    started: u64,
}

pub(super) fn stop_and_kill(group: Pid, leader: Option<Pid>) -> std::io::Result<Vec<Tracked>> {
    match rustix::process::kill_process_group(group, Signal::STOP) {
        Ok(()) | Err(Errno::SRCH | Errno::PERM) => {}
        Err(error) => return Err(error.into()),
    }
    let mut seen = HashSet::new();
    let mut tracked = Vec::new();
    let collected = collect(group, leader, &mut seen, &mut tracked);
    for process in &tracked {
        match signal(process, Signal::KILL) {
            Ok(()) | Err(Errno::SRCH | Errno::PERM) => {}
            Err(error) => return Err(error.into()),
        }
    }
    collected.map(|()| tracked)
}

fn collect(
    group: Pid,
    leader: Option<Pid>,
    seen: &mut HashSet<i32>,
    tracked: &mut Vec<Tracked>,
) -> std::io::Result<()> {
    let group = group.as_raw_nonzero().get();
    let leader = leader.map(|leader| leader.as_raw_nonzero().get());
    for _ in 0..MAX_ROUNDS {
        let mut found = false;
        for entry in snapshot()? {
            let member =
                entry.group == group || Some(entry.pid) == leader || seen.contains(&entry.parent);
            if !member || seen.contains(&entry.pid) {
                continue;
            }
            if tracked.len() >= MAX_TRACKED {
                return Err(std::io::Error::other(
                    "command descendant count exceeds bound",
                ));
            }
            let Some(process) = hold(&entry)? else {
                continue;
            };
            match signal(&process, Signal::STOP) {
                Ok(()) | Err(Errno::PERM) => {}
                Err(Errno::SRCH) => continue,
                Err(error) => return Err(error.into()),
            }
            seen.insert(entry.pid);
            tracked.push(process);
            found = true;
        }
        if !found {
            return Ok(());
        }
    }
    Err(std::io::Error::other(
        "command descendants kept appearing while they were stopped",
    ))
}

#[cfg(target_os = "linux")]
fn snapshot() -> std::io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for (index, entry) in std::fs::read_dir("/proc")?.enumerate() {
        if index >= MAX_PROCESSES {
            return Err(std::io::Error::other("process enumeration exceeds bound"));
        }
        let Some(pid) = entry?
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Some(stat) = super::super::process_group::group_member_identity(pid)? else {
            continue;
        };
        entries.push(Entry {
            pid: i32::try_from(pid).map_err(std::io::Error::other)?,
            parent: stat.parent,
            group: stat.group,
            started: stat.started,
        });
    }
    Ok(entries)
}

#[cfg(not(target_os = "linux"))]
fn snapshot() -> std::io::Result<Vec<Entry>> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-A", "-o", "pid=,ppid=,pgid="])
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "ps could not list processes: {}",
            output.status
        )));
    }
    let malformed = || std::io::Error::other("invalid ps process line");
    let text = std::str::from_utf8(&output.stdout).map_err(|_| malformed())?;
    let mut entries = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if index >= MAX_PROCESSES {
            return Err(std::io::Error::other("process enumeration exceeds bound"));
        }
        let mut fields = line.split_ascii_whitespace().map(str::parse::<i32>);
        let (Some(Ok(pid)), Some(Ok(parent)), Some(Ok(group)), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(malformed());
        };
        entries.push(Entry { pid, parent, group });
    }
    Ok(entries)
}

#[cfg(target_os = "linux")]
fn hold(entry: &Entry) -> std::io::Result<Option<Tracked>> {
    let Some(pid) = Pid::from_raw(entry.pid) else {
        return Ok(None);
    };
    let descriptor = match rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()) {
        Ok(descriptor) => descriptor,
        Err(Errno::SRCH) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let current = u32::try_from(entry.pid)
        .map_err(std::io::Error::other)
        .and_then(super::super::process_group::group_member_identity)?;
    Ok(current
        .filter(|stat| stat.started == entry.started)
        .map(|_| descriptor))
}

#[cfg(not(target_os = "linux"))]
fn hold(entry: &Entry) -> std::io::Result<Option<Tracked>> {
    Ok(Pid::from_raw(entry.pid))
}

#[cfg(target_os = "linux")]
fn signal(process: &Tracked, signal: Signal) -> Result<(), Errno> {
    rustix::process::pidfd_send_signal(process, signal)
}

#[cfg(not(target_os = "linux"))]
fn signal(process: &Tracked, signal: Signal) -> Result<(), Errno> {
    rustix::process::kill_process(*process, signal)
}

#[cfg(target_os = "linux")]
pub(super) fn confirm(tracked: Vec<Tracked>, deadline: std::time::Instant) -> std::io::Result<()> {
    super::super::process_group::wait_for_exits(&tracked, deadline)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn gone(tracked: &[Tracked]) -> std::io::Result<bool> {
    for process in tracked {
        match rustix::process::test_kill_process(*process) {
            Err(Errno::SRCH) => {}
            Ok(()) | Err(Errno::PERM) => return Ok(false),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(true)
}
