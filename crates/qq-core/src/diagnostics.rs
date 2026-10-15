use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crossbeam_channel::{Receiver, Sender, bounded, select_biased};

const MAX_FILE_BYTES: u64 = 64 * 1024;
const SLOT_COUNT: usize = 8;
const QUEUE_CAPACITY: usize = 128;
static GLOBAL: OnceLock<DiagnosticSink> = OnceLock::new();

#[derive(Clone, Copy, Debug)]
pub enum DiagnosticEvent {
    InteractiveStarted,
    TuiStarted,
    InteractiveExit,
    TuiExit,
    ServerOwnership,
    ServerShutdown,
    ConnectionState,
    ClientCommandFailure,
    CommandRejected,
}
#[derive(Clone, Copy, Debug)]
pub enum DiagnosticReason {
    Completed,
    Failed,
    Attached,
    Owned,
    OwnerExit,
    OwnerHandleMissing,
    ShutdownCompleted,
    ShutdownFailed,
    Offline,
    InvalidSlashCommand,
    ReservedClientCommand,
    CommandError,
    Signal,
    SignalError,
    ShutdownBeforeTerminal,
    ExplicitQuit,
    TerminalError,
    TerminalSetupError,
    TerminalLoopError,
    TerminalClosed,
    ClientStreamClosed,
}
macro_rules! names {
    ($($event:ident => $ename:literal),*; $($reason:ident => $rname:literal),*) => {
        impl DiagnosticEvent { const fn as_str(self) -> &'static str { match self { $(Self::$event => $ename),* } } }
        impl DiagnosticReason { const fn as_str(self) -> &'static str { match self { $(Self::$reason => $rname),* } } }
    }
}
names! {
 InteractiveStarted=>"interactive_started", InteractiveExit=>"interactive_exit", TuiStarted=>"tui_started", TuiExit=>"tui_exit",
 ServerOwnership=>"server_ownership", ServerShutdown=>"server_shutdown",
 ConnectionState=>"connection_state", ClientCommandFailure=>"client_command_failure",
 CommandRejected=>"command_rejected";
 Completed=>"completed", Failed=>"failed", Attached=>"attached", Owned=>"owned", OwnerExit=>"owner_exit",
 OwnerHandleMissing=>"owner_handle_missing", ShutdownCompleted=>"shutdown_completed",
 ShutdownFailed=>"shutdown_failed", Offline=>"offline",
 InvalidSlashCommand=>"invalid_slash_command", ReservedClientCommand=>"reserved_client_command",
 CommandError=>"command_error", Signal=>"signal", SignalError=>"signal_error",
 ShutdownBeforeTerminal=>"shutdown_before_terminal", ExplicitQuit=>"explicit_quit",
 TerminalError=>"terminal_error", TerminalSetupError=>"terminal_setup_error",
 TerminalLoopError=>"terminal_loop_error", TerminalClosed=>"terminal_closed",
 ClientStreamClosed=>"client_stream_closed"
}

const HEADER: &[u8] = b"{\"qq_lifecycle_format\":1}\n";
const NORMAL_LIMIT: u64 = MAX_FILE_BYTES - 4096;
const FLUSH_TIMEOUT: Duration = Duration::from_millis(100);

struct Record {
    timestamp_ms: u128,
    pid: u32,
    event: DiagnosticEvent,
    reason: Option<DiagnosticReason>,
}
impl Record {
    fn new(event: DiagnosticEvent, reason: Option<DiagnosticReason>) -> Self {
        Self {
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |v| v.as_millis()),
            pid: std::process::id(),
            event,
            reason,
        }
    }
    fn critical(&self) -> bool {
        matches!(
            self.event,
            DiagnosticEvent::TuiExit
                | DiagnosticEvent::ServerShutdown
                | DiagnosticEvent::InteractiveExit
        )
    }
}
enum Control {
    Finish {
        last: Option<Record>,
        ack: Sender<bool>,
    },
    Priority(Record),
}

#[derive(Clone)]
pub struct DiagnosticSink {
    records: Sender<Record>,
    controls: Sender<Control>,
    critical_lost: Arc<AtomicBool>,
}

impl DiagnosticSink {
    pub fn for_runtime(directory: &Path) -> Option<Self> {
        let (records, record_rx) = bounded(QUEUE_CAPACITY);
        let (controls, control_rx) = bounded(16);
        let critical_lost = Arc::new(AtomicBool::new(false));
        let writer_lost = critical_lost.clone();
        let directory = directory.to_path_buf();
        std::thread::Builder::new()
            .name("qq-diagnostics".to_owned())
            .spawn(move || write_loop(&directory, record_rx, control_rx, writer_lost))
            .ok()?;
        Some(Self {
            records,
            controls,
            critical_lost,
        })
    }
    pub fn install_global(self) {
        let _ = GLOBAL.set(self);
    }
    pub fn record(&self, event: DiagnosticEvent, reason: Option<DiagnosticReason>) {
        let record = Record::new(event, reason);
        if record.critical() {
            if self.controls.try_send(Control::Priority(record)).is_err() {
                self.critical_lost.store(true, Ordering::Relaxed);
            }
        } else {
            let _ = self.records.try_send(record);
        }
    }
    pub fn flush(&self) -> bool {
        self.finish(None)
    }
    fn finish(&self, last: Option<Record>) -> bool {
        let deadline = Instant::now() + FLUSH_TIMEOUT;
        let (ack, receiver) = bounded(1);
        if self
            .controls
            .send_deadline(Control::Finish { last, ack }, deadline)
            .is_err()
        {
            return false;
        }
        receiver.recv_deadline(deadline).unwrap_or(false)
    }
}

/// Finalizes diagnostics on every post-install return, including startup errors.
pub struct InteractiveExitGuard {
    completed: bool,
}
impl Default for InteractiveExitGuard {
    fn default() -> Self {
        Self::new()
    }
}
impl InteractiveExitGuard {
    pub fn new() -> Self {
        Self { completed: false }
    }
    pub fn completed(&mut self) {
        self.completed = true;
    }
}
impl Drop for InteractiveExitGuard {
    fn drop(&mut self) {
        if let Some(sink) = GLOBAL.get() {
            let reason = if self.completed {
                DiagnosticReason::Completed
            } else {
                DiagnosticReason::Failed
            };
            if !sink.finish(Some(Record::new(
                DiagnosticEvent::InteractiveExit,
                Some(reason),
            ))) {
                let _ = writeln!(
                    std::io::stderr(),
                    "QQ lifecycle diagnostics could not be confirmed; this exit may not be recorded."
                );
            }
        }
    }
}
pub fn record_global(event: DiagnosticEvent, reason: Option<DiagnosticReason>) {
    if let Some(sink) = GLOBAL.get() {
        sink.record(event, reason);
    }
}
pub fn flush_global() -> bool {
    GLOBAL.get().is_some_and(DiagnosticSink::flush)
}

struct Writer {
    file: File,
    bytes: u64,
    failed: bool,
}
impl Writer {
    fn record(&mut self, record: Record) {
        let critical = record.critical();
        let value = serde_json::json!({"timestamp_ms":record.timestamp_ms,"pid":record.pid,"event":record.event.as_str(),"reason":record.reason.map(DiagnosticReason::as_str)});
        let Ok(mut line) = serde_json::to_vec(&value) else {
            self.failed = true;
            return;
        };
        line.push(b'\n');
        let limit = if critical {
            MAX_FILE_BYTES
        } else {
            NORMAL_LIMIT
        };
        if self.bytes.saturating_add(line.len() as u64) > limit {
            self.failed |= critical;
            return;
        }
        if self.file.write_all(&line).is_err() {
            self.failed = true;
        }
        self.bytes += line.len() as u64;
    }
    fn drain(&mut self, records: &Receiver<Record>) {
        // Snapshot capacity bounds shutdown even if another producer is still active.
        for _ in 0..QUEUE_CAPACITY {
            match records.try_recv() {
                Ok(record) => self.record(record),
                Err(_) => break,
            }
        }
    }
}
fn write_loop(
    directory: &Path,
    records: Receiver<Record>,
    controls: Receiver<Control>,
    lost: Arc<AtomicBool>,
) {
    let Some(file) = open_slot(directory) else {
        while let Ok(control) = controls.recv() {
            if let Control::Finish { ack, .. } = control {
                let _ = ack.send(false);
            }
        }
        return;
    };
    let mut writer = Writer {
        file,
        bytes: HEADER.len() as u64,
        failed: false,
    };
    loop {
        select_biased! {
            recv(controls) -> message => match message {
                Ok(Control::Finish { last, ack }) => {
                    writer.drain(&records);
                    if let Some(last) = last { writer.record(last); }
                    let ok = writer.file.flush().is_ok() && !writer.failed && !lost.load(Ordering::Relaxed);
                    let _ = ack.send(ok);
                }
                Ok(Control::Priority(record)) => writer.record(record),
                Err(_) => { writer.drain(&records); let _ = writer.file.flush(); return; }
            },
            recv(records) -> message => match message {
                Ok(record) => writer.record(record),
                Err(_) => { let _ = writer.file.flush(); return; }
            }
        }
    }
}

#[cfg(unix)]
fn open_slot(directory: &Path) -> Option<File> {
    use rustix::fs::{FlockOperation, Mode, OFlags, flock, open, openat};
    use std::os::unix::fs::MetadataExt;
    let directory_fd = open(
        directory,
        OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let directory_file = File::from(directory_fd);
    let metadata = directory_file.metadata().ok()?;
    let uid = rustix::process::geteuid().as_raw();
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return None;
    }
    let flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let start = std::process::id() as usize % SLOT_COUNT;
    for offset in 0..SLOT_COUNT {
        let slot = (start + offset) % SLOT_COUNT;
        let name = format!("diagnostics-{slot}.jsonl");
        let (fd, created) = match openat(
            &directory_file,
            name.as_str(),
            flags | OFlags::CREATE | OFlags::EXCL,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(fd) => (fd, true),
            Err(rustix::io::Errno::EXIST) => {
                match openat(&directory_file, name.as_str(), flags, Mode::empty()) {
                    Ok(fd) => (fd, false),
                    Err(_) => continue,
                }
            }
            Err(_) => continue,
        };
        let mut file = File::from(fd);
        if flock(&file, FlockOperation::NonBlockingLockExclusive).is_err() {
            continue;
        }
        let Ok(meta) = file.metadata() else {
            continue;
        };
        if !meta.is_file() || meta.uid() != uid || meta.mode() & 0o777 != 0o600 || meta.nlink() != 1
        {
            continue;
        }
        if !created {
            let mut header = [0; HEADER.len()];
            if file.read_exact(&mut header).is_err() || header != HEADER {
                continue;
            }
        }
        // The descriptor owns this slot until process death; no persistent lockfile can strand it.
        if file.set_len(0).is_err()
            || file.seek(SeekFrom::Start(0)).is_err()
            || file.write_all(HEADER).is_err()
        {
            continue;
        }
        return Some(file);
    }
    None
}

#[cfg(not(unix))]
fn open_slot(_directory: &Path) -> Option<File> {
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        process::Command,
    };

    fn private_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }
    fn text(directory: &Path) -> String {
        fs::read_dir(directory)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
            .map(|entry| fs::read_to_string(entry.path()).unwrap())
            .collect()
    }
    #[test]
    fn flood_retains_terminal_event_and_bounds_bytes() {
        let dir = private_dir();
        let sink = DiagnosticSink::for_runtime(dir.path()).unwrap();
        for _ in 0..100_000 {
            sink.record(
                DiagnosticEvent::ClientCommandFailure,
                Some(DiagnosticReason::CommandError),
            );
        }
        sink.record(
            DiagnosticEvent::TuiExit,
            Some(DiagnosticReason::ExplicitQuit),
        );
        assert!(sink.finish(Some(Record::new(
            DiagnosticEvent::InteractiveExit,
            Some(DiagnosticReason::Completed)
        ))));
        let log = text(dir.path());
        assert!(log.contains("explicit_quit") && log.contains("interactive_exit"));
        assert!(log.len() as u64 <= MAX_FILE_BYTES);
    }
    #[test]
    fn normal_capacity_reserves_terminal_space() {
        let dir = private_dir();
        let file = open_slot(dir.path()).unwrap();
        let mut writer = Writer {
            file,
            bytes: HEADER.len() as u64,
            failed: false,
        };
        for _ in 0..100_000 {
            writer.record(Record::new(
                DiagnosticEvent::ConnectionState,
                Some(DiagnosticReason::Offline),
            ));
        }
        assert!(writer.bytes <= NORMAL_LIMIT);
        writer.record(Record::new(
            DiagnosticEvent::TuiExit,
            Some(DiagnosticReason::ExplicitQuit),
        ));
        assert!(!writer.failed);
        assert!(text(dir.path()).contains("explicit_quit"));
        assert!(writer.bytes <= MAX_FILE_BYTES);
    }
    #[test]
    fn repeated_launches_recover_after_clean_and_abrupt_exit() {
        let dir = private_dir();
        for index in 0..20 {
            let mode = if index % 2 == 0 {
                "abrupt"
            } else {
                "startup_error"
            };
            let result = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "diagnostics::tests::subprocess_driver",
                    "--nocapture",
                ])
                .env("QQ_DIAGNOSTIC_TEST_DIRECTORY", dir.path())
                .env("QQ_DIAGNOSTIC_TEST_MODE", mode)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        let logs = fs::read_dir(dir.path())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(logs.len() <= SLOT_COUNT);
        assert!(text(dir.path()).contains("interactive_exit"));
        assert!(text(dir.path()).contains("failed"));
    }
    #[test]
    fn subprocess_driver() {
        let Some(path) = std::env::var_os("QQ_DIAGNOSTIC_TEST_DIRECTORY") else {
            return;
        };
        let sink = DiagnosticSink::for_runtime(Path::new(&path)).unwrap();
        sink.install_global();
        let guard = InteractiveExitGuard::new();
        if std::env::var("QQ_DIAGNOSTIC_TEST_MODE").as_deref() == Ok("broken_stderr") {
            std::thread::sleep(Duration::from_millis(30));
            drop(guard);
            return;
        }
        record_global(DiagnosticEvent::InteractiveStarted, None);
        if std::env::var("QQ_DIAGNOSTIC_TEST_MODE").as_deref() == Ok("abrupt") {
            assert!(flush_global());
            std::process::exit(0);
        }
        drop(guard);
        assert!(flush_global());
        assert!(text(Path::new(&path)).contains("interactive_started"));
        assert!(text(Path::new(&path)).contains("interactive_exit"));
    }
    #[test]
    fn diagnostic_failure_preserves_exit_when_stderr_is_closed() {
        use std::process::Stdio;
        let dir = private_dir();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "diagnostics::tests::subprocess_driver",
                "--nocapture",
            ])
            .env("QQ_DIAGNOSTIC_TEST_DIRECTORY", dir.path().join("missing"))
            .env("QQ_DIAGNOSTIC_TEST_MODE", "broken_stderr")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        drop(child.stderr.take());
        assert!(child.wait().unwrap().success());
    }
    #[test]
    fn refuses_foreign_hardlinked_and_symlink_files() {
        let dir = private_dir();
        let outside = private_dir();
        let payload = b"user-owned\n";
        let target = outside.path().join("target");
        fs::write(&target, payload).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        for slot in 0..SLOT_COUNT {
            let path = dir.path().join(format!("diagnostics-{slot}.jsonl"));
            match slot % 3 {
                0 => {
                    fs::write(&path, payload).unwrap();
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                }
                1 => fs::hard_link(&target, path).unwrap(),
                _ => symlink(&target, path).unwrap(),
            }
        }
        assert!(open_slot(dir.path()).is_none());
        assert_eq!(fs::read(&target).unwrap(), payload);
        for entry in fs::read_dir(dir.path()).unwrap() {
            assert_eq!(fs::read(entry.unwrap().path()).unwrap(), payload);
        }
    }
    #[test]
    fn lifetime_locks_bound_parallel_writers_and_release() {
        use std::sync::{Arc, Barrier};
        let dir = private_dir();
        // Seed the owned namespace before concurrent writers arrive.
        let mut held = Vec::new();
        for _ in 0..SLOT_COUNT {
            held.push(open_slot(dir.path()).unwrap());
        }
        assert!(open_slot(dir.path()).is_none());
        drop(held);
        let barrier = Arc::new(Barrier::new(12));
        let workers = (0..12)
            .map(|_| {
                let path = dir.path().to_path_buf();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let slot = open_slot(&path);
                    barrier.wait();
                    slot.is_some()
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            workers
                .into_iter()
                .map(|w| usize::from(w.join().unwrap()))
                .sum::<usize>(),
            SLOT_COUNT
        );
        assert!(open_slot(dir.path()).is_some());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), SLOT_COUNT);
    }
    #[test]
    fn reuses_full_owned_slots_but_refuses_private_directory_redirect() {
        let dir = private_dir();
        let mut held = Vec::new();
        for _ in 0..SLOT_COUNT {
            let file = open_slot(dir.path()).unwrap();
            file.set_len(MAX_FILE_BYTES).unwrap();
            held.push(file);
        }
        drop(held);
        let file = open_slot(dir.path()).unwrap();
        assert_eq!(file.metadata().unwrap().len(), HEADER.len() as u64);
        let root = private_dir();
        let linked = root.path().join("runtime");
        symlink(dir.path(), &linked).unwrap();
        assert!(open_slot(&linked).is_none());
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(open_slot(dir.path()).is_none());
    }
}
