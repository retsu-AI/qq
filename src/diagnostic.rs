use qq_auth as auth;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const MAX_ORDINARY_RECORDS: u64 = 255;
const MAX_ORDINARY_BYTES: u64 = 130_560;
const MAX_LINE_BYTES: usize = 512;
const CLOSE_TIMEOUT: Duration = Duration::from_millis(250);

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
    CacheHit {
        store_id: u64,
        coordinator_id: u64,
        attempt_id: u64,
        route: Route,
        endpoint: Endpoint,
    },
    BackendReadStarted {
        store_id: u64,
        coordinator_id: u64,
        flight_id: u64,
        read_id: u64,
        route: Route,
        endpoint: Endpoint,
    },
    BackendReadReturned {
        store_id: u64,
        coordinator_id: u64,
        flight_id: u64,
        read_id: u64,
        route: Route,
        endpoint: Endpoint,
    },
}

impl auth::CredentialDiagnosticSink for DiagnosticSession {
    fn emit(&self, event: auth::CredentialDiagnosticEvent) {
        let event = match event {
            auth::CredentialDiagnosticEvent::CacheHit {
                store_id,
                coordinator_id,
                attempt_id,
                route: auth::CredentialDiagnosticRoute::StoredKeyring,
                endpoint,
            } => Event::CacheHit {
                store_id,
                coordinator_id,
                attempt_id,
                route: Route::StoredKeyring,
                endpoint: endpoint.into(),
            },
            auth::CredentialDiagnosticEvent::BackendReadStarted {
                store_id,
                coordinator_id,
                flight_id,
                read_id,
                route: auth::CredentialDiagnosticRoute::StoredKeyring,
                endpoint,
            } => Event::BackendReadStarted {
                store_id,
                coordinator_id,
                flight_id,
                read_id,
                route: Route::StoredKeyring,
                endpoint: endpoint.into(),
            },
            auth::CredentialDiagnosticEvent::BackendReadReturned {
                store_id,
                coordinator_id,
                flight_id,
                read_id,
                route: auth::CredentialDiagnosticRoute::StoredKeyring,
                endpoint,
            } => Event::BackendReadReturned {
                store_id,
                coordinator_id,
                flight_id,
                read_id,
                route: Route::StoredKeyring,
                endpoint: endpoint.into(),
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
            auth::CredentialDiagnosticEndpoint::Other => Self::Other,
        }
    }
}

struct WriterMessage(Vec<u8>);

pub(crate) struct DiagnosticSession {
    session_id: u64,
    pid: u32,
    tx: Mutex<Option<mpsc::SyncSender<WriterMessage>>>,
    emitted: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
    closed: AtomicBool,
    join: Mutex<Option<JoinHandle<()>>>,
    done: Mutex<mpsc::Receiver<()>>,
}

impl DiagnosticSession {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let file = options.open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
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
            session_id: 1,
            pid: std::process::id(),
            tx: Mutex::new(Some(tx)),
            emitted,
            dropped,
            bytes,
            failed,
            closed: AtomicBool::new(false),
            join: Mutex::new(Some(join)),
            done: Mutex::new(done_rx),
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
        if line.len() > MAX_LINE_BYTES
            || self.emitted.load(Ordering::Relaxed) >= MAX_ORDINARY_RECORDS
            || self.bytes.load(Ordering::Relaxed) + line.len() as u64 > MAX_ORDINARY_BYTES
        {
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
}
