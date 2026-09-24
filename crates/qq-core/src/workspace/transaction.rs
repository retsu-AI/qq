//! Journaled patch transactions (ADR-0042 § 1).
//!
//! Every batch of file replacements a built-in tool applies runs as one
//! transaction: the before- and after-bytes of every file are staged as
//! content-addressed blobs under `.qq/transactions/<id>/blobs/`, the journal
//! is written as `applying`, each file is renamed into place and recorded,
//! and the journal is closed as `complete`. A failure midway rolls the
//! completed writes back in reverse order. A journal found `applying` or
//! `rolling_back` when the workspace is next opened is a torn transaction
//! and is rolled back before any tool runs.
//!
//! Rollback and re-apply are fail-closed: a file whose current hash is not
//! the hash the journal expects has been changed by something else since,
//! so it is left alone and the transaction is marked `failed` with the
//! conflicting path. Nothing here consults the session file-state map; the
//! journal is the only source of truth.
//!
//! Journal and blob writes are temp + `fsync` + rename (+ directory `fsync`
//! on Unix), like the file writes they describe. Everything goes through the
//! workspace `Dir` capability, so a symlink under `.qq/` cannot redirect a
//! blob or a journal outside the workspace.

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{Workspace, content_hash};

/// Workspace-relative directory that holds one subdirectory per transaction.
pub(crate) const TRANSACTIONS_DIR: &str = ".qq/transactions";
/// Newest transactions kept after a commit; older ones are removed.
pub const MAX_RETAINED_TRANSACTIONS: usize = 64;
const JOURNAL_FILE: &str = "journal.json";
const BLOBS_DIR: &str = "blobs";
const SCHEMA_VERSION: u32 = 1;
static ORDINAL: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
struct ApplyHook {
    workspace: PathBuf,
    entered: tokio::sync::oneshot::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static APPLY_HOOKS: std::sync::Mutex<Vec<ApplyHook>> = std::sync::Mutex::new(Vec::new());

/// Pauses the next file rename in `workspace` until the returned sender
/// fires; the receiver resolves when the rename is reached.
#[cfg(test)]
pub(crate) fn hold_tool_apply(
    workspace: &Path,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    APPLY_HOOKS.lock().unwrap().push(ApplyHook {
        workspace: workspace.to_owned(),
        entered,
        release: release_rx,
    });
    (entered_rx, release)
}

/// Where a transaction stands. The journal is first written as `applying`;
/// a transaction directory without a journal is still preparing and is
/// discarded on recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionStatus {
    Applying,
    Complete,
    RollingBack,
    RolledBack,
    /// Rollback hit a conflict or an I/O error; `conflict`/`error` say which.
    Failed,
}

/// One file the transaction replaces. `before_hash` is `None` when the file
/// did not exist; rolling such a write back removes the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalWrite {
    pub path: String,
    pub before_hash: Option<String>,
    pub after_hash: String,
}

/// A file whose current content was not what the journal expected when a
/// rollback or re-apply reached it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub path: String,
    pub expected: Option<String>,
    pub actual: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Journal {
    pub schema_version: u32,
    pub id: String,
    pub tool: String,
    pub started_at_unix_ms: u64,
    pub status: TransactionStatus,
    pub planned: Vec<JournalWrite>,
    /// Paths from `planned` whose after-bytes are on disk, in apply order.
    pub completed: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict: Option<Conflict>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A file replacement staged for one transaction.
pub(crate) struct StagedWrite {
    /// Contained, workspace-relative path.
    pub(crate) path: PathBuf,
    /// Current bytes, or `None` for a file that does not exist yet.
    pub(crate) before: Option<Vec<u8>>,
    pub(crate) after: Vec<u8>,
    /// Permissions to carry onto the replacement; `None` takes the default.
    pub(crate) permissions: Option<cap_std::fs::Permissions>,
}

/// What a committed transaction hands back for the tool result.
#[derive(Debug)]
pub(crate) struct TransactionReceipt {
    pub(crate) id: String,
}

impl TransactionReceipt {
    /// The first eight hex digits of the id's random suffix, as the result
    /// header shows them (`tx:<id8>`).
    pub(crate) fn short(&self) -> &str {
        short_id(&self.id)
    }
}

fn short_id(id: &str) -> &str {
    let suffix = id.rsplit('-').next().unwrap_or(id);
    suffix.get(..8).unwrap_or(suffix)
}

/// How the completed writes stood after a failed apply was unwound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RollbackOutcome {
    RolledBack,
    Conflict(Conflict),
    Failed { path: String, error: String },
}

impl std::fmt::Display for RollbackOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RolledBack => formatter.write_str("every applied file was restored"),
            Self::Conflict(conflict) => write!(
                formatter,
                "rollback_conflict: {} changed underneath the transaction and was left as is",
                conflict.path
            ),
            Self::Failed { path, error } => {
                write!(formatter, "rollback_failed: {path}: {error}")
            }
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum TransactionError {
    /// The journal or a blob could not be staged; no file was touched.
    #[error("could not journal the transaction: {source}")]
    Journal {
        #[source]
        source: std::io::Error,
    },
    /// A file write failed after `completed` files had been renamed; those
    /// were unwound with the given outcome.
    #[error(transparent)]
    Apply(Box<ApplyFailure>),
}

/// A write that failed after earlier writes in its transaction had landed.
#[derive(Debug, Error)]
#[error("tx:{} failed at {path}: {error}; {outcome}", short_id(id))]
pub(crate) struct ApplyFailure {
    pub(crate) id: String,
    pub(crate) path: String,
    pub(crate) error: String,
    pub(crate) outcome: RollbackOutcome,
}

/// Runs `writes` as one transaction. The caller holds the workspace apply
/// lock and has already verified currency; every `before` is the bytes on
/// disk right now.
pub(crate) fn run(
    workspace: &Workspace,
    tool: &str,
    writes: &[StagedWrite],
) -> Result<TransactionReceipt, TransactionError> {
    let root = workspace.root();
    let id = new_id(workspace.path());
    let dir = Path::new(TRANSACTIONS_DIR).join(&id);
    let blobs = dir.join(BLOBS_DIR);
    let staged = ensure_transactions_dir(workspace)
        .and_then(|()| root.create_dir(&dir))
        .and_then(|()| root.create_dir(&blobs))
        .and_then(|()| {
            for write in writes {
                if let Some(before) = &write.before {
                    write_blob(workspace, &blobs, before)?;
                }
                write_blob(workspace, &blobs, &write.after)?;
            }
            Ok(())
        });
    if let Err(source) = staged {
        let _ = root.remove_dir_all(&dir);
        return Err(TransactionError::Journal { source });
    }
    let mut journal = Journal {
        schema_version: SCHEMA_VERSION,
        id: id.clone(),
        tool: tool.to_owned(),
        started_at_unix_ms: unix_ms(),
        status: TransactionStatus::Applying,
        planned: writes
            .iter()
            .map(|write| JournalWrite {
                path: write.path.to_string_lossy().into_owned(),
                before_hash: write.before.as_deref().map(content_hash),
                after_hash: content_hash(&write.after),
            })
            .collect(),
        completed: Vec::new(),
        conflict: None,
        error: None,
    };
    if let Err(source) = persist(workspace, &dir, &journal) {
        let _ = root.remove_dir_all(&dir);
        return Err(TransactionError::Journal { source });
    }

    for write in writes {
        let result = apply_atomically(
            workspace,
            &write.path,
            &write.after,
            write.permissions.clone(),
        );
        let key = write.path.to_string_lossy().into_owned();
        let error = match result {
            Ok(()) => {
                journal.completed.push(key.clone());
                match persist(workspace, &dir, &journal) {
                    Ok(()) => continue,
                    // The rename landed but its record did not: unwind now,
                    // while this process still knows what it did.
                    Err(source) => format!("could not journal the write: {source}"),
                }
            }
            Err(error) => error,
        };
        let outcome = unwind(workspace, &dir, &mut journal, format!("{key}: {error}"));
        return Err(TransactionError::Apply(Box::new(ApplyFailure {
            id,
            path: key,
            error,
            outcome,
        })));
    }
    journal.status = TransactionStatus::Complete;
    if let Err(source) = persist(workspace, &dir, &journal) {
        let error = format!("could not journal completion: {source}");
        let outcome = unwind(workspace, &dir, &mut journal, error.clone());
        return Err(TransactionError::Apply(Box::new(ApplyFailure {
            id,
            path: journal
                .planned
                .last()
                .map(|write| write.path.clone())
                .unwrap_or_default(),
            error,
            outcome,
        })));
    }
    prune(workspace);
    Ok(TransactionReceipt { id })
}

/// Rolls the completed writes back in reverse order and records the result
/// in the journal. Returns how it ended; the journal on disk agrees.
fn unwind(
    workspace: &Workspace,
    dir: &Path,
    journal: &mut Journal,
    error: String,
) -> RollbackOutcome {
    journal.status = TransactionStatus::RollingBack;
    journal.error = Some(error);
    let _ = persist(workspace, dir, journal);
    let outcome = restore(workspace, dir, journal, Direction::Backward);
    let _ = persist(workspace, dir, journal);
    outcome
}

#[derive(Clone, Copy)]
enum Direction {
    /// Restore `before` bytes over `after` bytes, newest write first.
    Backward,
    /// Restore `after` bytes over `before` bytes, oldest write first.
    Forward,
}

/// Restores every completed write of `journal` in `direction`, updating its
/// `completed` list and status as it goes. Fail-closed: the first file whose
/// current hash is not the expected one stops the pass with a conflict.
fn restore(
    workspace: &Workspace,
    dir: &Path,
    journal: &mut Journal,
    direction: Direction,
) -> RollbackOutcome {
    let root = workspace.root();
    let blobs = dir.join(BLOBS_DIR);
    let order: Vec<JournalWrite> = match direction {
        Direction::Backward => journal
            .completed
            .iter()
            .rev()
            .filter_map(|path| journal.planned.iter().find(|write| &write.path == path))
            .cloned()
            .collect(),
        Direction::Forward => journal.planned.clone(),
    };
    for write in order {
        let (expected, target): (Option<&str>, Option<&str>) = match direction {
            Direction::Backward => (Some(&write.after_hash), write.before_hash.as_deref()),
            Direction::Forward => (write.before_hash.as_deref(), Some(&write.after_hash)),
        };
        let path = Path::new(&write.path);
        let current = match root.symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() => match root.read(path) {
                Ok(bytes) => Some((content_hash(&bytes), metadata.permissions())),
                Err(error) => return fail(journal, &write.path, error),
            },
            Ok(_) => {
                return conflict(
                    journal,
                    &write.path,
                    expected,
                    Some("not_a_file".to_owned()),
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return fail(journal, &write.path, error),
        };
        let actual = current.as_ref().map(|(hash, _)| hash.as_str());
        if actual != expected {
            return conflict(journal, &write.path, expected, actual.map(str::to_owned));
        }
        let restored = match target {
            Some(hash) => root.read(blobs.join(hash)).and_then(|bytes| {
                apply_atomically(
                    workspace,
                    path,
                    &bytes,
                    current.map(|(_, permissions)| permissions),
                )
                .map_err(std::io::Error::other)
            }),
            None => root.remove_file(path),
        };
        if let Err(error) = restored {
            return fail(journal, &write.path, error);
        }
        match direction {
            Direction::Backward => {
                journal.completed.pop();
            }
            Direction::Forward => journal.completed.push(write.path.clone()),
        }
    }
    journal.status = match direction {
        Direction::Backward => TransactionStatus::RolledBack,
        Direction::Forward => TransactionStatus::Complete,
    };
    journal.conflict = None;
    RollbackOutcome::RolledBack
}

fn conflict(
    journal: &mut Journal,
    path: &str,
    expected: Option<&str>,
    actual: Option<String>,
) -> RollbackOutcome {
    let conflict = Conflict {
        path: path.to_owned(),
        expected: expected.map(str::to_owned),
        actual,
    };
    journal.status = TransactionStatus::Failed;
    journal.conflict = Some(conflict.clone());
    RollbackOutcome::Conflict(conflict)
}

fn fail(journal: &mut Journal, path: &str, error: std::io::Error) -> RollbackOutcome {
    journal.status = TransactionStatus::Failed;
    journal.error = Some(format!("{path}: {error}"));
    RollbackOutcome::Failed {
        path: path.to_owned(),
        error: error.to_string(),
    }
}

/// Why a rollback or re-apply requested by id did not restore the files.
#[derive(Debug, Error)]
pub enum TransactionRestoreError {
    #[error("workspace could not be opened: {source}")]
    Workspace {
        #[source]
        source: std::io::Error,
    },
    #[error("transaction {id} has no readable journal: {source}")]
    Journal {
        id: String,
        #[source]
        source: std::io::Error,
    },
    #[error("transaction {id} is {status:?}, which cannot be {operation}")]
    Status {
        id: String,
        status: TransactionStatus,
        operation: &'static str,
    },
    #[error(
        "transaction {id}: {} is {} now, not {}; left as is",
        conflict.path,
        conflict.actual.as_deref().unwrap_or("absent"),
        conflict.expected.as_deref().unwrap_or("absent")
    )]
    Conflict { id: String, conflict: Conflict },
    #[error("transaction {id} could not restore {path}: {error}")]
    Io {
        id: String,
        path: String,
        error: String,
    },
}

/// Restores every file a `complete` transaction wrote to its before-bytes,
/// newest write first. Idempotent for a transaction already `rolled_back`.
/// Blocking; call from a blocking context.
pub fn rollback_transaction(workspace: &Path, id: &str) -> Result<(), TransactionRestoreError> {
    let workspace = open(workspace)?;
    let _guard = workspace
        .apply_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = transaction_dir(id);
    let mut journal =
        read_journal(&workspace, &dir).map_err(|source| TransactionRestoreError::Journal {
            id: id.to_owned(),
            source,
        })?;
    match journal.status {
        TransactionStatus::RolledBack => return Ok(()),
        TransactionStatus::Complete
        | TransactionStatus::Applying
        | TransactionStatus::RollingBack => {}
        status @ TransactionStatus::Failed => {
            return Err(TransactionRestoreError::Status {
                id: id.to_owned(),
                status,
                operation: "rolled back",
            });
        }
    }
    journal.status = TransactionStatus::RollingBack;
    let _ = persist(&workspace, &dir, &journal);
    let outcome = restore(&workspace, &dir, &mut journal, Direction::Backward);
    finish(&workspace, &dir, &journal, id, outcome)
}

/// Restores every file a `rolled_back` transaction wrote to its after-bytes,
/// oldest write first. Idempotent for a transaction already `complete`.
/// Blocking; call from a blocking context.
pub fn reapply_transaction(workspace: &Path, id: &str) -> Result<(), TransactionRestoreError> {
    let workspace = open(workspace)?;
    let _guard = workspace
        .apply_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = transaction_dir(id);
    let mut journal =
        read_journal(&workspace, &dir).map_err(|source| TransactionRestoreError::Journal {
            id: id.to_owned(),
            source,
        })?;
    match journal.status {
        TransactionStatus::Complete => return Ok(()),
        TransactionStatus::RolledBack => {}
        status @ (TransactionStatus::Applying
        | TransactionStatus::RollingBack
        | TransactionStatus::Failed) => {
            return Err(TransactionRestoreError::Status {
                id: id.to_owned(),
                status,
                operation: "re-applied",
            });
        }
    }
    journal.status = TransactionStatus::Applying;
    journal.completed.clear();
    let _ = persist(&workspace, &dir, &journal);
    let outcome = restore(&workspace, &dir, &mut journal, Direction::Forward);
    finish(&workspace, &dir, &journal, id, outcome)
}

fn finish(
    workspace: &Workspace,
    dir: &Path,
    journal: &Journal,
    id: &str,
    outcome: RollbackOutcome,
) -> Result<(), TransactionRestoreError> {
    persist(workspace, dir, journal).map_err(|source| TransactionRestoreError::Journal {
        id: id.to_owned(),
        source,
    })?;
    match outcome {
        RollbackOutcome::RolledBack => Ok(()),
        RollbackOutcome::Conflict(conflict) => Err(TransactionRestoreError::Conflict {
            id: id.to_owned(),
            conflict,
        }),
        RollbackOutcome::Failed { path, error } => Err(TransactionRestoreError::Io {
            id: id.to_owned(),
            path,
            error,
        }),
    }
}

/// The journals under `.qq/transactions/`, oldest first. Unreadable
/// journals are skipped. Blocking; call from a blocking context.
pub fn list_transactions(workspace: &Path) -> Result<Vec<Journal>, TransactionRestoreError> {
    let workspace = open(workspace)?;
    Ok(transaction_ids(&workspace)
        .into_iter()
        .filter_map(|id| read_journal(&workspace, &transaction_dir(&id)).ok())
        .collect())
}

fn open(path: &Path) -> Result<Workspace, TransactionRestoreError> {
    std::fs::canonicalize(path)
        .and_then(|canonical| Workspace::open(&canonical))
        .map_err(|source| TransactionRestoreError::Workspace { source })
}

/// What recovery did with one incomplete transaction it found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecoveredTransaction {
    pub(crate) id: String,
    pub(crate) outcome: RecoveryOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecoveryOutcome {
    /// Never journaled past preparing; the directory was removed.
    Discarded,
    RolledBack,
    Conflict(Conflict),
    Failed {
        path: String,
        error: String,
    },
}

/// Rolls back every transaction left `applying` or `rolling_back` and
/// removes directories that never reached a journal. Runs under the apply
/// lock before the workspace serves any tool. Blocking.
pub(crate) fn recover(workspace: &Workspace) -> Vec<RecoveredTransaction> {
    let _guard = workspace
        .apply_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut recovered = Vec::new();
    for id in transaction_ids(workspace) {
        let dir = transaction_dir(&id);
        let mut journal = match read_journal(workspace, &dir) {
            Ok(journal) => journal,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let _ = workspace.root().remove_dir_all(&dir);
                recovered.push(RecoveredTransaction {
                    id,
                    outcome: RecoveryOutcome::Discarded,
                });
                continue;
            }
            // A corrupt journal is evidence; leave it for a human.
            Err(_) => continue,
        };
        match journal.status {
            TransactionStatus::Applying | TransactionStatus::RollingBack => {}
            TransactionStatus::Complete
            | TransactionStatus::RolledBack
            | TransactionStatus::Failed => continue,
        }
        // A crash between a rename and its journal entry leaves one planned
        // write on disk unrecorded; its after-hash on disk identifies it.
        for write in &journal.planned {
            if journal.completed.contains(&write.path) {
                continue;
            }
            let landed = workspace
                .root()
                .read(&write.path)
                .is_ok_and(|bytes| content_hash(&bytes) == write.after_hash);
            if landed {
                journal.completed.push(write.path.clone());
            }
        }
        journal.status = TransactionStatus::RollingBack;
        let _ = persist(workspace, &dir, &journal);
        let outcome = match restore(workspace, &dir, &mut journal, Direction::Backward) {
            RollbackOutcome::RolledBack => RecoveryOutcome::RolledBack,
            RollbackOutcome::Conflict(conflict) => RecoveryOutcome::Conflict(conflict),
            RollbackOutcome::Failed { path, error } => RecoveryOutcome::Failed { path, error },
        };
        let _ = persist(workspace, &dir, &journal);
        recovered.push(RecoveredTransaction { id, outcome });
    }
    recovered
}

/// Whether `path` (contained, workspace-relative) lies under the
/// transaction directory, which no tool may write.
pub(crate) fn is_reserved(path: &Path) -> bool {
    path.starts_with(TRANSACTIONS_DIR)
}

fn transaction_dir(id: &str) -> PathBuf {
    Path::new(TRANSACTIONS_DIR).join(id)
}

/// Transaction ids present on disk, oldest first (ids sort by start time).
fn transaction_ids(workspace: &Workspace) -> Vec<String> {
    let Ok(entries) = workspace.root().read_dir(TRANSACTIONS_DIR) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| is_transaction_id(name))
        .collect();
    ids.sort_unstable();
    ids
}

/// `<13 decimal digits of unix ms>-<16 hex digits>`.
fn is_transaction_id(name: &str) -> bool {
    let Some((stamp, suffix)) = name.split_once('-') else {
        return false;
    };
    stamp.len() == 13
        && stamp.bytes().all(|byte| byte.is_ascii_digit())
        && suffix.len() == 16
        && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn new_id(workspace: &Path) -> String {
    use sha2::{Digest as _, Sha256};
    let ordinal = ORDINAL.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(nanos.to_le_bytes());
    hasher.update(ordinal.to_le_bytes());
    hasher.update(workspace.as_os_str().as_encoded_bytes());
    let digest = hasher.finalize();
    let mut suffix = String::with_capacity(16);
    for byte in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(suffix, "{byte:02x}");
    }
    format!("{:013}-{suffix}", unix_ms())
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// Creates `.qq/transactions/` with a self-ignoring `.gitignore` on first use.
fn ensure_transactions_dir(workspace: &Workspace) -> Result<(), std::io::Error> {
    let root = workspace.root();
    if root.is_dir(TRANSACTIONS_DIR) {
        return Ok(());
    }
    root.create_dir_all(TRANSACTIONS_DIR)?;
    let ignore = Path::new(TRANSACTIONS_DIR).join(".gitignore");
    match root.write(&ignore, b"*\n") {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

fn read_journal(workspace: &Workspace, dir: &Path) -> Result<Journal, std::io::Error> {
    let bytes = workspace.root().read(dir.join(JOURNAL_FILE))?;
    let journal: Journal = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
    if journal.schema_version != SCHEMA_VERSION {
        return Err(std::io::Error::other(format!(
            "unsupported journal schema {}",
            journal.schema_version
        )));
    }
    Ok(journal)
}

fn persist(workspace: &Workspace, dir: &Path, journal: &Journal) -> Result<(), std::io::Error> {
    let bytes = serde_json::to_vec_pretty(journal).map_err(std::io::Error::other)?;
    write_synced(workspace, dir, JOURNAL_FILE, &bytes)
}

fn write_blob(workspace: &Workspace, blobs: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    let hash = content_hash(bytes);
    if workspace.root().exists(blobs.join(&hash)) {
        return Ok(());
    }
    write_synced(workspace, blobs, &hash, bytes)
}

/// Writes `dir/name` through a synced temporary file and rename, then syncs
/// the directory so the entry itself is durable.
fn write_synced(
    workspace: &Workspace,
    dir: &Path,
    name: &str,
    bytes: &[u8],
) -> Result<(), std::io::Error> {
    let root = workspace.root();
    let temp = dir.join(format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        ORDINAL.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = cap_std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = root.open_with(&temp, &options)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let renamed = written.and_then(|()| root.rename(&temp, root, dir.join(name)));
    if renamed.is_err() {
        let _ = root.remove_file(&temp);
        return renamed;
    }
    sync_dir(workspace, dir)
}

#[cfg(unix)]
fn sync_dir(workspace: &Workspace, dir: &Path) -> Result<(), std::io::Error> {
    // `open_dir` yields an `O_PATH` handle on Linux, which `fsync` rejects;
    // a read-only open of the directory is what durability needs.
    workspace
        .root()
        .open(dir)
        .and_then(|handle| handle.sync_all())
}

#[cfg(not(unix))]
fn sync_dir(_workspace: &Workspace, _dir: &Path) -> Result<(), std::io::Error> {
    Ok(())
}

/// Removes the oldest finished transaction directories until at most
/// [`MAX_RETAINED_TRANSACTIONS`] remain. A `failed` one counts toward the
/// bound but is never removed: it is evidence the user may still need.
fn prune(workspace: &Workspace) {
    let ids = transaction_ids(workspace);
    let excess = ids.len().saturating_sub(MAX_RETAINED_TRANSACTIONS);
    if excess == 0 {
        return;
    }
    let mut removed = 0;
    for id in ids {
        if removed == excess {
            break;
        }
        let dir = transaction_dir(&id);
        let prunable = read_journal(workspace, &dir).is_ok_and(|journal| {
            matches!(
                journal.status,
                TransactionStatus::Complete | TransactionStatus::RolledBack
            )
        });
        if prunable && workspace.root().remove_dir_all(&dir).is_ok() {
            removed += 1;
        }
    }
}

/// Writes `bytes` to a temporary file in the target's directory through the
/// workspace capability, preserves permissions when replacing an existing
/// file, and renames into place so readers never observe a partial write.
pub(crate) fn apply_atomically(
    workspace: &Workspace,
    path: &Path,
    bytes: &[u8],
    permissions: Option<cap_std::fs::Permissions>,
) -> Result<(), String> {
    let temp_name = format!(
        ".qq-apply-{}-{}.tmp",
        std::process::id(),
        ORDINAL.fetch_add(1, Ordering::Relaxed),
    );
    let temp_path = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(&temp_name),
        _ => PathBuf::from(&temp_name),
    };
    let mut options = cap_std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut temp = workspace
        .root()
        .open_with(&temp_path, &options)
        .map_err(|error| format!("could not create a temporary file: {error}"))?;
    let written = temp
        .write_all(bytes)
        .and_then(|()| temp.sync_all())
        .map_err(|error| format!("could not write the temporary file: {error}"));
    drop(temp);
    #[cfg(test)]
    {
        let hook = {
            let mut hooks = APPLY_HOOKS.lock().unwrap();
            hooks
                .iter()
                .position(|hook| hook.workspace == workspace.path())
                .map(|index| hooks.remove(index))
        };
        if let Some(hook) = hook {
            let _ = hook.entered.send(());
            let _ = hook.release.recv();
        }
    }
    let applied = written
        .and_then(|()| match permissions {
            Some(permissions) => workspace
                .root()
                .set_permissions(&temp_path, permissions)
                .map_err(|error| format!("could not preserve file permissions: {error}")),
            None => Ok(()),
        })
        .and_then(|()| {
            workspace
                .root()
                .rename(&temp_path, workspace.root(), path)
                .map_err(|error| format!("could not apply the change: {error}"))
        });
    if applied.is_err() {
        let _ = workspace.root().remove_file(&temp_path);
    }
    applied
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn staged(path: &str, before: Option<&str>, after: &str) -> StagedWrite {
        StagedWrite {
            path: PathBuf::from(path),
            before: before.map(|text| text.as_bytes().to_vec()),
            after: after.as_bytes().to_vec(),
            permissions: None,
        }
    }

    fn journal(root: &Path, id: &str) -> Journal {
        let bytes = fs::read(root.join(TRANSACTIONS_DIR).join(id).join(JOURNAL_FILE)).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn write_journal(root: &Path, journal: &Journal) {
        let dir = root.join(TRANSACTIONS_DIR).join(&journal.id);
        fs::create_dir_all(dir.join(BLOBS_DIR)).unwrap();
        fs::write(
            dir.join(JOURNAL_FILE),
            serde_json::to_vec_pretty(journal).unwrap(),
        )
        .unwrap();
    }

    fn write_blob(root: &Path, id: &str, bytes: &[u8]) -> String {
        let hash = content_hash(bytes);
        fs::write(
            root.join(TRANSACTIONS_DIR)
                .join(id)
                .join(BLOBS_DIR)
                .join(&hash),
            bytes,
        )
        .unwrap();
        hash
    }

    #[test]
    fn a_committed_transaction_journals_every_write_and_keeps_both_blobs() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("a.txt"), "alpha\n").unwrap();
        let workspace = Workspace::open(root).unwrap();
        let writes = [
            staged("a.txt", Some("alpha\n"), "ALPHA\n"),
            staged("new.txt", None, "fresh\n"),
        ];
        let receipt = run(&workspace, "edit_file", &writes).unwrap();
        assert_eq!(receipt.short().len(), 8);
        assert!(receipt.id.ends_with(receipt.short()) || receipt.id.contains(receipt.short()));
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "ALPHA\n");
        assert_eq!(fs::read_to_string(root.join("new.txt")).unwrap(), "fresh\n");

        let journal = journal(root, &receipt.id);
        assert_eq!(journal.schema_version, SCHEMA_VERSION);
        assert_eq!(journal.status, TransactionStatus::Complete);
        assert_eq!(journal.tool, "edit_file");
        assert_eq!(journal.completed, ["a.txt", "new.txt"]);
        assert_eq!(
            journal.planned,
            [
                JournalWrite {
                    path: "a.txt".to_owned(),
                    before_hash: Some(content_hash(b"alpha\n")),
                    after_hash: content_hash(b"ALPHA\n"),
                },
                JournalWrite {
                    path: "new.txt".to_owned(),
                    before_hash: None,
                    after_hash: content_hash(b"fresh\n"),
                },
            ]
        );
        let blobs = root
            .join(TRANSACTIONS_DIR)
            .join(&receipt.id)
            .join(BLOBS_DIR);
        for bytes in [&b"alpha\n"[..], b"ALPHA\n", b"fresh\n"] {
            assert_eq!(fs::read(blobs.join(content_hash(bytes))).unwrap(), bytes);
        }
        // No temp files linger anywhere in the transaction directory.
        let mut pending = vec![root.join(TRANSACTIONS_DIR)];
        while let Some(dir) = pending.pop() {
            for entry in fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name().to_string_lossy().into_owned();
                assert!(!name.ends_with(".tmp"), "{name}");
                if entry.file_type().unwrap().is_dir() {
                    pending.push(entry.path());
                }
            }
        }
        assert_eq!(
            fs::read_to_string(root.join(TRANSACTIONS_DIR).join(".gitignore")).unwrap(),
            "*\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_failure_midway_restores_the_completed_files_in_reverse_and_records_it() {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("a.txt"), "alpha\n").unwrap();
        fs::create_dir(root.join("locked")).unwrap();
        fs::write(root.join("locked/b.txt"), "beta\n").unwrap();
        let workspace = Workspace::open(root).unwrap();
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o555)).unwrap();
        let writes = [
            staged("a.txt", Some("alpha\n"), "ALPHA\n"),
            staged("created.txt", None, "new\n"),
            staged("locked/b.txt", Some("beta\n"), "BETA\n"),
        ];
        let error = run(&workspace, "edit_file", &writes).unwrap_err();
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        let TransactionError::Apply(failure) = error else {
            panic!("expected an apply failure, got {error}");
        };
        let ApplyFailure {
            id, path, outcome, ..
        } = *failure;
        assert_eq!(path, "locked/b.txt");
        assert_eq!(outcome, RollbackOutcome::RolledBack);
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "alpha\n");
        assert!(!root.join("created.txt").exists());
        assert_eq!(
            fs::read_to_string(root.join("locked/b.txt")).unwrap(),
            "beta\n"
        );
        let rolled_back = journal(root, &id);
        assert_eq!(rolled_back.id, id);
        assert_eq!(rolled_back.status, TransactionStatus::RolledBack);
        assert!(rolled_back.completed.is_empty());
        assert!(
            rolled_back
                .error
                .as_deref()
                .unwrap()
                .contains("locked/b.txt")
        );
        // The rolled-back id can be re-applied once the obstacle is gone.
        reapply_transaction(root, &id).unwrap();
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "ALPHA\n");
        assert_eq!(
            fs::read_to_string(root.join("created.txt")).unwrap(),
            "new\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("locked/b.txt")).unwrap(),
            "BETA\n"
        );
        let reapplied = journal(root, &id);
        assert_eq!(reapplied.status, TransactionStatus::Complete);
        assert_eq!(
            reapplied.completed,
            ["a.txt", "created.txt", "locked/b.txt"]
        );
    }

    #[test]
    fn rollback_and_reapply_are_idempotent_and_fail_closed_on_a_changed_file() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("a.txt"), "alpha\n").unwrap();
        fs::write(root.join("b.txt"), "beta\n").unwrap();
        let workspace = Workspace::open(root).unwrap();
        let writes = [
            staged("a.txt", Some("alpha\n"), "ALPHA\n"),
            staged("b.txt", Some("beta\n"), "BETA\n"),
        ];
        let id = run(&workspace, "edit_file", &writes).unwrap().id;
        // Re-applying a complete transaction is a no-op.
        reapply_transaction(root, &id).unwrap();

        // b.txt drifted after the commit: rollback stops at it (newest first)
        // and leaves a.txt, which it has not reached, untouched too.
        fs::write(root.join("b.txt"), "drifted\n").unwrap();
        let error = rollback_transaction(root, &id).unwrap_err();
        match &error {
            TransactionRestoreError::Conflict { conflict, .. } => {
                assert_eq!(conflict.path, "b.txt");
                assert_eq!(conflict.expected, Some(content_hash(b"BETA\n")));
                assert_eq!(conflict.actual, Some(content_hash(b"drifted\n")));
            }
            other => panic!("expected a conflict, got {other}"),
        }
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "ALPHA\n");
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "drifted\n");
        let failed = journal(root, &id);
        assert_eq!(failed.status, TransactionStatus::Failed);
        assert_eq!(failed.conflict.as_ref().unwrap().path, "b.txt");
        // A failed transaction is terminal for both directions.
        assert!(matches!(
            rollback_transaction(root, &id),
            Err(TransactionRestoreError::Status { .. })
        ));
        assert!(matches!(
            reapply_transaction(root, &id),
            Err(TransactionRestoreError::Status { .. })
        ));

        // A clean commit rolls back fully, twice, then re-applies only while
        // the before-bytes are still in place.
        fs::write(root.join("b.txt"), "beta\n").unwrap();
        let workspace = Workspace::open(root).unwrap();
        let id = run(&workspace, "edit_file", &writes[1..]).unwrap().id;
        rollback_transaction(root, &id).unwrap();
        rollback_transaction(root, &id).unwrap();
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "beta\n");
        fs::write(root.join("b.txt"), "someone else\n").unwrap();
        assert!(matches!(
            reapply_transaction(root, &id),
            Err(TransactionRestoreError::Conflict { .. })
        ));
        assert_eq!(
            fs::read_to_string(root.join("b.txt")).unwrap(),
            "someone else\n"
        );
        assert!(matches!(
            rollback_transaction(root, "0000000000000-00000000000000ff"),
            Err(TransactionRestoreError::Journal { .. })
        ));
    }

    #[test]
    fn recovery_rolls_back_torn_transactions_and_leaves_finished_ones_alone() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("a.txt"), "ALPHA\n").unwrap();
        fs::write(root.join("b.txt"), "BETA\n").unwrap();
        fs::write(root.join("c.txt"), "gamma\n").unwrap();
        fs::create_dir_all(root.join(TRANSACTIONS_DIR)).unwrap();

        // Torn after two of three renames; the second rename landed but the
        // process died before journaling it.
        let torn = "0000000000001-0000000000000001";
        let mut journal_torn = Journal {
            schema_version: SCHEMA_VERSION,
            id: torn.to_owned(),
            tool: "edit_file".to_owned(),
            started_at_unix_ms: 1,
            status: TransactionStatus::Applying,
            planned: vec![
                JournalWrite {
                    path: "a.txt".to_owned(),
                    before_hash: Some(content_hash(b"alpha\n")),
                    after_hash: content_hash(b"ALPHA\n"),
                },
                JournalWrite {
                    path: "b.txt".to_owned(),
                    before_hash: Some(content_hash(b"beta\n")),
                    after_hash: content_hash(b"BETA\n"),
                },
                JournalWrite {
                    path: "c.txt".to_owned(),
                    before_hash: Some(content_hash(b"gamma\n")),
                    after_hash: content_hash(b"GAMMA\n"),
                },
            ],
            completed: vec!["a.txt".to_owned()],
            conflict: None,
            error: None,
        };
        write_journal(root, &journal_torn);
        for bytes in [
            &b"alpha\n"[..],
            b"ALPHA\n",
            b"beta\n",
            b"BETA\n",
            b"gamma\n",
            b"GAMMA\n",
        ] {
            write_blob(root, torn, bytes);
        }
        // Complete, and a directory that never reached a journal.
        journal_torn.id = "0000000000002-0000000000000002".to_owned();
        journal_torn.status = TransactionStatus::Complete;
        journal_torn.completed = vec!["a.txt".to_owned(), "b.txt".to_owned(), "c.txt".to_owned()];
        write_journal(root, &journal_torn);
        fs::create_dir_all(
            root.join(TRANSACTIONS_DIR)
                .join("0000000000003-0000000000000003")
                .join(BLOBS_DIR),
        )
        .unwrap();
        // Corrupt journals are left for a human.
        let corrupt = root
            .join(TRANSACTIONS_DIR)
            .join("0000000000004-0000000000000004");
        fs::create_dir_all(&corrupt).unwrap();
        fs::write(corrupt.join(JOURNAL_FILE), b"{not json").unwrap();

        let workspace = Workspace::open(root).unwrap();
        let recovered = recover(&workspace);
        assert_eq!(
            recovered,
            [
                RecoveredTransaction {
                    id: torn.to_owned(),
                    outcome: RecoveryOutcome::RolledBack,
                },
                RecoveredTransaction {
                    id: "0000000000003-0000000000000003".to_owned(),
                    outcome: RecoveryOutcome::Discarded,
                },
            ]
        );
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "alpha\n");
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "beta\n");
        assert_eq!(fs::read_to_string(root.join("c.txt")).unwrap(), "gamma\n");
        assert_eq!(journal(root, torn).status, TransactionStatus::RolledBack);
        assert_eq!(
            journal(root, "0000000000002-0000000000000002").status,
            TransactionStatus::Complete
        );
        assert!(
            !root
                .join(TRANSACTIONS_DIR)
                .join("0000000000003-0000000000000003")
                .exists()
        );
        assert_eq!(fs::read(corrupt.join(JOURNAL_FILE)).unwrap(), b"{not json");
        // A second pass finds nothing to do.
        assert!(recover(&workspace).is_empty());
        let listed = list_transactions(root).unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|journal| journal.id.as_str())
                .collect::<Vec<_>>(),
            [torn, "0000000000002-0000000000000002"]
        );
    }

    #[test]
    fn recovery_of_a_torn_transaction_over_a_drifted_file_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("a.txt"), "edited by hand\n").unwrap();
        fs::create_dir_all(root.join(TRANSACTIONS_DIR)).unwrap();
        let id = "0000000000001-0000000000000001";
        write_journal(
            root,
            &Journal {
                schema_version: SCHEMA_VERSION,
                id: id.to_owned(),
                tool: "write_file".to_owned(),
                started_at_unix_ms: 1,
                status: TransactionStatus::Applying,
                planned: vec![JournalWrite {
                    path: "a.txt".to_owned(),
                    before_hash: Some(content_hash(b"alpha\n")),
                    after_hash: content_hash(b"ALPHA\n"),
                }],
                completed: vec!["a.txt".to_owned()],
                conflict: None,
                error: None,
            },
        );
        write_blob(root, id, b"alpha\n");
        write_blob(root, id, b"ALPHA\n");
        let workspace = Workspace::open(root).unwrap();
        let recovered = recover(&workspace);
        assert_eq!(recovered.len(), 1);
        assert!(matches!(
            &recovered[0].outcome,
            RecoveryOutcome::Conflict(conflict) if conflict.path == "a.txt"
        ));
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).unwrap(),
            "edited by hand\n"
        );
        assert_eq!(journal(root, id).status, TransactionStatus::Failed);
    }

    #[test]
    fn retention_prunes_the_oldest_finished_transactions_but_never_failed_ones() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("a.txt"), "0\n").unwrap();
        let workspace = Workspace::open(root).unwrap();
        // A failed transaction older than everything else must survive.
        fs::create_dir_all(root.join(TRANSACTIONS_DIR)).unwrap();
        write_journal(
            root,
            &Journal {
                schema_version: SCHEMA_VERSION,
                id: "0000000000000-0000000000000000".to_owned(),
                tool: "edit_file".to_owned(),
                started_at_unix_ms: 0,
                status: TransactionStatus::Failed,
                planned: Vec::new(),
                completed: Vec::new(),
                conflict: None,
                error: Some("kept".to_owned()),
            },
        );
        let mut ids = Vec::new();
        for step in 0..MAX_RETAINED_TRANSACTIONS + 5 {
            let before = format!("{step}\n");
            let after = format!("{}\n", step + 1);
            let writes = [staged("a.txt", Some(&before), &after)];
            ids.push(run(&workspace, "edit_file", &writes).unwrap().id);
        }
        // The failed one counts against the bound but is never the one removed.
        let listed = list_transactions(root).unwrap();
        assert_eq!(listed.len(), MAX_RETAINED_TRANSACTIONS);
        assert_eq!(listed[0].id, "0000000000000-0000000000000000");
        let kept: Vec<&str> = listed[1..]
            .iter()
            .map(|journal| journal.id.as_str())
            .collect();
        let expected: Vec<&str> = ids[ids.len() - (MAX_RETAINED_TRANSACTIONS - 1)..]
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(kept, expected);
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).unwrap(),
            format!("{}\n", MAX_RETAINED_TRANSACTIONS + 5)
        );
    }

    #[test]
    fn ids_are_unique_and_sort_by_start_time() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("a.txt"), "0\n").unwrap();
        let workspace = Workspace::open(root).unwrap();
        let mut ids = Vec::new();
        for step in 0..8 {
            let before = format!("{step}\n");
            let after = format!("{}\n", step + 1);
            ids.push(
                run(
                    &workspace,
                    "edit_file",
                    &[staged("a.txt", Some(&before), &after)],
                )
                .unwrap()
                .id,
            );
        }
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted, ids);
        assert!(is_reserved(Path::new(".qq/transactions")));
        assert!(is_reserved(Path::new(".qq/transactions/x/journal.json")));
        assert!(!is_reserved(Path::new(".qq/other")));
        assert!(!is_reserved(Path::new("src/.qq/transactions")));
    }
}
