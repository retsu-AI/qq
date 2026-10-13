use qq_auth as auth;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const MAX_ORDINARY_RECORDS: u64 = 255;
const MAX_ORDINARY_BYTES: u64 = 130_560;
const MAX_LINE_BYTES: usize = 512;
const CLOSE_TIMEOUT: Duration = Duration::from_millis(250);
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

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
        coordinator_id: u64,
        flight_id: u64,
        read_id: u64,
        route: Route,
        endpoint: Endpoint,
    },
    BackendReadReturned {
        session_id: u64,
        pid: u32,
        monotonic_ns: u64,
        store_id: u64,
        coordinator_id: u64,
        flight_id: u64,
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
        }
    }
}

struct WriterMessage(Vec<u8>);

pub(crate) struct DiagnosticSession {
    session_id: u64,
    pid: u32,
    tx: Mutex<Option<mpsc::SyncSender<WriterMessage>>>,
    dropped: Arc<AtomicU64>,
    reserved_records: AtomicU64,
    reserved_bytes: AtomicU64,
    failed: Arc<AtomicBool>,
    closed: AtomicBool,
    join: Mutex<Option<JoinHandle<()>>>,
    done: Mutex<mpsc::Receiver<()>>,
    started: Instant,
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
        let (tx, rx) = mpsc::sync_channel(255);
        let (done_tx, done_rx) = mpsc::channel();
        let emitted = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicU64::new(0));
        let bytes = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicBool::new(false));
        let writer_emitted = Arc::clone(&emitted);
        let writer_dropped = Arc::clone(&dropped);
        let writer_bytes = Arc::clone(&bytes);
        let writer_failed = Arc::clone(&failed);
        let join = thread::Builder::new()
            .name("qq-diagnostic-writer".to_owned())
            .spawn(move || {
                write_records(
                    file,
                    rx,
                    done_tx,
                    writer_emitted,
                    writer_dropped,
                    writer_bytes,
                    writer_failed,
                )
            })?;
        Ok(Self {
            session_id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
            pid: std::process::id(),
            tx: Mutex::new(Some(tx)),
            dropped,
            reserved_records: AtomicU64::new(0),
            reserved_bytes: AtomicU64::new(0),
            failed,
            closed: AtomicBool::new(false),
            join: Mutex::new(Some(join)),
            done: Mutex::new(done_rx),
            started: Instant::now(),
        })
    }

    pub(crate) fn emit(&self, event: Event) {
        let mut line = match serde_json::to_vec(&event) {
            Ok(line) => line,
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        line.push(b'\n');
        let line_bytes = line.len() as u64;
        if line.len() > MAX_LINE_BYTES || !self.reserve(line_bytes) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let Ok(tx) = self.tx.lock() else { return };
        let Some(tx) = tx.as_ref() else { return };
        match tx.try_send(WriterMessage(line)) {
            Ok(()) => {}
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn reserve(&self, line_bytes: u64) -> bool {
        let mut records = self.reserved_records.load(Ordering::Acquire);
        loop {
            if records >= MAX_ORDINARY_RECORDS {
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

    pub(crate) fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(mut tx) = self.tx.lock() {
            tx.take();
        }
        let done = self
            .done
            .lock()
            .ok()
            .and_then(|done| done.recv_timeout(CLOSE_TIMEOUT).ok());
        if done.is_some() {
            if let Ok(mut join) = self.join.lock() {
                if let Some(join) = join.take() {
                    let _ = join.join();
                }
            }
        } else {
            self.failed.store(true, Ordering::Relaxed);
        }
        let _ = (self.session_id, self.pid);
    }
}

impl Drop for DiagnosticSession {
    fn drop(&mut self) {
        self.close();
    }
}

fn write_records(
    mut file: File,
    rx: mpsc::Receiver<WriterMessage>,
    done: mpsc::Sender<()>,
    emitted: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
) {
    for WriterMessage(line) in rx {
        if file.write_all(&line).is_err() {
            failed.store(true, Ordering::Relaxed);
            break;
        }
        emitted.fetch_add(1, Ordering::Relaxed);
        bytes.fetch_add(line.len() as u64, Ordering::Relaxed);
    }
    let summary = format!(
        "{{\"kind\":\"Summary\",\"emitted\":{},\"dropped\":{},\"bytes\":{},\"sink\":\"{}\"}}\n",
        emitted.load(Ordering::Relaxed),
        dropped.load(Ordering::Relaxed),
        bytes.load(Ordering::Relaxed),
        if failed.load(Ordering::Relaxed) {
            "Failed"
        } else {
            "Closed"
        },
    );
    if !failed.load(Ordering::Relaxed) && file.write_all(summary.as_bytes()).is_err() {
        failed.store(true, Ordering::Relaxed);
    }
    let _ = file.flush();
    let _ = done.send(());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

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
        session.close();
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
        session.close();
        let lines = fs::read_to_string(&path).unwrap();
        assert_eq!(lines.lines().filter(|line| line.contains("CacheHit")).count(), 255);
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
