//! Opt-in metadata-only tracing. The subscriber rejects all dependency targets.
use std::{
    fs::File,
    io::{self, Write},
    path::Path,
    sync::mpsc::{self, SyncSender},
    thread,
};

mod subscriber;

#[cfg(not(unix))]
use std::fs::OpenOptions;

const MAX_RECORD: usize = 4096;
const MAX_FILE: usize = 8 * 1024 * 1024;
const QUEUE: usize = 256;

#[derive(Debug, thiserror::Error)]
pub(crate) enum LogError {
    #[error("QQ_LOG must be off or info")]
    Value,
    #[error("cannot initialize diagnostic log: {0}")]
    Io(#[from] io::Error),
    #[error("QQ_LOG is not supported with --tui-qa-root; unset QQ_LOG")]
    QaRoot,
    #[error("a tracing subscriber is already installed")]
    Subscriber,
    #[error("diagnostic writer failed")]
    Worker,
}

pub(crate) fn enabled(value: Option<&std::ffi::OsStr>) -> Result<bool, LogError> {
    match value {
        None => Ok(false),
        Some(value) if value == "off" || value.is_empty() => Ok(false),
        Some(value) if value == "info" => Ok(true),
        Some(_) => Err(LogError::Value),
    }
}

pub(crate) struct LogGuard {
    dispatch: Option<tracing::Dispatch>,

    stop: SyncSender<Vec<u8>>,
    worker: thread::JoinHandle<io::Result<()>>,
}

impl LogGuard {
    // Called off the async executor, including directory and file creation.
    pub(crate) fn start(directory: &Path) -> Result<Self, LogError> {
        let directory = open_log_directory(directory)?;
        let mut file = None;
        // Fixed create-new slots bound retained disk use across processes without
        // deleting another process's log or reopening an attacker-supplied file.
        for slot in 0..16 {
            match create_log(&directory, slot) {
                Ok(opened) => {
                    file = Some(opened);
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        let file =
            file.ok_or_else(|| io::Error::other("log slots full; remove old diagnostic logs"))?;
        let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(QUEUE);
        let worker = thread::Builder::new()
            .name("qq-log".into())
            .spawn(move || write_records(file, receiver))?;
        let dispatch =
            tracing::Dispatch::new(subscriber::DiagnosticSubscriber::new(sender.clone()));
        Ok(Self {
            dispatch: Some(dispatch),
            stop: sender,
            worker,
        })
    }

    pub(crate) fn install(&self) -> Result<(), LogError> {
        tracing::dispatcher::set_global_default(
            self.dispatch.as_ref().expect("live logger").clone(),
        )
        .map_err(|_| LogError::Subscriber)
    }

    pub(crate) fn finish(mut self) -> Result<(), LogError> {
        // The process-global subscriber remains alive, so explicit shutdown is
        // sent in-band after all runtime tasks have settled.
        if let Some(dispatch) = self.dispatch.take() {
            drop(dispatch);
        }
        // A global subscriber retains its sender. The worker uses an explicit
        // empty-record sentinel instead of waiting for channel disconnection.
        self.stop.send(Vec::new()).map_err(|_| LogError::Worker)?;
        self.worker.join().map_err(|_| LogError::Worker)??;
        Ok(())
    }
}

#[cfg(unix)]
fn open_log_directory(path: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags, mkdirat, open, openat};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    if !path.is_absolute() {
        return Err(io::Error::other("log directory must be absolute"));
    }
    let mut directory = open("/", flags, Mode::empty())?;
    for component in path.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(name) => {
                match mkdirat(&directory, name, Mode::from_bits_truncate(0o700)) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(error) => return Err(error.into()),
                }
                directory = openat(&directory, name, flags, Mode::empty())?;
            }
            _ => return Err(io::Error::other("invalid log directory component")),
        }
    }
    rustix::fs::fchmod(&directory, Mode::from_bits_truncate(0o700))?;
    Ok(File::from(directory))
}

#[cfg(unix)]
fn create_log(directory: &File, slot: usize) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};
    Ok(File::from(rustix::fs::openat(
        directory,
        format!("qq-{slot}.jsonl"),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )?))
}

#[cfg(not(unix))]
fn open_log_directory(_path: &Path) -> io::Result<std::path::PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "QQ_LOG requires Unix no-follow directory traversal on this build",
    ))
}

#[cfg(not(unix))]
fn create_log(directory: &Path, slot: usize) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join(format!("qq-{slot}.jsonl")))
}

fn write_records(mut file: File, receiver: mpsc::Receiver<Vec<u8>>) -> io::Result<()> {
    let mut written = 0;
    for record in receiver {
        if record.is_empty() {
            break;
        }
        if written + record.len() <= MAX_FILE {
            file.write_all(&record)?;
            written += record.len();
        }
    }
    file.flush()
}

#[cfg(test)]
#[derive(Clone)]
struct RecordWriter(SyncSender<Vec<u8>>);

#[cfg(test)]
struct Record {
    sender: SyncSender<Vec<u8>>,
    bytes: Vec<u8>,
    overflow: bool,
}

#[cfg(test)]
impl RecordWriter {
    fn make_writer(&self) -> Record {
        Record {
            sender: self.0.clone(),
            bytes: Vec::new(),
            overflow: false,
        }
    }
}

#[cfg(test)]
impl Write for Record {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len() + bytes.len() <= MAX_RECORD && !self.overflow {
            self.bytes.extend_from_slice(bytes);
        } else {
            self.overflow = true;
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
impl Drop for Record {
    fn drop(&mut self) {
        if !self.overflow && !self.bytes.is_empty() {
            // Diagnostics are lossy under pressure; agent work never waits on disk.
            let _ = self.sender.try_send(std::mem::take(&mut self.bytes));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn dependency_secrets_are_rejected_and_span_metadata_survives() {
        let home = tempfile::tempdir().unwrap();
        let directory = home.path().join("logs");
        let logger = LogGuard::start(&directory).unwrap();
        tracing::dispatcher::with_default(logger.dispatch.as_ref().unwrap(), || {
            let run = tracing::info_span!(target: "qq::diagnostic", "run");
            run.in_scope(|| {
                tracing::info!(target: "reqwest", credential = "SECRET_CREDENTIAL", "SECRET_ERROR");
                let turn = tracing::info_span!(target: "qq::diagnostic", "turn", turn_ordinal = 1);
                turn.in_scope(|| {
                    let provider = tracing::info_span!(target: "qq::diagnostic", "provider");
                    provider
                        .in_scope(|| tracing::warn!(target: "provider", "SECRET_PROVIDER_BODY"));
                });
            });
        });
        logger.finish().unwrap();
        let logs = std::fs::read_to_string(directory.join("qq-0.jsonl")).unwrap();
        assert!(!logs.contains("SECRET_"));
        let records: Vec<serde_json::Value> = logs
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(records.iter().any(|record| {
            record["span"]["name"] == "provider"
                && record["spans"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|span| span["name"] == "run")
        }));
        assert!(records.iter().any(
            |record| record["span"]["name"] == "run" && record["fields"]["message"] == "close"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn exclusive_slots_and_io_errors_are_explicit() {
        let home = tempfile::tempdir().unwrap();
        let directory = home.path().join("logs");
        for _ in 0..16 {
            LogGuard::start(&directory).unwrap().finish().unwrap();
        }
        assert!(matches!(LogGuard::start(&directory), Err(LogError::Io(_))));
        assert_eq!(std::fs::read_dir(directory).unwrap().count(), 16);
        let file = home.path().join("not-a-directory");
        std::fs::write(&file, "unchanged").unwrap();
        assert!(LogGuard::start(&file).is_err());
        assert_eq!(std::fs::read_to_string(file).unwrap(), "unchanged");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directory_is_refused_without_changing_target() {
        use std::os::unix::{fs::PermissionsExt as _, fs::symlink};
        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = home.path().join("link");
        symlink(&target, &link).unwrap();
        assert!(LogGuard::start(&link.join("logs")).is_err());
        assert!(!target.join("logs").exists());
        assert_eq!(
            std::fs::metadata(target).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn opt_in_is_explicit() {
        for (value, expected) in [
            (None, false),
            (Some(""), false),
            (Some("off"), false),
            (Some("info"), true),
        ] {
            assert_eq!(enabled(value.map(std::ffi::OsStr::new)).unwrap(), expected);
        }
        assert!(enabled(Some(std::ffi::OsStr::new("debug"))).is_err());
    }

    #[test]
    fn records_are_bounded_and_never_partially_written() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let writer = RecordWriter(sender);
        {
            let mut record = writer.make_writer();
            record.write_all(&vec![b'x'; MAX_RECORD + 1]).unwrap();
        }
        assert!(receiver.try_recv().is_err());
        {
            let mut record = writer.make_writer();
            record.write_all(b"{}\n").unwrap();
        }
        {
            let mut record = writer.make_writer();
            record.write_all(b"dropped\n").unwrap();
        }
        assert_eq!(receiver.try_recv().unwrap(), b"{}\n");
        assert!(receiver.try_recv().is_err());
    }
}
