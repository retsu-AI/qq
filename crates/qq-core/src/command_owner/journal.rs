use qq_protocol::{StoreId, ToolCallId};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    fs::{File, OpenOptions, TryLockError},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[cfg(test)]
use super::{ADMISSION_HOOKS, CANCELLATION_HOOKS, wait_at_commit};
use super::{
    CommandOwnerError, CommandSnapshot, CommandStatus, ExecutionContextReceipt,
    MAX_LIVE_OUTPUT_BYTES, MAX_RETAINED, ToolOutput,
};

// Durable control must retain capacity when project filesystem reads stall.
// Bound journal I/O independently across owners, in addition to each journal's
// serial transaction permit; neither pool may starve the other.
async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, CommandOwnerError> + Send + 'static,
) -> Result<T, CommandOwnerError> {
    static CAPACITY: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    let permit = CAPACITY
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(8)))
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| CommandOwnerError::Unavailable)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    })
    .await
    .map_err(|_| CommandOwnerError::Unavailable)?
}

#[derive(Clone)]
pub(super) struct Journal(Arc<JournalState>);
struct JournalState {
    store_id: StoreId,
    serial: Arc<tokio::sync::Semaphore>,
    connection: Mutex<Option<Connection>>,
    lock: Mutex<Option<File>>,
    #[cfg(windows)]
    directory: Mutex<Option<File>>,
    #[cfg(test)]
    cleanup: Mutex<Option<tokio::sync::oneshot::Sender<std::thread::ThreadId>>>,
}

impl Drop for JournalState {
    fn drop(&mut self) {
        let connection = self
            .connection
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let lock = self
            .lock
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        #[cfg(windows)]
        let directory = self
            .directory
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let empty = connection.is_none() && lock.is_none();
        #[cfg(windows)]
        let empty = empty && directory.is_none();
        if empty {
            return;
        }
        #[cfg(test)]
        let observed = self.cleanup.get_mut().unwrap().take();
        let close = move || {
            // SQLite may checkpoint the WAL while closing. Keep the exclusive
            // file lock until that I/O has finished, including implicit drops.
            drop(connection);
            drop(lock);
            #[cfg(windows)]
            drop(directory);
            #[cfg(test)]
            if let Some(observed) = observed {
                let _ = observed.send(std::thread::current().id());
            }
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            drop(runtime.spawn_blocking(close));
        } else {
            close();
        }
    }
}

struct StoredSnapshot {
    version: u64,
    status: String,
    receipt: Option<String>,
    result: Option<String>,
    output: Vec<u8>,
}

impl Journal {
    pub(super) async fn open(directory: PathBuf) -> Result<Self, CommandOwnerError> {
        blocking(move || Self::open_blocking(directory)).await
    }

    fn open_blocking(directory: PathBuf) -> Result<Self, CommandOwnerError> {
        #[cfg(unix)]
        let directory = trusted_parent(directory.parent().ok_or(CommandOwnerError::PrivateState)?)?
            .join(
                directory
                    .file_name()
                    .ok_or(CommandOwnerError::PrivateState)?,
            );
        // A private parent also protects the transport's sibling metadata.
        // Check it before creating anything, and keep it pinned through open.
        #[cfg(windows)]
        let _parent = super::windows_state::directory(
            directory.parent().ok_or(CommandOwnerError::PrivateState)?,
        )?;
        if !directory.exists() {
            #[cfg(unix)]
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(not(unix))]
            let builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                builder.mode(0o700);
            }
            builder.create(&directory)?;
        }
        let metadata = std::fs::symlink_metadata(&directory)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(CommandOwnerError::PrivateState);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if metadata.mode() & 0o077 != 0 || metadata.uid() != rustix::process::getuid().as_raw()
            {
                return Err(CommandOwnerError::PrivateState);
            }
        }
        #[cfg(windows)]
        let held_directory = super::windows_state::directory(&directory)?;
        let lock = private_file(&directory.join("owner.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(CommandOwnerError::Busy),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        let path = directory.join("commands.sqlite3");
        let _database_file = private_file(&path)?;
        let mut connection = Connection::open(path)?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if !matches!(version, 0 | 3 | 4) {
            return Err(CommandOwnerError::Schema(version));
        }
        if version == 0 {
            let existing: u64 = connection.query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'",
                [], |row| row.get(0),
            )?;
            if existing != 0 {
                return Err(CommandOwnerError::Schema(0));
            }
        }
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")?;
        if version == 0 {
            let store_id = StoreId::generate()?;
            let tx = connection.transaction()?;
            tx.execute_batch(
                "CREATE TABLE metadata (
                    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                    store_id TEXT NOT NULL CHECK(length(store_id)=32)
                );
                CREATE TABLE commands (
                    id TEXT PRIMARY KEY,
                    request TEXT CHECK(request IS NULL OR length(CAST(request AS BLOB))<=262144),
                    status TEXT NOT NULL CHECK(status IN ('queued','running','finished','lost')),
                    version INTEGER NOT NULL DEFAULT 0 CHECK(version>=0),
                    cancel_requested INTEGER NOT NULL DEFAULT 0 CHECK(cancel_requested IN (0,1)),
                    output BLOB NOT NULL DEFAULT X'' CHECK(typeof(output)='blob' AND length(output)<=131072),
                    receipt TEXT CHECK(receipt IS NULL OR length(CAST(receipt AS BLOB))<=16384),
                    result TEXT CHECK(result IS NULL OR length(CAST(result AS BLOB))<=1048576)
                );
                PRAGMA user_version=3;",
            )?;
            tx.execute(
                "INSERT INTO metadata (singleton, store_id) VALUES (1, ?1)",
                [store_id.to_string()],
            )?;
            tx.commit()?;
        }
        if version != 4 {
            let tx = connection.transaction()?;
            tx.execute_batch("CREATE TABLE retired_commands (
                id TEXT NOT NULL PRIMARY KEY CHECK(length(id)=32),
                request_sha256 BLOB CHECK(request_sha256 IS NULL OR (typeof(request_sha256)='blob' AND length(request_sha256)=32))
            ) STRICT, WITHOUT ROWID; PRAGMA user_version=4;")?;
            tx.commit()?;
        }
        let store_id: String = connection.query_row(
            "SELECT store_id FROM metadata WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        let store_id = store_id.parse()?;
        connection.execute(
            "UPDATE commands SET status='lost',version=version+1 WHERE status IN ('queued','running')",
            [],
        )?;
        Ok(Self(Arc::new(JournalState {
            store_id,
            serial: Arc::new(tokio::sync::Semaphore::new(1)),
            connection: Mutex::new(Some(connection)),
            lock: Mutex::new(Some(lock)),
            #[cfg(windows)]
            directory: Mutex::new(Some(held_directory)),
            #[cfg(test)]
            cleanup: Mutex::new(None),
        })))
    }

    pub(super) fn store_id(&self) -> StoreId {
        self.0.store_id
    }

    #[cfg(test)]
    pub(super) fn observe_cleanup(&self) -> tokio::sync::oneshot::Receiver<std::thread::ThreadId> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        assert!(self.0.cleanup.lock().unwrap().replace(sender).is_none());
        receiver
    }

    async fn call<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T, CommandOwnerError> + Send + 'static,
    ) -> Result<T, CommandOwnerError> {
        let serial = Arc::clone(&self.0.serial)
            .acquire_owned()
            .await
            .map_err(|_| CommandOwnerError::Unavailable)?;
        let state = Arc::clone(&self.0);
        blocking(move || {
            let _serial = serial;
            let mut connection = state
                .connection
                .lock()
                .map_err(|_| CommandOwnerError::Unavailable)?;
            operation(connection.as_mut().ok_or(CommandOwnerError::Unavailable)?)
        })
        .await
    }

    pub(super) async fn close(&self) -> Result<(), CommandOwnerError> {
        let serial = Arc::clone(&self.0.serial)
            .acquire_owned()
            .await
            .map_err(|_| CommandOwnerError::Unavailable)?;
        let state = Arc::clone(&self.0);
        blocking(move || {
            let _serial = serial;
            let mut connection_guard = state
                .connection
                .lock()
                .map_err(|_| CommandOwnerError::Unavailable)?;
            if let Some(connection) = connection_guard.take() {
                connection
                    .close()
                    .map_err(|(_, error)| CommandOwnerError::Sqlite(error))?;
            }
            if let Some(lock) = state
                .lock
                .lock()
                .map_err(|_| CommandOwnerError::Unavailable)?
                .take()
            {
                lock.unlock()?;
            }
            #[cfg(windows)]
            drop(
                state
                    .directory
                    .lock()
                    .map_err(|_| CommandOwnerError::Unavailable)?
                    .take(),
            );
            Ok(())
        })
        .await
    }

    pub(super) async fn existing(
        &self,
        id: ToolCallId,
        request: String,
    ) -> Result<bool, CommandOwnerError> {
        self.call(move |db| {
            let previous: Option<Option<String>> = db
                .query_row(
                    "SELECT request FROM commands WHERE id=?1",
                    [id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            match previous {
                Some(Some(previous)) if previous != request => Err(CommandOwnerError::Conflict),
                Some(Some(_)) => Ok(true),
                Some(None) => Ok(false),
                None => {
                    reject_retired(db, id, Some(&request))?;
                    Ok(false)
                }
            }
        })
        .await
    }

    pub(super) async fn admit(
        &self,
        id: ToolCallId,
        request: String,
    ) -> Result<bool, CommandOwnerError> {
        self.call(move |db| {
            let tx = db.transaction()?;
            let previous: Option<Option<String>> = tx
                .query_row(
                    "SELECT request FROM commands WHERE id=?1",
                    [id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            match previous {
                Some(Some(previous)) => return if previous == request { Ok(false) } else { Err(CommandOwnerError::Conflict) },
                Some(None) => {
                    // Cancellation can win before submission reaches the owner.
                    // Bind the first late invocation to that durable tombstone.
                    let bound = tx.execute("UPDATE commands SET request=?2 WHERE id=?1 AND status='finished' AND cancel_requested=1", params![id.to_string(), request])?;
                    if bound != 1 { return Err(CommandOwnerError::Invalid); }
                    tx.commit()?;
                    return Ok(false);
                }
                None => {}
            }
            reject_retired(&tx, id, Some(&request))?;
            let count: usize =
                tx.query_row("SELECT COUNT(*) FROM commands", [], |row| row.get(0))?;
            if count >= MAX_RETAINED {
                return Err(CommandOwnerError::Full);
            }
            tx.execute(
                "INSERT INTO commands(id,request,status) VALUES (?1,?2,'queued')",
                params![id.to_string(), request],
            )?;
            tx.commit()?;
            #[cfg(test)]
            wait_at_commit(&ADMISSION_HOOKS, id);
            Ok(true)
        })
        .await
    }

    pub(super) async fn started(
        &self,
        id: ToolCallId,
        receipt: Box<ExecutionContextReceipt>,
    ) -> Result<u64, CommandOwnerError> {
        self.call(move |db| {
            let tx = db.transaction()?;
            let request: String = tx.query_row(
                "SELECT request FROM commands WHERE id=?1 AND status='queued'",
                [id.to_string()],
                |row| row.get(0),
            )?;
            #[derive(serde::Deserialize)]
            struct ReceiptBinding {
                tool: String,
                arguments: String,
            }
            let request: ReceiptBinding = serde_json::from_str(&request)?;
            if !receipt.valid() || !receipt.bound_to(id, &request.tool, &request.arguments) {
                return Err(CommandOwnerError::Invalid);
            }
            tx.execute(
                "UPDATE commands SET status='running',receipt=?2,version=version+1 WHERE id=?1",
                params![id.to_string(), serde_json::to_string(&receipt)?],
            )?;
            let version = tx.query_row(
                "SELECT version FROM commands WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )?;
            tx.commit()?;
            Ok(version)
        })
        .await
    }

    pub(super) async fn output(
        &self,
        id: ToolCallId,
        chunk: Vec<u8>,
    ) -> Result<u64, CommandOwnerError> {
        #[cfg(test)]
        super::wait_before_output(id).await;
        self.call(move |db| {
            let tx = db.transaction()?;
            let (mut output, version): (Vec<u8>, u64) = tx.query_row(
                "SELECT output,version FROM commands WHERE id=?1 AND status='running'",
                [id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let remaining = MAX_LIVE_OUTPUT_BYTES.saturating_sub(output.len());
            let end = remaining.min(chunk.len());
            if end == 0 {
                return Ok(version);
            }
            output.extend_from_slice(&chunk[..end]);
            tx.execute(
                "UPDATE commands SET output=?2,version=version+1 WHERE id=?1",
                params![id.to_string(), output],
            )?;
            tx.commit()?;
            Ok(version + 1)
        })
        .await
    }

    pub(super) async fn finished(
        &self,
        id: ToolCallId,
        output: ToolOutput,
    ) -> Result<u64, CommandOwnerError> {
        self.call(move |db| {
            let tx = db.transaction()?;
            let process: Option<String> = tx.query_row(
                "SELECT receipt FROM commands WHERE id=?1 AND status IN ('queued','running')",
                [id.to_string()],
                |row| row.get(0),
            )?;
            if let Some(process) = process {
                let process: ExecutionContextReceipt = serde_json::from_str(&process)?;
                if output
                    .execution_context
                    .as_deref()
                    .is_some_and(|receipt| receipt != &process)
                {
                    return Err(CommandOwnerError::Conflict);
                }
            }
            tx.execute(
                "UPDATE commands SET status='finished',result=?2,version=version+1 WHERE id=?1",
                params![id.to_string(), serde_json::to_string(&output)?],
            )?;
            let version = tx.query_row(
                "SELECT version FROM commands WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )?;
            tx.commit()?;
            Ok(version)
        })
        .await
    }

    pub(super) async fn snapshot(
        &self,
        id: ToolCallId,
    ) -> Result<CommandSnapshot, CommandOwnerError> {
        self.call(move |db| {
            let row: Option<StoredSnapshot> = db
                .query_row(
                    "SELECT version,status,receipt,result,output FROM commands WHERE id=?1",
                    [id.to_string()],
                    |row| {
                        Ok(StoredSnapshot {
                            version: row.get(0)?,
                            status: row.get(1)?,
                            receipt: row.get(2)?,
                            result: row.get(3)?,
                            output: row.get(4)?,
                        })
                    },
                )
                .optional()?;
            if row.is_none() {
                reject_retired(db, id, None)?;
            }
            let StoredSnapshot {
                version,
                status,
                receipt,
                result,
                output,
            } = row.ok_or(CommandOwnerError::NotFound)?;
            let status = match status.as_str() {
                "queued" => CommandStatus::Queued,
                "running" => CommandStatus::Running,
                "finished" => CommandStatus::Finished,
                "lost" => CommandStatus::Lost,
                _ => return Err(CommandOwnerError::Invalid),
            };
            Ok(CommandSnapshot {
                id,
                version,
                status,
                output: rendered_output(
                    &output,
                    matches!(status, CommandStatus::Finished | CommandStatus::Lost),
                ),
                process: receipt
                    .map(|text| serde_json::from_str(&text))
                    .transpose()?,
                result: result.map(|text| serde_json::from_str(&text)).transpose()?,
            })
        })
        .await
    }

    pub(super) async fn acknowledge(&self, ids: Vec<ToolCallId>) -> Result<(), CommandOwnerError> {
        self.call(move |db| {
            use sha2::{Digest as _, Sha256};
            let tx = db.transaction()?;
            for id in ids {
                let row: Option<(Option<String>, String)> = tx
                    .query_row(
                        "SELECT request,status FROM commands WHERE id=?1",
                        [id.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                let Some((request, status)) = row else {
                    let retired: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM retired_commands WHERE id=?1)",
                        [id.to_string()],
                        |row| row.get(0),
                    )?;
                    if retired {
                        continue;
                    }
                    return Err(CommandOwnerError::NotFound);
                };
                if !matches!(status.as_str(), "finished" | "lost") {
                    return Err(CommandOwnerError::Invalid);
                }
                let digest = request.map(|request| Sha256::digest(request.as_bytes()).to_vec());
                tx.execute(
                    "INSERT INTO retired_commands(id,request_sha256) VALUES (?1,?2)",
                    params![id.to_string(), digest],
                )?;
                tx.execute("DELETE FROM commands WHERE id=?1", [id.to_string()])?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub(super) async fn cancel(&self, id: ToolCallId) -> Result<(), CommandOwnerError> {
        self.call(move |db| {
            match reject_retired(db, id, None) {
                Err(CommandOwnerError::Retired) => return Ok(()),
                Err(error) => return Err(error),
                Ok(()) => {},
            }
            let tx = db.transaction()?;
            if tx.execute("UPDATE commands SET cancel_requested=1 WHERE id=?1", [id.to_string()])? == 0 {
                let count: usize = tx.query_row("SELECT COUNT(*) FROM commands", [], |row| row.get(0))?;
                if count >= MAX_RETAINED { return Err(CommandOwnerError::Full); }
                let result = ToolOutput::interrupted("command was cancelled before admission");
                tx.execute("INSERT INTO commands(id,request,status,version,cancel_requested,result) VALUES (?1,NULL,'finished',1,1,?2)", params![id.to_string(), serde_json::to_string(&result)?])?;
            }
            tx.commit()?;
            #[cfg(test)]
            wait_at_commit(&CANCELLATION_HOOKS, id);
            Ok(())
        }).await
    }
    pub(super) async fn cancel_requested(&self, id: ToolCallId) -> Result<bool, CommandOwnerError> {
        self.call(move |db| {
            Ok(db.query_row(
                "SELECT cancel_requested FROM commands WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )?)
        })
        .await
    }
}

fn reject_retired(
    db: &Connection,
    id: ToolCallId,
    request: Option<&str>,
) -> Result<(), CommandOwnerError> {
    use sha2::{Digest as _, Sha256};
    let retired: Option<Option<Vec<u8>>> = db
        .query_row(
            "SELECT request_sha256 FROM retired_commands WHERE id=?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    match retired {
        None => Ok(()),
        Some(Some(previous))
            if request
                .is_some_and(|request| previous != Sha256::digest(request.as_bytes()).to_vec()) =>
        {
            Err(CommandOwnerError::Conflict)
        }
        Some(_) => Err(CommandOwnerError::Retired),
    }
}

fn rendered_output(bytes: &[u8], terminal: bool) -> String {
    let mut end = bytes.len();
    if !terminal {
        let mut offset = 0;
        while let Err(error) = std::str::from_utf8(&bytes[offset..]) {
            if let Some(invalid) = error.error_len() {
                offset += error.valid_up_to() + invalid;
            } else {
                end = offset + error.valid_up_to();
                break;
            }
        }
    }
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

// SQLite reopens pathnames, including its WAL and shared-memory files. Every
// ancestor must therefore prevent another UID from replacing a checked entry.
// Resolve trusted symlinks one component at a time; canonicalizing first would
// conceal a symlink reached through an attacker-controlled directory.
#[cfg(unix)]
fn trusted_parent(path: &Path) -> Result<PathBuf, CommandOwnerError> {
    use std::{os::unix::fs::MetadataExt as _, path::Component};
    if !path.is_absolute() {
        return Err(CommandOwnerError::PrivateState);
    }
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Normal(_) => resolved.push(component),
            _ => return Err(CommandOwnerError::PrivateState),
        }
        let metadata = std::fs::symlink_metadata(&resolved)?;
        if metadata.file_type().is_symlink() {
            if !trusted_uid(metadata.uid()) {
                return Err(CommandOwnerError::PrivateState);
            }
            resolved = std::fs::canonicalize(&resolved)?;
            // Check from the root down so each entry's parent is already
            // protected when the entry itself is inspected.
            for ancestor in resolved.ancestors().collect::<Vec<_>>().into_iter().rev() {
                trusted_directory(&std::fs::symlink_metadata(ancestor)?)?;
            }
        } else {
            trusted_directory(&metadata)?;
        }
    }
    Ok(resolved)
}

#[cfg(unix)]
fn trusted_directory(metadata: &std::fs::Metadata) -> Result<(), CommandOwnerError> {
    use std::os::unix::fs::MetadataExt as _;
    let mode = metadata.mode();
    if !metadata.is_dir()
        || !trusted_uid(metadata.uid())
        || (mode & 0o022 != 0 && mode & 0o1000 == 0)
    {
        return Err(CommandOwnerError::PrivateState);
    }
    Ok(())
}

#[cfg(unix)]
fn trusted_uid(uid: u32) -> bool {
    uid == 0 || uid == rustix::process::getuid().as_raw()
}

fn private_file(path: &Path) -> Result<File, CommandOwnerError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            return Err(CommandOwnerError::PrivateState);
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options
            .custom_flags(super::windows_state::FILE_FLAG_OPEN_REPARSE_POINT)
            .share_mode(super::windows_state::SHARE_READ_WRITE);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(CommandOwnerError::PrivateState);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
            || metadata.uid() != rustix::process::getuid().as_raw()
        {
            return Err(CommandOwnerError::PrivateState);
        }
    }
    #[cfg(windows)]
    super::windows_state::verify(&file, false)?;
    Ok(file)
}
