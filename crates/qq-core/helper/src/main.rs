#![forbid(unsafe_code)]

fn main() {
    let interrupted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGHUP,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGQUIT,
        signal_hook::consts::SIGTERM,
    ] {
        signal_hook::flag::register(signal, interrupted.clone())
            .expect("watcher signal registration");
    }
    std::process::exit(run());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn report_ready(command_pid: u32) {
    use std::os::fd::AsFd as _;
    let stdin = std::io::stdin();
    let mut bytes = &command_pid.to_le_bytes()[..];
    while !bytes.is_empty() {
        match rustix::io::write(stdin.as_fd(), bytes) {
            Ok(0) => return,
            Ok(written) => bytes = &bytes[written..],
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => return,
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn run() -> i32 {
    eprintln!("qq-command-watcher runs only on Linux and macOS");
    127
}

#[cfg(target_os = "linux")]
fn run() -> i32 {
    use rustix::process::Signal;

    let mut args = std::env::args_os().skip(1);
    let expected_parent = match args
        .next()
        .and_then(|raw| raw.to_str().and_then(|text| text.parse::<i32>().ok()))
    {
        Some(pid) => pid,
        None => return 127,
    };
    let program = match args.next() {
        Some(program) => program,
        None => return 127,
    };
    let program_args: Vec<std::ffi::OsString> = args.collect();

    if rustix::process::set_child_subreaper(Some(rustix::process::getpid())).is_err() {
        return 126;
    }

    let parent_now = rustix::process::getppid().map(|pid| pid.as_raw_nonzero().get());
    if parent_now != Some(expected_parent) {
        reap_until_empty();
        return 125;
    }

    let stdin_file = std::env::var_os("QQ_WATCHER_STDIN_FILE");

    let mut command = std::process::Command::new(&program);
    command.args(&program_args);
    if let Some(arg0) = std::env::var_os("QQ_WATCHER_ARG0") {
        use std::os::unix::process::CommandExt as _;
        command.arg0(arg0);
    }
    command.env_remove("QQ_WATCHER_ARG0");
    command.env_remove("QQ_WATCHER_STDIN_FILE");
    if let Some(path) = stdin_file {
        match std::fs::File::open(&path) {
            Ok(file) => {
                let _ = std::fs::remove_file(&path);
                command.stdin(std::process::Stdio::from(file));
            }
            Err(_) => return 124,
        }
    } else {
        command.stdin(std::process::Stdio::null());
    }

    let child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            reap_until_empty();
            return 123;
        }
    };
    let command_pid = child.id();
    std::mem::forget(child);

    report_ready(command_pid);

    let exit = supervise(command_pid);

    reap_until_empty();

    match exit {
        Termination::Code(code) => code.clamp(0, 255),
        Termination::Signal(signal) => {
            let me = rustix::process::getpid();
            let _ = rustix::process::set_parent_process_death_signal(None);
            let _ = signal_hook::low_level::emulate_default_handler(signal);
            let _ = rustix::process::kill_process(me, Signal::KILL);
            (128 + signal).clamp(0, 255)
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
enum Termination {
    Code(i32),
    Signal(i32),
}

#[cfg(target_os = "linux")]
fn supervise(command_pid: u32) -> Termination {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    use rustix::process::{Pid, WaitOptions, waitpid};
    use std::os::fd::AsFd as _;

    let command = i32::try_from(command_pid).ok().and_then(Pid::from_raw);
    let stdin = std::io::stdin();
    let stdin_fd = stdin.as_fd();
    let interval = Timespec {
        tv_sec: 0,
        tv_nsec: 100_000_000,
    };
    let mut parent_gone = false;
    let mut escaped = false;

    loop {
        loop {
            match waitpid(None, WaitOptions::NOHANG) {
                Ok(Some((pid, status))) => {
                    if Some(pid) == command {
                        return termination_of(status);
                    }
                }
                Ok(None) => break,
                Err(rustix::io::Errno::INTR) => continue,
                Err(rustix::io::Errno::CHILD) => {
                    escaped = true;
                    break;
                }
                Err(_) => break,
            }
        }

        if parent_gone {
            kill_direct_children();
            return if escaped {
                Termination::Code(0)
            } else {
                Termination::Signal(rustix::process::Signal::KILL.as_raw())
            };
        }

        let mut fds = [PollFd::new(&stdin_fd, PollFlags::IN)];
        match poll(&mut fds, Some(&interval)) {
            Ok(0) => {}
            Ok(_) => {
                let revents = fds[0].revents();
                let hangup = revents.intersects(PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL);
                if hangup || (revents.contains(PollFlags::IN) && drained_eof(stdin_fd)) {
                    parent_gone = true;
                }
            }
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => parent_gone = true,
        }
    }
}

#[cfg(target_os = "linux")]
fn drained_eof(fd: std::os::fd::BorrowedFd<'_>) -> bool {
    let mut buffer = [0_u8; 4096];
    loop {
        match rustix::io::read(fd, &mut buffer) {
            Ok(0) => return true,
            Ok(_) => return false,
            Err(rustix::io::Errno::INTR) => continue,
            Err(rustix::io::Errno::AGAIN) => return false,
            Err(_) => return true,
        }
    }
}

#[cfg(target_os = "linux")]
fn termination_of(status: rustix::process::WaitStatus) -> Termination {
    if let Some(code) = status.exit_status() {
        return Termination::Code(code);
    }
    if let Some(signal) = status.terminating_signal() {
        return Termination::Signal(signal);
    }
    Termination::Code(0)
}

#[cfg(target_os = "linux")]
fn reap_until_empty() {
    use rustix::process::{WaitOptions, waitpid};
    loop {
        kill_direct_children();
        match waitpid(None, WaitOptions::empty()) {
            Ok(Some(_)) | Ok(None) => continue,
            Err(rustix::io::Errno::INTR) => continue,
            Err(rustix::io::Errno::CHILD) => return,
            Err(_) => return,
        }
    }
}

#[cfg(target_os = "linux")]
fn kill_direct_children() {
    use rustix::process::{Signal, kill_process};
    for pid in direct_children() {
        if let Some(pid) = i32::try_from(pid)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = kill_process(pid, Signal::KILL);
        }
    }
}

#[cfg(target_os = "linux")]
fn direct_children() -> Vec<u32> {
    let me = std::process::id();
    let mut children = Vec::new();
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{me}/task")) else {
        return children;
    };
    for task in tasks.flatten() {
        let Some(tid) = task
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(listed) = std::fs::read_to_string(format!("/proc/{me}/task/{tid}/children")) else {
            continue;
        };
        for pid in listed
            .split_ascii_whitespace()
            .filter_map(|pid| pid.parse::<u32>().ok())
        {
            if !children.contains(&pid) {
                children.push(pid);
            }
        }
    }
    children
}

#[cfg(target_os = "macos")]
fn run() -> i32 {
    use rustix::process::Signal;

    let mut args = std::env::args_os().skip(1);
    let expected_parent = match args
        .next()
        .and_then(|raw| raw.to_str().and_then(|text| text.parse::<i32>().ok()))
    {
        Some(pid) => pid,
        None => return 127,
    };
    let program = match args.next() {
        Some(program) => program,
        None => return 127,
    };
    let program_args: Vec<std::ffi::OsString> = args.collect();

    let parent_now = rustix::process::getppid().map(|pid| pid.as_raw_nonzero().get());
    if parent_now != Some(expected_parent) {
        return 125;
    }

    let stdin_file = std::env::var_os("QQ_WATCHER_STDIN_FILE");

    let mut command = std::process::Command::new(&program);
    command.args(&program_args);
    if let Some(arg0) = std::env::var_os("QQ_WATCHER_ARG0") {
        use std::os::unix::process::CommandExt as _;
        command.arg0(arg0);
    }
    command.env_remove("QQ_WATCHER_ARG0");
    command.env_remove("QQ_WATCHER_STDIN_FILE");
    if let Some(path) = stdin_file {
        match std::fs::File::open(&path) {
            Ok(file) => {
                let _ = std::fs::remove_file(&path);
                command.stdin(std::process::Stdio::from(file));
            }
            Err(_) => return 124,
        }
    } else {
        command.stdin(std::process::Stdio::null());
    }

    let child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return 123,
    };
    let command_pid = child.id();
    std::mem::forget(child);

    report_ready(command_pid);

    let mut watched = Watched::default();
    let exit = supervise(command_pid, &mut watched);
    let cleaned = teardown(watched);

    if !cleaned {
        return 122;
    }

    match exit {
        Termination::Code(code) => code.clamp(0, 255),
        Termination::Signal(signal) => {
            let me = rustix::process::getpid();
            let _ = signal_hook::low_level::emulate_default_handler(signal);
            let _ = rustix::process::kill_process(me, Signal::KILL);
            (128 + signal).clamp(0, 255)
        }
    }
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
enum Termination {
    Code(i32),
    Signal(i32),
}

#[cfg(target_os = "macos")]
#[derive(Default)]
struct Watched {
    identities: Vec<ProcessIdentity>,
}

#[cfg(target_os = "macos")]
impl Watched {
    fn record(&mut self, snapshot: &[ProcessEntry]) {
        self.identities.retain(|identity| {
            snapshot
                .iter()
                .any(|entry| entry.pid == identity.pid && entry.start == identity.start)
        });
        for pid in descendants(std::process::id(), snapshot).into_iter().chain(
            snapshot
                .iter()
                .filter(|entry| {
                    entry.group == std::process::id() && entry.pid != std::process::id()
                })
                .map(|entry| entry.pid),
        ) {
            let Some(start) = snapshot
                .iter()
                .find(|entry| entry.pid == pid)
                .map(|entry| entry.start.clone())
            else {
                continue;
            };
            let identity = ProcessIdentity { pid, start };
            if !self.identities.contains(&identity) {
                self.identities.push(identity);
            }
        }
    }
}

#[cfg(target_os = "macos")]
#[derive(Clone, PartialEq, Eq)]
struct ProcessIdentity {
    pid: u32,
    start: String,
}

#[cfg(target_os = "macos")]
fn supervise(command_pid: u32, watched: &mut Watched) -> Termination {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    use rustix::process::{Pid, WaitOptions, waitpid};
    use std::os::fd::AsFd as _;

    let command = i32::try_from(command_pid).ok().and_then(Pid::from_raw);
    let stdin = std::io::stdin();
    let stdin_fd = stdin.as_fd();
    let interval = Timespec {
        tv_sec: 0,
        tv_nsec: 100_000_000,
    };

    loop {
        watched.record(&scan_processes());

        match waitpid(command, WaitOptions::NOHANG) {
            Ok(Some((_, status))) => return termination_of(status),
            Ok(None) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(rustix::io::Errno::CHILD) => {
                return Termination::Signal(rustix::process::Signal::KILL.as_raw());
            }
            Err(_) => {}
        }

        let mut fds = [PollFd::new(&stdin_fd, PollFlags::IN)];
        match poll(&mut fds, Some(&interval)) {
            Ok(0) => {}
            Ok(_) => {
                let revents = fds[0].revents();
                let hangup = revents.intersects(PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL);
                if hangup || (revents.contains(PollFlags::IN) && drained_eof(stdin_fd)) {
                    return Termination::Signal(rustix::process::Signal::KILL.as_raw());
                }
            }
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => return Termination::Signal(rustix::process::Signal::KILL.as_raw()),
        }
    }
}

#[cfg(target_os = "macos")]
fn drained_eof(fd: std::os::fd::BorrowedFd<'_>) -> bool {
    let mut buffer = [0_u8; 4096];
    loop {
        match rustix::io::read(fd, &mut buffer) {
            Ok(0) => return true,
            Ok(_) => return false,
            Err(rustix::io::Errno::INTR) => continue,
            Err(rustix::io::Errno::AGAIN) => return false,
            Err(_) => return true,
        }
    }
}

#[cfg(target_os = "macos")]
fn termination_of(status: rustix::process::WaitStatus) -> Termination {
    if let Some(code) = status.exit_status() {
        return Termination::Code(code);
    }
    if let Some(signal) = status.terminating_signal() {
        return Termination::Signal(signal);
    }
    Termination::Code(0)
}

#[cfg(target_os = "macos")]
fn teardown(mut watched: Watched) -> bool {
    use rustix::process::Signal;

    watched.record(&scan_processes());
    for identity in &watched.identities {
        signal_identity(identity, Signal::STOP);
    }

    watched.record(&scan_processes());
    for identity in &watched.identities {
        signal_identity(identity, Signal::STOP);
    }

    let mut ordered = watched.identities.clone();
    ordered.reverse();
    for identity in &ordered {
        signal_identity(identity, Signal::KILL);
    }

    reap_direct_children();
    confirm_gone(&watched.identities)
}

#[cfg(target_os = "macos")]
fn signal_identity(identity: &ProcessIdentity, signal: rustix::process::Signal) {
    if !alive(identity) {
        return;
    }
    if let Some(pid) = i32::try_from(identity.pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process(pid, signal);
    }
}

#[cfg(target_os = "macos")]
fn alive(identity: &ProcessIdentity) -> bool {
    scan_processes()
        .iter()
        .any(|entry| entry.pid == identity.pid && entry.start == identity.start)
}

#[cfg(target_os = "macos")]
fn reap_direct_children() {
    use rustix::process::{WaitOptions, waitpid};
    loop {
        match waitpid(None, WaitOptions::NOHANG) {
            Ok(Some(_)) => continue,
            Ok(None) => return,
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => return,
        }
    }
}

#[cfg(target_os = "macos")]
fn confirm_gone(identities: &[ProcessIdentity]) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        reap_direct_children();
        let snapshot = scan_processes();
        let alive = identities.iter().any(|identity| {
            snapshot
                .iter()
                .any(|entry| entry.pid == identity.pid && entry.start == identity.start)
        });
        if !alive {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(target_os = "macos")]
struct ProcessEntry {
    pid: u32,
    ppid: u32,
    group: u32,
    start: String,
}

#[cfg(target_os = "macos")]
fn scan_processes() -> Vec<ProcessEntry> {
    let Ok(output) = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,pgid=,lstart="])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .spawn()
    else {
        return Vec::new();
    };
    let scanner = output.id();
    let Ok(output) = output.wait_with_output() else {
        return Vec::new();
    };
    let Ok(text) = String::from_utf8(output.stdout) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(pid) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let Some(ppid) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let Some(group) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let start = fields.collect::<Vec<_>>().join(" ");
        if pid != scanner {
            entries.push(ProcessEntry {
                pid,
                ppid,
                group,
                start,
            });
        }
    }
    entries
}

#[cfg(target_os = "macos")]
fn descendants(root: u32, snapshot: &[ProcessEntry]) -> Vec<u32> {
    let mut ordered = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for entry in snapshot {
            if entry.ppid == parent && entry.pid != root && !ordered.contains(&entry.pid) {
                ordered.push(entry.pid);
                frontier.push(entry.pid);
            }
        }
    }
    ordered
}
