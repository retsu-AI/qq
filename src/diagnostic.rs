use qq_auth as auth;
use serde::Serialize;
use std::fs::OpenOptions;
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
    monotonic_ns: u128,
    emitted: u64,
    dropped: u64,
    bytes: u64,
    status: SummaryStatus,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(tag = "kind")]
pub(crate) enum Event {
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
    },
    BackendReadStarted {
        session_id: u64,
        pid: u32,
        monotonic_ns: u64,
        store_id: u64,
        coordinator_id: Option<u64>,
        flight_id: Option<u64>,
        read_id: u64,
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
        route: Route,
        endpoint: Endpoint,
    },
    ResolutionFinished {
        session_id: u64,
        pid: u32,
        monotonic_ns: u64,
        store_id: u64,
        coordinator_id: u64,
        attempt_id: u64,
        flight_id: u64,
        route: Route,
        endpoint: Endpoint,
        outcome: auth::CredentialDiagnosticOutcome,
    },
}

impl auth::CredentialDiagnosticSink for DiagnosticSession {
    fn emit(&self, event: auth::CredentialDiagnosticEvent) {
        let (session_id, pid, monotonic_ns) = self.metadata();
        let event = match event {
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
            } => Event::CacheHit {
                session_id,
                pid,
                monotonic_ns,
                store_id,
                coordinator_id,
                attempt_id,
                route: route.into(),
                endpoint: endpoint.into(),
            },
            auth::CredentialDiagnosticEvent::BackendReadStarted {
                store_id,
                coordinator_id,
                flight_id,
                read_id,
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
                route: route.into(),
                endpoint: endpoint.into(),
            },
            auth::CredentialDiagnosticEvent::BackendReadReturned {
                store_id,
                coordinator_id,
                flight_id,
                read_id,
                route,
                endpoint,
            } => Event::BackendReadReturned {
                session_id,
                pid,
                monotonic_ns,
                store_id,
                coordinator_id,
                flight_id,
                read_id,
                route: route.into(),
                endpoint: endpoint.into(),
            },
            auth::CredentialDiagnosticEvent::ResolutionFinished {
                store_id,
                coordinator_id,
                attempt_id,
                flight_id,
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
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        Self::from_writer(file)
    }

    fn from_writer<W: Write + Send + 'static>(file: W) -> io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(255);
        let (done_tx, done_rx) = mpsc::channel();
        let emitted = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicU64::new(0));
        let bytes = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicBool::new(false));
        let budget_exhausted = Arc::new(AtomicBool::new(false));
        let writer_emitted = Arc::clone(&emitted);
        let writer_dropped = Arc::clone(&dropped);
        let writer_bytes = Arc::clone(&bytes);
        let writer_failed = Arc::clone(&failed);
        let writer_budget_exhausted = Arc::clone(&budget_exhausted);
        let session_id = allocate_saturating(&NEXT_SESSION_ID).unwrap_or(0);
        let pid = std::process::id();
        let started = Instant::now();
        let join = thread::Builder::new()
            .name("qq-diagnostic-writer".to_owned())
            .spawn(move || {
                let _ = write_records(
                    file,
                    rx,
                    done_tx,
                    writer_emitted,
                    writer_dropped,
                    writer_bytes,
                    writer_failed,
                    writer_budget_exhausted,
                    session_id,
                    pid,
                    started,
                );
            })?;
        Ok(Self {
            session_id,
            next_id: AtomicU64::new(1),
            pid: std::process::id(),
            tx: Mutex::new(Some(tx)),
            dropped,
            reserved_records: AtomicU64::new(0),
            reserved_bytes: AtomicU64::new(0),
            budget_exhausted,
            failed,
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
    fn open_with_writer<W: Write + Send + 'static>(writer: W) -> Self {
        Self::from_writer(writer).expect("diagnostic writer fixture must start")
    }

    #[cfg(test)]
    fn set_next_id_for_test(&self, value: u64) {
        self.next_id.store(value, Ordering::Release);
    }

    pub(crate) fn allocate_id(&self) -> Option<u64> {
        allocate_saturating(&self.next_id)
    }

    pub(crate) fn emit(&self, event: Event) {
        let mut line = match serde_json::to_vec(&event) {
            Ok(line) => line,
            Err(_) => {
                saturating_increment(&self.dropped);
                return;
            }
        };
        line.push(b'\n');
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
        let Some(tx) = tx.as_ref() else { return };
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
        #[cfg(test)]
        self.close_entered.store(true, Ordering::Release);
        if self.closed.swap(true, Ordering::AcqRel) {
            if self.close_outcome.load(Ordering::Acquire) == 0 {
                return CloseOutcome::Incomplete;
            }
            if let Ok(done) = self.done.try_lock() {
                if done.try_recv().is_ok() {
                    if let Ok(mut join) = self.join.try_lock() {
                        if let Some(join) = join.take() {
                            if join.join().is_err() {
                                self.close_outcome.store(2, Ordering::Release);
                            }
                        }
                    }
                }
            }
            return match self.close_outcome.load(Ordering::Acquire) {
                2 => CloseOutcome::Incomplete,
                _ => CloseOutcome::Closed,
            };
        }
        if let Ok(mut tx) = self.tx.try_lock() {
            tx.take();
        } else {
            self.failed.store(true, Ordering::Release);
        }
        let done = self
            .done
            .try_lock()
            .ok()
            .and_then(|done| done.recv_timeout(CLOSE_TIMEOUT).ok());
        if done.is_some() {
            let joined = if let Ok(mut join) = self.join.try_lock() {
                if let Some(join) = join.take() {
                    join.join().is_ok()
                } else {
                    false
                }
            } else {
                false
            };
            if !joined {
                self.failed.store(true, Ordering::Release);
            }
            if self.failed.load(Ordering::Acquire) {
                self.close_outcome.store(2, Ordering::Release);
                CloseOutcome::Incomplete
            } else {
                self.close_outcome.store(1, Ordering::Release);
                CloseOutcome::Closed
            }
        } else {
            self.failed.store(true, Ordering::Relaxed);
            self.close_outcome.store(2, Ordering::Release);
            CloseOutcome::Incomplete
        }
    }
}

impl Drop for DiagnosticSession {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn write_records<W: Write + Send + 'static>(
    mut file: W,
    rx: mpsc::Receiver<WriterMessage>,
    done: mpsc::Sender<()>,
    emitted: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
    budget_exhausted: Arc<AtomicBool>,
    session_id: u64,
    pid: u32,
    started: Instant,
) -> W {
    for WriterMessage(line) in rx {
        if file.write_all(&line).is_err() {
            failed.store(true, Ordering::Relaxed);
            break;
        }
        emitted.fetch_add(1, Ordering::Relaxed);
        bytes.fetch_add(line.len() as u64, Ordering::Relaxed);
    }
    let status = if failed.load(Ordering::Acquire) {
        SummaryStatus::Failed
    } else if budget_exhausted.load(Ordering::Acquire) {
        SummaryStatus::BudgetExhausted
    } else {
        SummaryStatus::Closed
    };
    let mut summary = serde_json::to_vec(&Summary {
        kind: "Summary",
        session_id,
        pid,
        monotonic_ns: started.elapsed().as_nanos(),
        emitted: emitted.load(Ordering::Relaxed),
        dropped: dropped.load(Ordering::Relaxed),
        bytes: bytes.load(Ordering::Relaxed),
        status,
    })
    .unwrap_or_default();
    summary.push(b'\n');
    if summary.len() > MAX_LINE_BYTES
        || (!failed.load(Ordering::Relaxed) && file.write_all(&summary).is_err())
    {
        failed.store(true, Ordering::Relaxed);
    }
    if file.flush().is_err() {
        failed.store(true, Ordering::Release);
    }
    let _ = done.send(());
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
        let emitted = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicU64::new(0));
        let bytes = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicBool::new(false));
        let budget = Arc::new(AtomicBool::new(false));
        let emitted_ref = Arc::clone(&emitted);
        let dropped_ref = Arc::clone(&dropped);
        let bytes_ref = Arc::clone(&bytes);
        let failed_ref = Arc::clone(&failed);
        let budget_ref = Arc::clone(&budget);
        let handle = thread::spawn(move || {
            write_records(
                writer,
                rx,
                done_tx,
                emitted_ref,
                dropped_ref,
                bytes_ref,
                failed_ref,
                budget_ref,
                1,
                1,
                Instant::now(),
            )
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
        });
        assert_eq!(session.close(), CloseOutcome::Closed);
        assert_eq!(session.close(), CloseOutcome::Closed);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("CacheHit"));
        assert!(text.contains("Summary"));
        assert!(DiagnosticSession::open(&path).is_err());
    }

    #[test]
    fn default_path_is_silent() {
        let dir = tempdir().unwrap();
        assert!(!dir.path().join("events.jsonl").exists());
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

        let (_, flush_failed) = run_writer(
            FailingWriter {
                writes: 0,
                fail_after: None,
                fail_flush: true,
                bytes: Vec::new(),
            },
            1,
        );
        assert!(flush_failed);
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
        assert!(first_close.is_finished() == false);
        let timeout_started = Instant::now();
        assert_eq!(first_close.join().unwrap(), CloseOutcome::Incomplete);
        let timeout_elapsed = timeout_started.elapsed();
        eprintln!("blocked diagnostic close elapsed: {timeout_elapsed:?}");
        assert!(timeout_elapsed >= Duration::from_millis(200));
        assert!(timeout_elapsed < Duration::from_millis(400));
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
                });
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(session.close(), CloseOutcome::Closed);
        let lines = fs::read_to_string(&path).unwrap();
        assert_eq!(
            lines
                .lines()
                .filter(|line| line.contains("CacheHit"))
                .count(),
            255
        );
        assert!(lines.lines().all(|line| line.len() < MAX_LINE_BYTES));
        assert!(lines.lines().count() <= 256);
        assert!(lines.as_bytes().len() <= 131_072);
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
}
