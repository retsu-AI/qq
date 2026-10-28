//! Linux process-group teardown that counts a zombie as stopped. Signal zero
//! still finds a killed descendant until something reaps it, and a container
//! whose PID 1 never reaps would otherwise look like a group that will not die.
//! The caller kills the group while its leader still pins the id; this only
//! waits, and never signals a group by its number.

fn group_member_identity(pid: u32) -> std::io::Result<Option<(i32, u64)>> {
    let file = match std::fs::File::open(format!("/proc/{pid}/stat")) {
        Ok(file) => file,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || error.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    read_group_member_identity(file)
}

fn read_group_member_identity(file: std::fs::File) -> std::io::Result<Option<(i32, u64)>> {
    use std::io::Read;
    let mut stat = Vec::new();
    match file.take(4097).read_to_end(&mut stat) {
        Ok(_) => {}
        // procfs can retain an open inode after its task is reaped. That
        // task is gone, even though opening stat succeeded moments earlier.
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || error.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    }
    if stat.len() > 4096 {
        return Err(std::io::Error::other(
            "check group member stat exceeds bound",
        ));
    }
    let malformed = || std::io::Error::other("invalid check group member stat");
    let delimiter = stat
        .windows(2)
        .rposition(|bytes| bytes == b") ")
        .ok_or_else(malformed)?;
    let fields = std::str::from_utf8(&stat[delimiter + 2..]).map_err(|_| malformed())?;
    let mut fields = fields.split_ascii_whitespace();
    let group = fields
        .nth(2)
        .ok_or_else(malformed)?
        .parse()
        .map_err(|_| malformed())?;
    let started = fields
        .nth(16)
        .ok_or_else(malformed)?
        .parse()
        .map_err(|_| malformed())?;
    Ok(Some((group, started)))
}

#[cfg(test)]
#[test]
fn an_exited_process_stat_does_not_fail_group_cleanup() {
    let mut child = std::process::Command::new("/bin/sh")
        .env_clear()
        .args(["-c", "read token"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let stat = std::fs::File::open(format!("/proc/{}/stat", child.id())).unwrap();
    drop(child.stdin.take());
    child.wait().unwrap();
    assert_eq!(read_group_member_identity(stat).unwrap(), None);
}

pub(crate) fn confirm_linux_group_exit(
    pgid: rustix::process::Pid,
    deadline: std::time::Instant,
) -> std::io::Result<()> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let mut confirmed = std::collections::HashSet::new();
    loop {
        let check_deadline = || {
            deadline
                .checked_duration_since(std::time::Instant::now())
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "check process group did not exit",
                    )
                })
        };
        check_deadline()?;
        // The caller killed the group while its leader still pinned the id.
        // The leader is reaped by now, so the id may name someone else's
        // group: never signal it, only wait for the members found in it.
        let mut members = Vec::new();
        let mut identities = std::collections::HashSet::new();
        for (index, entry) in std::fs::read_dir("/proc")?.enumerate() {
            check_deadline()?;
            if index >= 65_536 {
                return Err(std::io::Error::other(
                    "check group process enumeration exceeds bound",
                ));
            }
            let entry = entry?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            let Some((group, started)) = group_member_identity(pid)? else {
                continue;
            };
            if group != pgid.as_raw_nonzero().get() {
                continue;
            }
            if members.len() >= 4096 {
                return Err(std::io::Error::other(
                    "check group member count exceeds bound",
                ));
            }
            let Some(process) = i32::try_from(pid)
                .ok()
                .and_then(rustix::process::Pid::from_raw)
            else {
                return Err(std::io::Error::other("invalid check group member pid"));
            };
            let descriptor =
                match rustix::process::pidfd_open(process, rustix::process::PidfdFlags::empty()) {
                    Ok(descriptor) => descriptor,
                    Err(rustix::io::Errno::SRCH) => continue,
                    Err(error) => return Err(error.into()),
                };
            match group_member_identity(pid)? {
                None => continue,
                Some(identity) if identity == (group, started) => {}
                Some(_) => {
                    return Err(std::io::Error::other(
                        "check group member identity changed during observation",
                    ));
                }
            }
            identities.insert((pid, started));
            members.push(descriptor);
        }
        // A zombie thread-group leader can still have live threads. Only
        // pidfd readiness proves the whole process has stopped executing.
        for member in &members {
            loop {
                let remaining = check_deadline()?;
                let timeout = Timespec::try_from(remaining).map_err(std::io::Error::other)?;
                let mut descriptors = [PollFd::new(member, PollFlags::IN)];
                match poll(&mut descriptors, Some(&timeout)) {
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(error) => return Err(error.into()),
                    Ok(_)
                        if descriptors[0]
                            .revents()
                            .intersects(PollFlags::IN | PollFlags::HUP) =>
                    {
                        break;
                    }
                    Ok(0) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "check group member did not exit",
                        ));
                    }
                    Ok(_) => {
                        return Err(std::io::Error::other(
                            "unexpected check group member readiness",
                        ));
                    }
                }
            }
        }
        // A complete rescan closes the fork-during-enumeration window. A
        // recycled group id can only add an unrelated process to the scan,
        // which is waited for, never signalled by group.
        if identities.is_subset(&confirmed) {
            return Ok(());
        }
        confirmed = identities;
    }
}

#[cfg(test)]
mod tests {
    /// A killed background child that nobody reaps stays a zombie in the
    /// group. Teardown must still confirm, as under a container's PID 1.
    #[test]
    fn a_zombie_descendant_counts_as_stopped() {
        if std::env::var_os("QQ_ZOMBIE_GROUP_WORKER").is_none() {
            // The subreaper bit is process-wide; never set it in the shared
            // test process.
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tools::process_group::tests::a_zombie_descendant_counts_as_stopped",
                    "--nocapture",
                ])
                .env("QQ_ZOMBIE_GROUP_WORKER", "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        // Orphans reparent to this process, which never reaps them.
        rustix::process::set_child_subreaper(Some(rustix::process::getpid())).unwrap();
        use std::os::unix::process::CommandExt as _;
        let mut leader = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                "tail -f /dev/null & echo ready; exec tail -f /dev/null",
            ])
            .stdout(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut ready = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(leader.stdout.take().unwrap()),
            &mut ready,
        )
        .unwrap();
        let pgid = rustix::process::Pid::from_raw(i32::try_from(leader.id()).unwrap()).unwrap();
        rustix::process::kill_process_group(pgid, rustix::process::Signal::KILL).unwrap();
        leader.wait().unwrap();
        // The background child is now an unreaped zombie in the group.
        assert!(rustix::process::test_kill_process_group(pgid).is_ok());
        super::confirm_linux_group_exit(
            pgid,
            std::time::Instant::now() + std::time::Duration::from_secs(5),
        )
        .unwrap();
    }
}

#[cfg(test)]
mod reuse_tests {
    /// After the leader is reaped its group id may be reused. Confirmation
    /// reaches only processes it found and pinned, and a live one is waited
    /// for, never killed: a reused id can delay teardown, not kill a stranger.
    #[test]
    fn confirmation_never_kills_a_live_process_in_a_reused_group() {
        use std::os::unix::process::CommandExt as _;
        let mut unrelated = std::process::Command::new("/bin/sh")
            .args(["-c", "exec tail -f /dev/null"])
            .stdout(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let group = rustix::process::Pid::from_raw(i32::try_from(unrelated.id()).unwrap()).unwrap();
        let result = super::confirm_linux_group_exit(
            group,
            std::time::Instant::now() + std::time::Duration::from_millis(300),
        );
        let alive = unrelated.try_wait().unwrap().is_none();
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
        assert!(alive, "confirmation killed a process it does not own");
        assert!(result.is_err(), "a live member must not count as stopped");
    }
}
