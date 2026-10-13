use qq_auth as auth;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const MAX_ORDINARY_RECORDS: u64 = 255;
const MAX_ORDINARY_BYTES: u64 = 130_560;
const MAX_LINE_BYTES: usize = 512;
const CLOSE_TIMEOUT: Duration = Duration::from_millis(250);
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);
#[cfg(test)]
static SESSION_LAUNCHES: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static WRITER_LAUNCHES: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
pub(crate) fn resource_counts() -> (u64, u64, u64) {
    (
        NEXT_SESSION_ID.load(Ordering::Acquire),
        SESSION_LAUNCHES.load(Ordering::Acquire),
        WRITER_LAUNCHES.load(Ordering::Acquire),
    )
}

#[derive(Debug, PartialEq, Eq)]
enum EncodingFailure {
    Serialization,
    LineLimit,
}
fn encode_line(value: &impl Serialize) -> Result<Vec<u8>, EncodingFailure> {
    let mut line = serde_json::to_vec(value).map_err(|_| EncodingFailure::Serialization)?;
    line.push(b'\n');
    if line.len() > MAX_LINE_BYTES {
        return Err(EncodingFailure::LineLimit);
    }
    Ok(line)
}
fn allocate_saturating(next: &AtomicU64) -> Option<u64> {
    let mut current = next.load(Ordering::Acquire);
    loop {
        if current == u64::MAX {
            return None;
        }
        match next.compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => return Some(current),
            Err(actual) => current = actual,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) enum Route {
    StoredKeyring,
    CodexOAuth,
    XaiOAuth,
    DirectConfiguredKeyring,
    TypesafeReviewer,
    OtherKnown,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) enum Endpoint {
    Typesafe,
    ChatGPT,
    Xai,
    CustomLoopback,
    Other,
}

#[derive(Clone, Copy, Debug, Serialize)]
enum SummaryStatus {
    Closed,
    Failed,
    BudgetExhausted,
}

#[derive(Serialize)]
struct Summary {
    kind: &'static str,
    session_id: u64,
    pid: u32,
    monotonic_ns: u64,
    emitted: u64,
    dropped: u64,
    bytes: u64,
    #[serde(rename = "sink")]
    status: SummaryStatus,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(tag = "kind")]
pub(crate) enum Event {
    ResolutionAdmitted {
        session_id: u64,
        pid: u32,
        monotonic_ns: u64,
        store_id: u64,
        coordinator_id: Option<u64>,
        attempt_id: u64,
        route: Route,
        endpoint: Endpoint,
    },
    OwnerAdmitted {
        session_id: u64,
        pid: u32,
        monotonic_ns: u64,
        store_id: u64,
        coordinator_id: u64,
        attempt_id: u64,
        flight_id: u64,
        route: Route,
        endpoint: Endpoint,
    },
    FlightJoined {
        session_id: u64,
        pid: u32,
        monotonic_ns: u64,
        store_id: u64,
        coordinator_id: u64,
        attempt_id: u64,
        flight_id: u64,
        route: Route,
        endpoint: Endpoint,
    },
    CacheHit {
        session_id: u64,
        pid: u32,
        monotonic_ns: u64,
        store_id: u64,
        coordinator_id: u64,
        attempt_id: u64,
        route: Route,
        endpoint: Endpoint,
        outcome: auth::CredentialDiagnosticOutcome,
    },
    BackendReadStarted {
        session_id: u64,
        pid: u32,
        monotonic_ns: u64,
        store_id: u64,
        coordinator_id: Option<u64>,
        flight_id: Option<u64>,
        read_id: u64,
        attempt_id: u64,
        route: Route,
        endpoint: Endpoint,
    },
    BackendReadReturned {
        session_id: u64,
        pid: u32,
        monotonic_ns: u64,
        store_id: u64,
        coordinator_id: Option<u64>,
        flight_id: Option<u64>,
        read_id: u64,
        attempt_id: u64,
        route: Route,
        endpoint: Endpoint,
        outcome: auth::CredentialDiagnosticOutcome,
    },
    ResolutionFinished {
        session_id: u64,
        pid: u32,
        monotonic_ns: u64,
        store_id: u64,
        coordinator_id: Option<u64>,
        attempt_id: u64,
        flight_id: Option<u64>,
        read_id: Option<u64>,
        route: Route,
        endpoint: Endpoint,
        outcome: auth::CredentialDiagnosticOutcome,
    },
    OwnerUnwind {
        session_id: u64,
        pid: u32,
        monotonic_ns: u64,
        store_id: u64,
        coordinator_id: u64,
        attempt_id: u64,
        flight_id: u64,
        route: Route,
        endpoint: Endpoint,
        outcome: auth::CredentialDiagnosticOwnerUnwind,
    },
}

impl auth::CredentialDiagnosticSink for DiagnosticSession {
    fn emit(&self, event: auth::CredentialDiagnosticEvent) {
        let (session_id, pid, monotonic_ns) = self.metadata();
        let event = match event {
            auth::CredentialDiagnosticEvent::ResolutionAdmitted {
                store_id,
                coordinator_id,
                attempt_id,
                route,
                endpoint,
            } => Event::ResolutionAdmitted {
                session_id,
                pid,
                monotonic_ns,
                store_id,
                coordinator_id,
                attempt_id,
                route: route.into(),
                endpoint: endpoint.into(),
            },
            auth::CredentialDiagnosticEvent::OwnerAdmitted {
                store_id,
                coordinator_id,
                attempt_id,
                flight_id,
                route,
                endpoint,
            } => Event::OwnerAdmitted {
                session_id,
                pid,
                monotonic_ns,
                store_id,
                coordinator_id,
                attempt_id,
                flight_id,
                route: route.into(),
                endpoint: endpoint.into(),
            },
            auth::CredentialDiagnosticEvent::FlightJoined {
                store_id,
                coordinator_id,
                attempt_id,
                flight_id,
                route,
                endpoint,
            } => Event::FlightJoined {
                session_id,
                pid,
                monotonic_ns,
                store_id,
                coordinator_id,
                attempt_id,
                flight_id,
                route: route.into(),
                endpoint: endpoint.into(),
            },
            auth::CredentialDiagnosticEvent::CacheHit {
                store_id,
                coordinator_id,
                attempt_id,
                route,
                endpoint,
                outcome,
            } => Event::CacheHit {
                session_id,
                pid,
                monotonic_ns,
                store_id,
                coordinator_id,
                attempt_id,
                route: route.into(),
                endpoint: endpoint.into(),
                outcome,
            },
            auth::CredentialDiagnosticEvent::BackendReadStarted {
                store_id,
                coordinator_id,
                flight_id,
                read_id,
                attempt_id,
                route,
                endpoint,
            } => Event::BackendReadStarted {
                session_id,
                pid,
                monotonic_ns,
                store_id,
                coordinator_id,
                flight_id,
                read_id,
                attempt_id,
                route: route.into(),
                endpoint: endpoint.into(),
            },
            auth::CredentialDiagnosticEvent::BackendReadReturned {
                store_id,
                coordinator_id,
                flight_id,
                read_id,
                attempt_id,
                route,
                endpoint,
                outcome,
            } => Event::BackendReadReturned {
                session_id,
                pid,
                monotonic_ns,
                store_id,
                coordinator_id,
                flight_id,
                read_id,
                attempt_id,
                route: route.into(),
                endpoint: endpoint.into(),
                outcome,
            },
            auth::CredentialDiagnosticEvent::ResolutionFinished {
                store_id,
                coordinator_id,
                attempt_id,
                flight_id,
                read_id,
                route,
                endpoint,
                outcome,
            } => Event::ResolutionFinished {
                session_id,
                pid,
                monotonic_ns,
                store_id,
                coordinator_id,
                attempt_id,
                flight_id,
                read_id,
                route: route.into(),
                endpoint: endpoint.into(),
                outcome,
            },
            auth::CredentialDiagnosticEvent::OwnerUnwind {
                store_id,
                coordinator_id,
                attempt_id,
                flight_id,
                route,
                endpoint,
                outcome,
            } => Event::OwnerUnwind {
                session_id,
                pid,
                monotonic_ns,
                store_id,
                coordinator_id,
                attempt_id,
                flight_id,
                route: route.into(),
                endpoint: endpoint.into(),
                outcome,
            },
        };
        self.emit(event);
    }

    fn allocate_id(&self) -> Option<u64> {
        self.allocate_id()
    }
}

impl From<auth::CredentialDiagnosticEndpoint> for Endpoint {
    fn from(endpoint: auth::CredentialDiagnosticEndpoint) -> Self {
        match endpoint {
            auth::CredentialDiagnosticEndpoint::Typesafe => Self::Typesafe,
            auth::CredentialDiagnosticEndpoint::ChatGpt => Self::ChatGPT,
            auth::CredentialDiagnosticEndpoint::Xai => Self::Xai,
            auth::CredentialDiagnosticEndpoint::CustomLoopback => Self::CustomLoopback,
            auth::CredentialDiagnosticEndpoint::Other => Self::Other,
        }
    }
}

impl From<auth::CredentialDiagnosticRoute> for Route {
    fn from(route: auth::CredentialDiagnosticRoute) -> Self {
        match route {
            auth::CredentialDiagnosticRoute::StoredKeyring => Self::StoredKeyring,
            auth::CredentialDiagnosticRoute::CodexOAuth => Self::CodexOAuth,
            auth::CredentialDiagnosticRoute::XaiOAuth => Self::XaiOAuth,
            auth::CredentialDiagnosticRoute::DirectConfiguredKeyring => {
                Self::DirectConfiguredKeyring
            }
            auth::CredentialDiagnosticRoute::TypesafeReviewer => Self::TypesafeReviewer,
            auth::CredentialDiagnosticRoute::OtherKnown => Self::OtherKnown,
        }
    }
}

struct WriterMessage(Vec<u8>);

pub(crate) struct DiagnosticSession {
    session_id: u64,
    next_id: AtomicU64,
    pid: u32,
    tx: Mutex<Option<mpsc::SyncSender<WriterMessage>>>,
    dropped: Arc<AtomicU64>,
    reserved_records: AtomicU64,
    reserved_bytes: AtomicU64,
    budget_exhausted: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    id_exhausted: AtomicBool,
    writer_done: AtomicBool,
    closed: AtomicBool,
    close_outcome: AtomicU64,
    #[cfg(test)]
    close_entered: AtomicBool,
    join: Mutex<Option<JoinHandle<()>>>,
    done: Mutex<mpsc::Receiver<()>>,
    started: Instant,
}

fn saturating_increment(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
        value.checked_add(1)
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloseOutcome {
    Closed,
    Incomplete,
}

impl DiagnosticSession {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = path;
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "private diagnostic output unsupported",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
                .open(path)?;
            validate_output_file(path, &file)?;
            Self::from_writer(file)
        }
    }

    fn from_writer<W: Write + Send + 'static>(file: W) -> io::Result<Self> {
        Self::from_writer_with_capacity(file, MAX_ORDINARY_RECORDS as usize)
    }
    fn from_writer_with_capacity<W: Write + Send + 'static>(
        file: W,
        capacity: usize,
    ) -> io::Result<Self> {
        let session_id = allocate_saturating(&NEXT_SESSION_ID)
            .ok_or_else(|| io::Error::other("diagnostic identifiers exhausted"))?;
        let (tx, rx) = mpsc::sync_channel(capacity);
        let (done_tx, done_rx) = mpsc::channel();
        let dropped = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicBool::new(false));
        let budget_exhausted = Arc::new(AtomicBool::new(false));
        let pid = std::process::id();
        let started = Instant::now();
        let state = WriterState {
            dropped: Arc::clone(&dropped),
            failed: Arc::clone(&failed),
            budget_exhausted: Arc::clone(&budget_exhausted),
            session_id,
            pid,
            started,
        };
        let join = thread::Builder::new()
            .name("qq-diagnostic-writer".to_owned())
            .spawn(move || {
                let writer = write_records(file, rx, state);
                // Include the writer's destructor in completion. A successful signal
                // must never precede a blocking close or a writer-drop panic.
                drop(writer);
                let _ = done_tx.send(());
            })?;
        #[cfg(test)]
        {
            saturating_increment(&SESSION_LAUNCHES);
            saturating_increment(&WRITER_LAUNCHES);
        }
        Ok(Self {
            session_id,
            next_id: AtomicU64::new(1),
            pid,
            tx: Mutex::new(Some(tx)),
            dropped,
            reserved_records: AtomicU64::new(0),
            reserved_bytes: AtomicU64::new(0),
            budget_exhausted,
            failed,
            id_exhausted: AtomicBool::new(false),
            writer_done: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            close_outcome: AtomicU64::new(0),
            #[cfg(test)]
            close_entered: AtomicBool::new(false),
            join: Mutex::new(Some(join)),
            done: Mutex::new(done_rx),
            started,
        })
    }

    #[cfg(test)]
    pub(crate) fn open_with_writer<W: Write + Send + 'static>(writer: W) -> Self {
        Self::from_writer(writer).expect("diagnostic writer fixture must start")
    }

    #[cfg(test)]
    fn set_next_id_for_test(&self, value: u64) {
        self.next_id.store(value, Ordering::Release);
    }

    pub(crate) fn allocate_id(&self) -> Option<u64> {
        let id = allocate_saturating(&self.next_id);
        if id.is_none() {
            self.id_exhausted.store(true, Ordering::Release);
            self.failed.store(true, Ordering::Release);
        }
        id
    }

    pub(crate) fn emit(&self, event: Event) {
        if self.closed.load(Ordering::Acquire) || self.id_exhausted.load(Ordering::Acquire) {
            saturating_increment(&self.dropped);
            return;
        }
        let line = match encode_line(&event) {
            Ok(line) => line,
            Err(_) => {
                saturating_increment(&self.dropped);
                return;
            }
        };
        self.admit_line(line);
    }
    fn admit_line(&self, line: Vec<u8>) {
        let line_bytes = line.len() as u64;
        if line.len() > MAX_LINE_BYTES || !self.reserve(line_bytes) {
            saturating_increment(&self.dropped);
            return;
        }
        let Ok(tx) = self.tx.try_lock() else {
            self.reserved_records.fetch_sub(1, Ordering::AcqRel);
            self.reserved_bytes.fetch_sub(line_bytes, Ordering::AcqRel);
            saturating_increment(&self.dropped);
            return;
        };
        let Some(tx) = tx.as_ref() else {
            self.reserved_records.fetch_sub(1, Ordering::AcqRel);
            self.reserved_bytes.fetch_sub(line_bytes, Ordering::AcqRel);
            saturating_increment(&self.dropped);
            return;
        };
        match tx.try_send(WriterMessage(line)) {
            Ok(()) => {}
            Err(_) => {
                saturating_increment(&self.dropped);
            }
        }
    }

    fn reserve(&self, line_bytes: u64) -> bool {
        let mut records = self.reserved_records.load(Ordering::Acquire);
        loop {
            if records >= MAX_ORDINARY_RECORDS {
                self.budget_exhausted.store(true, Ordering::Release);
                return false;
            }
            match self.reserved_records.compare_exchange_weak(
                records,
                records + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => records = actual,
            }
        }
        let mut bytes = self.reserved_bytes.load(Ordering::Acquire);
        loop {
            if bytes.saturating_add(line_bytes) > MAX_ORDINARY_BYTES {
                self.budget_exhausted.store(true, Ordering::Release);
                self.reserved_records.fetch_sub(1, Ordering::AcqRel);
                return false;
            }
            match self.reserved_bytes.compare_exchange_weak(
                bytes,
                bytes + line_bytes,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => bytes = actual,
            }
        }
    }

    fn metadata(&self) -> (u64, u32, u64) {
        (
            self.session_id,
            self.pid,
            self.started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
        )
    }

    pub(crate) fn close(&self) -> CloseOutcome {
        let deadline = Instant::now() + CLOSE_TIMEOUT;
        let already_closed = self.closed.swap(true, Ordering::AcqRel);
        #[cfg(test)]
        self.close_entered.store(true, Ordering::Release);
        match self.tx.try_lock() {
            Ok(mut tx) => {
                tx.take();
            }
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                self.failed.store(true, Ordering::Release);
                poisoned.into_inner().take();
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                self.failed.store(true, Ordering::Release);
            }
        }
        if already_closed && self.close_outcome.load(Ordering::Acquire) == 0 {
            return CloseOutcome::Incomplete;
        }
        if !self.writer_done.load(Ordering::Acquire)
            && let Ok(done) = self.done.try_lock()
        {
            let completed = if already_closed {
                done.try_recv().is_ok()
            } else {
                done.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .is_ok()
            };
            if completed {
                self.writer_done.store(true, Ordering::Release);
            }
        }
        let mut joined = false;
        if self.writer_done.load(Ordering::Acquire)
            && let Ok(mut join) = self.join.try_lock()
        {
            if let Some(handle) = join.as_ref() {
                while !handle.is_finished() && Instant::now() < deadline {
                    thread::yield_now();
                }
                if handle.is_finished() {
                    joined = join.take().unwrap().join().is_ok();
                }
            } else {
                joined = true;
            }
        }
        if already_closed {
            return if self.close_outcome.load(Ordering::Acquire) == 1 {
                CloseOutcome::Closed
            } else {
                CloseOutcome::Incomplete
            };
        }
        if !joined {
            self.failed.store(true, Ordering::Release);
        }
        let outcome = if self.failed.load(Ordering::Acquire) {
            CloseOutcome::Incomplete
        } else {
            CloseOutcome::Closed
        };
        self.close_outcome.store(
            if outcome == CloseOutcome::Closed {
                1
            } else {
                2
            },
            Ordering::Release,
        );
        outcome
    }
}

impl Drop for DiagnosticSession {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(unix)]
fn validate_output_file(path: &Path, file: &File) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let descriptor = file.metadata()?;
    let entry = std::fs::symlink_metadata(path)?;
    if !descriptor.is_file()
        || !entry.is_file()
        || descriptor.nlink() != 1
        || descriptor.dev() != entry.dev()
        || descriptor.ino() != entry.ino()
        || descriptor.mode() & 0o777 != 0o600
    {
        return Err(io::Error::other(
            "diagnostic output identity or privacy invalid",
        ));
    }
    Ok(())
}

struct WriterState {
    dropped: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
    budget_exhausted: Arc<AtomicBool>,
    session_id: u64,
    pid: u32,
    started: Instant,
}
fn write_records<W: Write + Send + 'static>(
    mut file: W,
    rx: mpsc::Receiver<WriterMessage>,
    state: WriterState,
) -> W {
    let mut emitted = 0u64;
    let mut bytes = 0u64;
    for WriterMessage(line) in rx {
        if file.write_all(&line).is_err() {
            state.failed.store(true, Ordering::Release);
            break;
        }
        emitted = emitted.saturating_add(1);
        bytes = bytes.saturating_add(line.len() as u64);
    }
    // Flush ordinary output before choosing its status. Production uses an
    // unbuffered File: the reserved footer is one final direct write, whose
    // failure is reported by close rather than by an impossible second footer.
    if file.flush().is_err() {
        state.failed.store(true, Ordering::Release);
    }
    let status = if state.failed.load(Ordering::Acquire) {
        SummaryStatus::Failed
    } else if state.budget_exhausted.load(Ordering::Acquire) {
        SummaryStatus::BudgetExhausted
    } else {
        SummaryStatus::Closed
    };
    let summary = encode_line(&Summary {
        kind: "Summary",
        session_id: state.session_id,
        pid: state.pid,
        monotonic_ns: state.started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
        emitted,
        dropped: state.dropped.load(Ordering::Acquire),
        bytes,
        status,
    });
    match summary {
        Ok(line) => {
            if file.write_all(&line).is_err() {
                state.failed.store(true, Ordering::Release);
            }
        }
        Err(_) => {
            state.failed.store(true, Ordering::Release);
        }
    }
    file
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc;
    use tempfile::tempdir;

    struct FailingWriter {
        writes: usize,
        fail_after: Option<usize>,
        fail_flush: bool,
        bytes: Vec<u8>,
    }

    struct HeldWriter {
        entered: Arc<AtomicBool>,
        release: Arc<AtomicBool>,
        bytes: Vec<u8>,
    }

    impl Write for HeldWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.entered.store(true, Ordering::Release);
            while !self.release.load(Ordering::Acquire) {
                thread::yield_now();
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.fail_after.is_some_and(|limit| self.writes >= limit) {
                return Err(io::Error::other("fixture write failure"));
            }
            self.writes += 1;
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                Err(io::Error::other("fixture flush failure"))
            } else {
                Ok(())
            }
        }
    }

    fn run_writer(writer: FailingWriter, records: usize) -> (FailingWriter, bool) {
        let (tx, rx) = mpsc::channel();
        for _ in 0..records {
            tx.send(WriterMessage(b"{\"kind\":\"CacheHit\"}\n".to_vec()))
                .unwrap();
        }
        drop(tx);
        let (done_tx, done_rx) = mpsc::channel();
        let failed = Arc::new(AtomicBool::new(false));
        let state = WriterState {
            dropped: Arc::new(AtomicU64::new(0)),
            failed: Arc::clone(&failed),
            budget_exhausted: Arc::new(AtomicBool::new(false)),
            session_id: 1,
            pid: 1,
            started: Instant::now(),
        };
        let handle = thread::spawn(move || {
            let writer = write_records(writer, rx, state);
            let _ = done_tx.send(());
            writer
        });
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let writer = handle.join().unwrap();
        (writer, failed.load(Ordering::Acquire))
    }

    #[test]
    fn diagnostic_activation_is_exclusive_and_typed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let session = DiagnosticSession::open(&path).unwrap();
        session.emit(Event::CacheHit {
            session_id: 1,
            pid: std::process::id(),
            monotonic_ns: 0,
            store_id: 1,
            coordinator_id: 1,
            attempt_id: 1,
            route: Route::StoredKeyring,
            endpoint: Endpoint::Other,
            outcome: auth::CredentialDiagnosticOutcome::CacheHit,
        });
        assert_eq!(session.close(), CloseOutcome::Closed);
        assert_eq!(session.close(), CloseOutcome::Closed);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("CacheHit"));
        assert!(text.contains("Summary"));
        assert!(DiagnosticSession::open(&path).is_err());
    }

    #[test]
    fn session_allocator_saturates_without_reusing_ids() {
        let dir = tempdir().unwrap();
        let session = DiagnosticSession::open(&dir.path().join("events.jsonl")).unwrap();
        let first = session.allocate_id().unwrap();
        assert_eq!(first, 1);
        session.set_next_id_for_test(u64::MAX - 1);
        assert_eq!(session.allocate_id(), Some(u64::MAX - 1));
        assert_eq!(session.allocate_id(), None);
        assert_eq!(session.allocate_id(), None);
    }

    #[test]
    fn writer_failures_mark_incomplete_without_claiming_success() {
        let (_, first_write_failed) = run_writer(
            FailingWriter {
                writes: 0,
                fail_after: Some(0),
                fail_flush: false,
                bytes: Vec::new(),
            },
            1,
        );
        assert!(first_write_failed);

        let (_, summary_write_failed) = run_writer(
            FailingWriter {
                writes: 0,
                fail_after: Some(1),
                fail_flush: false,
                bytes: Vec::new(),
            },
            1,
        );
        assert!(summary_write_failed);

        let (writer, flush_failed) = run_writer(
            FailingWriter {
                writes: 0,
                fail_after: None,
                fail_flush: true,
                bytes: Vec::new(),
            },
            1,
        );
        assert!(flush_failed);
        let text = std::str::from_utf8(&writer.bytes).unwrap();
        let rows: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["kind"], "Summary");
        assert_eq!(rows[1]["sink"], "Failed");
        assert_eq!(rows[1]["emitted"], 1);
        assert!(!text.contains("Closed"));
    }

    #[test]
    fn producer_drops_immediately_when_output_mutex_is_held() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let session = DiagnosticSession::open(&path).unwrap();
        let _guard = session.tx.lock().unwrap();
        let started = Instant::now();
        session.emit(Event::CacheHit {
            session_id: 1,
            pid: 1,
            monotonic_ns: 0,
            store_id: 1,
            coordinator_id: 1,
            attempt_id: 1,
            route: Route::StoredKeyring,
            endpoint: Endpoint::Other,
            outcome: auth::CredentialDiagnosticOutcome::CacheHit,
        });
        assert!(started.elapsed() < Duration::from_millis(50));
        assert_eq!(session.dropped.load(Ordering::Acquire), 1);
    }

    #[test]
    fn close_deadline_and_repeated_close_remain_incomplete_until_writer_finishes() {
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let session = Arc::new(DiagnosticSession::open_with_writer(HeldWriter {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            bytes: Vec::new(),
        }));
        session.emit(Event::CacheHit {
            session_id: 1,
            pid: 1,
            monotonic_ns: 0,
            store_id: 1,
            coordinator_id: 1,
            attempt_id: 1,
            route: Route::StoredKeyring,
            endpoint: Endpoint::Other,
            outcome: auth::CredentialDiagnosticOutcome::CacheHit,
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while !entered.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "writer did not enter");
            thread::yield_now();
        }
        let first = Arc::clone(&session);
        let close_started = Instant::now();
        let first_close = thread::spawn(move || first.close());
        while !session.close_entered.load(Ordering::Acquire) {
            assert!(close_started.elapsed() < Duration::from_secs(1));
            thread::yield_now();
        }
        let second = thread::spawn({
            let session = Arc::clone(&session);
            move || session.close()
        });
        assert_eq!(second.join().unwrap(), CloseOutcome::Incomplete);
        assert!(close_started.elapsed() < Duration::from_millis(100));
        assert!(!release.load(Ordering::Acquire));
        assert!(!first_close.is_finished());
        assert_eq!(first_close.join().unwrap(), CloseOutcome::Incomplete);
        let timeout_elapsed = close_started.elapsed();
        eprintln!("blocked diagnostic close elapsed: {timeout_elapsed:?}");
        assert!(timeout_elapsed >= Duration::from_millis(240));
        assert!(timeout_elapsed < Duration::from_millis(350));
        release.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(1);
        while session
            .join
            .try_lock()
            .ok()
            .is_none_or(|join| join.is_some())
        {
            let _ = session.close();
            assert!(
                Instant::now() < deadline,
                "writer did not finish after release"
            );
            thread::yield_now();
        }
    }

    #[test]
    fn concurrent_emit_reserves_the_inclusive_ordinary_budget() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let session = Arc::new(DiagnosticSession::open(&path).unwrap());
        let mut workers = Vec::new();
        for _ in 0..300 {
            let session = Arc::clone(&session);
            workers.push(thread::spawn(move || {
                session.emit(Event::CacheHit {
                    session_id: 1,
                    pid: 1,
                    monotonic_ns: 0,
                    store_id: 1,
                    coordinator_id: 1,
                    attempt_id: 1,
                    route: Route::StoredKeyring,
                    endpoint: Endpoint::Other,
                    outcome: auth::CredentialDiagnosticOutcome::CacheHit,
                });
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(session.close(), CloseOutcome::Closed);
        let lines = fs::read_to_string(&path).unwrap();
        let summary: serde_json::Value =
            serde_json::from_str(lines.lines().last().unwrap()).unwrap();
        assert_eq!(
            summary["emitted"].as_u64().unwrap() + summary["dropped"].as_u64().unwrap(),
            300
        );
        assert!(summary["emitted"].as_u64().unwrap() <= 255);
        assert!(lines.lines().all(|line| line.len() < MAX_LINE_BYTES));
        assert!(lines.lines().count() <= 256);
        assert!(lines.len() <= 131_072);
        assert!(lines.lines().any(|line| line.contains("Summary")));
    }

    #[cfg(unix)]
    #[test]
    fn activation_rejects_symlink_without_touching_target() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, b"preserve").unwrap();
        let path = dir.path().join("events.jsonl");
        symlink(&target, &path).unwrap();
        assert!(DiagnosticSession::open(&path).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"preserve");
    }

    #[cfg(unix)]
    #[test]
    fn activation_checks_private_descriptor_identity_and_preserves_rejected_paths() {
        use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
        let dir = tempdir().unwrap();
        let path = dir.path().join("private.jsonl");
        let session = DiagnosticSession::open(&path).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(session.close(), CloseOutcome::Closed);
        let original = fs::read(&path).unwrap();
        assert!(DiagnosticSession::open(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(DiagnosticSession::open(dir.path()).is_err());
        assert!(DiagnosticSession::open(Path::new("/dev/null")).is_err());
        let fifo = dir.path().join("fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        assert!(DiagnosticSession::open(&fifo).is_err());
        assert!(fs::symlink_metadata(&fifo).unwrap().file_type().is_fifo());
        let socket = dir.path().join("socket");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        assert!(DiagnosticSession::open(&socket).is_err());
        drop(listener);
        let denied = dir.path().join("denied");
        fs::create_dir(&denied).unwrap();
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o500)).unwrap();
        let result = DiagnosticSession::open(&denied.join("events"));
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
        assert!(!denied.join("events").exists());
        let identity = dir.path().join("identity");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&identity)
            .unwrap();
        fs::hard_link(&identity, dir.path().join("linked")).unwrap();
        assert!(validate_output_file(&identity, &file).is_err());
        fs::remove_file(dir.path().join("linked")).unwrap();
        fs::rename(&identity, dir.path().join("original")).unwrap();
        let other = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&identity)
            .unwrap();
        drop(other);
        assert!(validate_output_file(&identity, &file).is_err());
    }

    #[test]
    fn exact_byte_record_line_bounds_and_trailing_drops_use_real_admission() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("budget");
        let session = DiagnosticSession::open(&path).unwrap();
        let mut exact = b"{\"kind\":\"CacheHit\"}".to_vec();
        exact.resize(511, b' ');
        exact.push(b'\n');
        for _ in 0..255 {
            session.admit_line(exact.clone());
        }
        session.admit_line(exact.clone());
        session.admit_line(exact);
        assert_eq!(session.close(), CloseOutcome::Closed);
        let raw = fs::read(&path).unwrap();
        assert!(raw.len() <= 131072);
        let lines: Vec<_> = std::str::from_utf8(&raw).unwrap().lines().collect();
        assert_eq!(lines.len(), 256);
        assert!(lines[..255].iter().all(|l| l.len() + 1 == 512));
        let summary: serde_json::Value = serde_json::from_str(lines[255]).unwrap();
        assert_eq!(summary["emitted"], 255);
        assert_eq!(summary["bytes"], 130560);
        assert_eq!(summary["dropped"], 2);
        assert_eq!(summary["sink"], "BudgetExhausted");
        assert_eq!(
            encode_line(&"x".repeat(510)),
            Err(EncodingFailure::LineLimit)
        );
        assert_eq!(encode_line(&"x".repeat(509)).unwrap().len(), 512);
    }

    #[test]
    fn queue_pressure_and_saturated_drops_are_observational() {
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let session = DiagnosticSession::from_writer_with_capacity(
            HeldWriter {
                entered: entered.clone(),
                release: release.clone(),
                bytes: Vec::new(),
            },
            1,
        )
        .unwrap();
        session.admit_line(b"{}\n".to_vec());
        let deadline = Instant::now() + Duration::from_secs(1);
        while !entered.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        session.admit_line(b"{}\n".to_vec());
        session.admit_line(b"{}\n".to_vec());
        assert_eq!(session.dropped.load(Ordering::Acquire), 1);
        session.dropped.store(u64::MAX, Ordering::Release);
        session.admit_line(b"{}\n".to_vec());
        assert_eq!(session.dropped.load(Ordering::Acquire), u64::MAX);
        release.store(true, Ordering::Release);
        assert_eq!(session.close(), CloseOutcome::Closed);
    }

    #[test]
    fn close_with_held_mutex_is_bounded_and_reaps_after_release() {
        for which in [0, 1, 2] {
            let dir = tempdir().unwrap();
            let session = DiagnosticSession::open(&dir.path().join("events")).unwrap();
            let started = Instant::now();
            match which {
                0 => {
                    let guard = session.tx.lock().unwrap();
                    assert_eq!(session.close(), CloseOutcome::Incomplete);
                    drop(guard);
                }
                1 => {
                    let guard = session.done.lock().unwrap();
                    assert_eq!(session.close(), CloseOutcome::Incomplete);
                    drop(guard);
                }
                _ => {
                    let guard = session.join.lock().unwrap();
                    assert_eq!(session.close(), CloseOutcome::Incomplete);
                    drop(guard);
                }
            }
            assert!(started.elapsed() < Duration::from_millis(350));
            let deadline = Instant::now() + Duration::from_secs(1);
            while session.join.lock().unwrap().is_some() {
                assert_eq!(session.close(), CloseOutcome::Incomplete);
                assert!(Instant::now() < deadline);
                thread::yield_now();
            }
        }
    }

    #[test]
    fn exhaustion_disables_encoding_without_reusing_zero_ids() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("exhaustion");
        let session = DiagnosticSession::open(&path).unwrap();
        session.set_next_id_for_test(u64::MAX);
        assert_eq!(session.allocate_id(), None);
        session.emit(Event::CacheHit {
            session_id: session.session_id,
            pid: session.pid,
            monotonic_ns: 0,
            store_id: 1,
            coordinator_id: 2,
            attempt_id: 3,
            route: Route::StoredKeyring,
            endpoint: Endpoint::Other,
            outcome: auth::CredentialDiagnosticOutcome::CacheHit,
        });
        assert_eq!(session.close(), CloseOutcome::Incomplete);
        let raw = fs::read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), 1);
        let summary: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(summary["sink"], "Failed");
        assert_eq!(summary["dropped"], 1);
    }
}
