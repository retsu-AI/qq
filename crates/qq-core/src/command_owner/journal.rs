use qq_protocol::ToolCallId;
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
    MAX_LIVE_OUTPUT_BYTES, MAX_RETAINED, NativeCommand, ToolOutput, blocking,
};

#[derive(Clone)]
pub(super) struct Journal(Arc<JournalState>);
struct JournalState {
    connection: Mutex<Option<Connection>>,
    lock: Mutex<Option<File>>,
}

struct StoredSnapshot {
    version: u64,
    status: String,
    receipt: Option<String>,
    result: Option<String>,
    output: String,
}

impl Journal {
    pub(super) async fn open(directory: PathBuf) -> Result<Self, CommandOwnerError> {
        blocking(move || Self::open_blocking(directory)).await
    }

    fn open_blocking(directory: PathBuf) -> Result<Self, CommandOwnerError> {
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
        let lock = private_file(&directory.join("owner.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(CommandOwnerError::Busy),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        let path = directory.join("commands.sqlite3");
        let _database_file = private_file(&path)?;
        let connection = Connection::open(path)?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if !matches!(version, 0 | 1) {
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
            connection.execute_batch(
                "BEGIN IMMEDIATE;
                CREATE TABLE commands (
                    id TEXT PRIMARY KEY,
                    request TEXT CHECK(request IS NULL OR length(CAST(request AS BLOB))<=262144),
                    status TEXT NOT NULL CHECK(status IN ('queued','running','finished','lost')),
                    version INTEGER NOT NULL DEFAULT 0 CHECK(version>=0),
                    cancel_requested INTEGER NOT NULL DEFAULT 0 CHECK(cancel_requested IN (0,1)),
                    output TEXT NOT NULL DEFAULT '' CHECK(length(CAST(output AS BLOB))<=393216),
                    receipt TEXT CHECK(receipt IS NULL OR length(CAST(receipt AS BLOB))<=16384),
                    result TEXT CHECK(result IS NULL OR length(CAST(result AS BLOB))<=1048576)
                );
                PRAGMA user_version=1;
                COMMIT;",
            )?;
        }
        connection.execute(
            "UPDATE commands SET status='lost',version=version+1 WHERE status IN ('queued','running')",
            [],
        )?;
        Ok(Self(Arc::new(JournalState {
            connection: Mutex::new(Some(connection)),
            lock: Mutex::new(Some(lock)),
        })))
    }

    async fn call<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T, CommandOwnerError> + Send + 'static,
    ) -> Result<T, CommandOwnerError> {
        let state = Arc::clone(&self.0);
        blocking(move || {
            let mut connection = state
                .connection
                .lock()
                .map_err(|_| CommandOwnerError::Unavailable)?;
            operation(connection.as_mut().ok_or(CommandOwnerError::Unavailable)?)
        })
        .await
    }

    pub(super) async fn close(&self) -> Result<(), CommandOwnerError> {
        let state = Arc::clone(&self.0);
        blocking(move || {
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
                Some(None) | None => Ok(false),
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
            let request: NativeCommand = serde_json::from_str(&request)?;
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
        chunk: String,
    ) -> Result<u64, CommandOwnerError> {
        self.call(move |db| {
            let tx = db.transaction()?;
            let (mut output, version): (String, u64) = tx.query_row(
                "SELECT output,version FROM commands WHERE id=?1 AND status='running'",
                [id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let remaining = MAX_LIVE_OUTPUT_BYTES.saturating_sub(output.len());
            let end = chunk.floor_char_boundary(remaining.min(chunk.len()));
            if end == 0 {
                return Ok(version);
            }
            output.push_str(&chunk[..end]);
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
                output,
                process: receipt
                    .map(|text| serde_json::from_str(&text))
                    .transpose()?,
                result: result.map(|text| serde_json::from_str(&text)).transpose()?,
            })
        })
        .await
    }

    pub(super) async fn cancel(&self, id: ToolCallId) -> Result<(), CommandOwnerError> {
        self.call(move |db| {
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
    Ok(file)
}
