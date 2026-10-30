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

pub(super) fn stop_and_kill(
    group: Pid,
    leader: Option<Pid>,
    watcher: Option<Pid>,
) -> std::io::Result<Vec<Tracked>> {
    let mut tracked = Vec::new();
    let collected = collect(group, leader, watcher, &mut tracked);
    for process in tracked.iter().rev() {
        match signal(process, Signal::KILL) {
            Ok(()) | Err(Errno::SRCH | Errno::PERM) => {}
            Err(error) => return Err(error.into()),
        }
    }
    collected.map(|()| tracked)
}

struct Membership {
    group: i32,
    leader: Option<i32>,
    watcher: Option<i32>,
}

impl Membership {
    fn admits(&self, entry: &Entry, stopped: &HashSet<i32>) -> bool {
        Some(entry.pid) != self.watcher
            && (Some(entry.pid) == self.leader
                || entry.group == self.group
                || stopped.contains(&entry.parent))
    }
}

fn collect(
    group: Pid,
    leader: Option<Pid>,
    watcher: Option<Pid>,
    tracked: &mut Vec<Tracked>,
) -> std::io::Result<()> {
    let membership = Membership {
        group: group.as_raw_nonzero().get(),
        leader: leader.map(|leader| leader.as_raw_nonzero().get()),
        watcher: watcher.map(|watcher| watcher.as_raw_nonzero().get()),
    };
    let mut seen = HashSet::new();
    let mut stopped = HashSet::new();
    for _ in 0..MAX_ROUNDS {
        let stopped_before = stopped.clone();
        let mut found = false;
        for entry in snapshot()? {
            if !membership.admits(&entry, &stopped_before) || seen.contains(&entry.pid) {
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
                Ok(()) => {
                    stopped.insert(entry.pid);
                }
                Err(Errno::PERM) => {
                    seen.insert(entry.pid);
                    continue;
                }
                Err(Errno::SRCH) => continue,
                Err(error) => return Err(error.into()),
            }
            seen.insert(entry.pid);
            tracked.push(process);
            found = true;
        }
        #[cfg(not(target_os = "linux"))]
        release_strangers(
            &membership,
            &stopped_before,
            &mut stopped,
            &mut seen,
            tracked,
        )?;
        if !found {
            return Ok(());
        }
    }
    Err(std::io::Error::other(
        "command descendants kept appearing while they were stopped",
    ))
}

#[cfg(not(target_os = "linux"))]
fn release_strangers(
    membership: &Membership,
    stopped_before: &HashSet<i32>,
    stopped: &mut HashSet<i32>,
    seen: &mut HashSet<i32>,
    tracked: &mut Vec<Tracked>,
) -> std::io::Result<()> {
    let current = snapshot()?
        .into_iter()
        .map(|entry| (entry.pid, entry))
        .collect::<std::collections::HashMap<_, _>>();
    let mut kept = Vec::with_capacity(tracked.len());
    for process in tracked.drain(..) {
        let pid = process.as_raw_nonzero().get();
        let verified = stopped_before.contains(&pid)
            || current
                .get(&pid)
                .is_some_and(|entry| membership.admits(entry, stopped_before));
        if verified {
            kept.push(process);
            continue;
        }
        if current.contains_key(&pid) {
            match rustix::process::kill_process(process, Signal::CONT) {
                Ok(()) | Err(Errno::SRCH | Errno::PERM) => {}
                Err(error) => return Err(error.into()),
            }
        }
        stopped.remove(&pid);
        seen.remove(&pid);
    }
    *tracked = kept;
    Ok(())
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[tokio::test]
    async fn collection_freezes_the_command_but_not_its_crash_watcher() {
        let mut watcher = super::super::spawn_watcher(
            std::time::Instant::now() + std::time::Duration::from_secs(10),
        )
        .await
        .unwrap();
        let watcher_pid = rustix::process::Pid::from_raw(watcher.id().unwrap() as i32).unwrap();
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .args(["-c", "exec tail -f /dev/null"])
            .process_group(watcher_pid.as_raw_nonzero().get())
            .kill_on_drop(true);
        let mut child = command.spawn().unwrap();
        let leader = rustix::process::Pid::from_raw(child.id().unwrap() as i32).unwrap();
        let state = |pid: rustix::process::Pid| {
            let stat =
                std::fs::read_to_string(format!("/proc/{}/stat", pid.as_raw_nonzero())).unwrap();
            let fields = &stat[stat.rfind(") ").unwrap() + 2..];
            fields.chars().next().unwrap()
        };
        let mut tracked = Vec::new();
        let collected = tokio::task::spawn_blocking(move || {
            let result = super::collect(watcher_pid, Some(leader), Some(watcher_pid), &mut tracked);
            (result, tracked)
        })
        .await
        .unwrap();
        collected.0.unwrap();
        let (leader_state, watcher_state) = (state(leader), state(watcher_pid));
        child.start_kill().unwrap();
        child.wait().await.unwrap();
        watcher.start_kill().unwrap();
        watcher.wait().await.unwrap();
        assert_eq!(collected.1.len(), 1);
        assert_eq!(leader_state, 'T');
        assert_ne!(watcher_state, 'T', "the crash watcher was frozen");
    }
}
