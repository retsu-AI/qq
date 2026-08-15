use std::{
    collections::HashMap,
    future::Future,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_stream::stream;
use futures_core::Stream;
use futures_util::{FutureExt, StreamExt};
use qq_protocol::{
    AccountingTotal, ApprovalDecision, ApprovalGrant, ApprovalMode, ApprovalResolution, CommandId,
    CommandOutcome, CommandReceipt, EditPreview, EventCursor, MessageId, MessageRole,
    MessageSnapshot, MessageState, ModelPricing, ModelSelection, ReasoningEvent, RunActivity,
    RunFailure, RunFailureKind, RunId, RunOutcome, RunPromptIdentity, RunSnapshot, RunStatus,
    SessionAccounting, SessionCommand, SessionEvent, SessionEventEnvelope, SessionId,
    SessionSnapshot, SessionStatus, SessionSummary, ShellCommandPreview, SnapshotRequest, StoreId,
    SubscribeRequest, TextChannel, TokenUsage, ToolCallDisplay, ToolCallId, ToolCallSnapshot,
    ToolCallState, WorkspaceGrantOutcome, WorkspaceId, WorkspaceSnapshot, WorkspaceSummary,
};
use qq_provider::{ContentBlock, Message, Role};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{RwLock, Semaphore, mpsc, oneshot, watch};

use crate::{
    GateDecision, RunCapabilities, Runtime, RuntimeEvent, RuntimeToolCall, SpawnAgentFuture,
    SpawnAgentOutcome, SubagentSpawner, ToolGate, ToolGateFuture, approval,
    workspace::{FileState, FileStateUpdate},
};

mod approvals;
mod execution;
mod runtime;
mod scheduler;
mod store;
mod subagents;

pub use runtime::{
    ApprovalReviewer, GrantPromotionFuture, GrantSeedFuture, LoadedRuntime, ReviewFuture,
    ReviewRequest, ReviewVerdict, RuntimeLoadError, RuntimeLoadFuture, RuntimeLoadRequest,
    RuntimeLoader, SessionEventStream, SessionRuntime, SessionRuntimeError, SessionRuntimeOptions,
    SpawnModelValidationFuture, WorkerRuntimeLoadFuture, WorkspaceGrantAuthority,
    WorkspaceGrantSeed,
};

use approvals::ConcludedApproval;
use execution::{ModelTurnCommit, RunAccounting, add_usage};
use store::Store;
#[cfg(test)]
use store::{Priority, has_column, open_database};
#[cfg(test)]
use subagents::spawn_child_run;

const MAX_PENDING_PROMPTS: u16 = 16;
const MAX_CONTEXT_BYTES: usize = 4 * 1024 * 1024;
/// Auto-compaction trigger: when a prompt run is about to be claimed and the
/// session's assembled (pruned) context — including the queued prompt —
/// exceeds this share of [`MAX_CONTEXT_BYTES`], a compaction run is claimed
/// first and the prompt runs right after it. ~70% of the session budget. A
/// second trigger on the model context window (the run's last reported
/// `context_tokens` against the window) needs the window plumbed into
/// qq-core; today it lives only in the client-facing model catalog, so the
/// byte threshold is the sole automatic signal.
const AUTO_COMPACT_CONTEXT_BYTES: usize = MAX_CONTEXT_BYTES / 10 * 7;
/// The assembly recency window: the last K model turns keep their tool
/// results verbatim. Read-only results older than that are replaced by
/// one-line stubs during context assembly (the stored rows are untouched).
/// Sits with the context budget because the budget measures the assembled,
/// pruned size.
const CONTEXT_PRUNE_KEEP_TURNS: usize = 4;
/// Longest argument excerpt embedded in a pruned-result stub.
const CONTEXT_PRUNE_STUB_ARGUMENT_BYTES: usize = 256;
/// Compaction summaries retained per session, newest first. History is kept
/// (not deleted eagerly) so a bad compaction can be rolled back later.
const COMPACTION_HISTORY_ROWS: u32 = 3;
/// Longest summary excerpt carried on the `SessionCompacted` event.
const MAX_EVENT_SUMMARY_BYTES: usize = 16 * 1024;
const MAX_PROMPT_BYTES: usize = 128 * 1024;
const MAX_REPLAY_EVENTS: u16 = 128;
const MAX_SNAPSHOT_SESSIONS: u16 = 512;
const MAX_SNAPSHOT_MESSAGES: u16 = 256;
const MAX_SNAPSHOT_TOOL_CALLS: usize = 4_096;
const MAX_TEXT_CHUNK_BYTES: usize = 64 * 1024;
const MAX_FAILURE_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_WORKSPACES: u32 = 1024;
const MAX_SESSIONS_PER_WORKSPACE: u32 = 512;
const MAX_COMMANDS: u32 = 100_000;
const MAX_MODEL_SELECTION_BYTES: usize = 512;
const OUTPUT_BATCH_BYTES: usize = 8 * 1024;
const OUTPUT_BATCH_DELAY: Duration = Duration::from_millis(8);
const MAX_PERSISTED_EVENT_BYTES: usize = 1024 * 1024;
const MAX_GRANT_BYTES: usize = 256;
const MAX_SESSION_GRANTS: u32 = 256;
const MAX_SESSION_FILES: u32 = 4_096;
const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
/// Child runs one parent run may hold in flight at once. Spawn calls beyond
/// this cap queue behind it inside the parent's turn rather than failing.
const MAX_CONCURRENT_CHILDREN_PER_RUN: usize = 3;
/// Total children one parent run may spawn before further `spawn_agent`
/// calls return a tool error.
const MAX_SPAWNED_CHILDREN_PER_RUN: usize = 8;
const INTERRUPTED_TOOL_RESULT: &str =
    "Tool execution was interrupted before a durable result was recorded.";
const RUNTIME_NOTICE_PREAMBLE: &str = "[QQ runtime notice; not a user instruction]";
const RUNTIME_NOTICE_GUIDANCE: &str = "Continue from the committed history above. Do not \
    automatically retry tool calls whose result says execution was interrupted.";
/// Read-only built-in tools whose results context assembly may replace with
/// stubs: the agent can re-derive them on demand. Mutating, shell, and MCP
/// results are never pruned — their outputs are not re-derivable.
const PRUNABLE_READ_ONLY_TOOLS: [&str; 3] = ["read_file", "list_dir", "search"];
/// Prefixes the latest compaction summary when assembly replays it as the
/// conversation's opening message.
const COMPACTION_SUMMARY_PREAMBLE: &str = "The earlier part of this conversation was compacted \
into the summary below. Treat it as authoritative context; the verbatim conversation resumes \
after it.";
/// The fixed instruction appended as the final user message of a compaction
/// run. It demands the structured schema; the mechanically seeded file list
/// is appended beneath it.
const COMPACTION_INSTRUCTION: &str = "Summarize this conversation so it can replace the \
transcript as model context. Do not call any tools. Reply with exactly these sections:\n\
1. Intent: what the user is trying to accomplish, in their terms.\n\
2. Decisions and constraints: each decision with its why. Use exact names, paths, and flags \
verbatim; vague references are forbidden.\n\
3. Work state: what was done, what is in flight, what is pending.\n\
4. Files touched: annotate the seeded list below with each file's role; add any files it is \
missing.\n\
5. Errors: every error seen and how it was resolved, with error strings verbatim.\n\
6. User messages: every user message, preserved verbatim or near-verbatim.\n\
If the conversation begins with a prior compaction summary, fold it into these sections rather \
than referring to it.";

/// What a run row exists for. Prompt runs answer a user message and their
/// output joins the transcript; compaction runs are internal — their request
/// and streamed output never become session messages, and their product is a
/// summary row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunKind {
    Prompt,
    Compaction,
}

fn parse_run_kind(value: &str) -> Result<RunKind, SessionRuntimeError> {
    match value {
        "prompt" => Ok(RunKind::Prompt),
        "compaction" => Ok(RunKind::Compaction),
        _ => Err(SessionRuntimeError::Persistence),
    }
}

#[derive(Clone)]
struct ClaimedRun {
    workspace_id: WorkspaceId,
    workspace: String,
    session_id: SessionId,
    run_id: RunId,
    command_id: CommandId,
    kind: RunKind,
    /// Whether the run belongs to a child (sub-agent) session. Guaranteed by
    /// the claim query's parent filter; child runs may not spawn further
    /// children.
    child: bool,
    /// True only when this run's command is present in the durable command
    /// journal. Runtime- and model-created runs use generated command ids but
    /// intentionally have no command row.
    user_initiated: bool,
    /// The durable command used `//` to escape runtime slash semantics. Its
    /// message was normalized before `PromptQueued`, so preparation must not
    /// reinterpret the resulting leading slash.
    literal_slash: bool,
    model: ModelSelection,
    messages: Vec<Message>,
    started: SessionEventEnvelope,
    /// The prompt's assembled context still exceeded the hard budget when it
    /// was claimed — after the one auto-compaction attempt the claim path
    /// guarantees. The run fails immediately with the context policy failure
    /// instead of reaching the model.
    over_budget: bool,
}

struct AppliedCommand {
    receipt: CommandReceipt,
    schedule: bool,
    /// Other runs whose in-memory cancellation must be signalled with this
    /// command: an auto-compaction made unnecessary by its queued prompt, or
    /// running child work durably owned by a cancelled parent.
    cascade_cancels: Vec<RunId>,
    /// Set when a first-applied approve-for-workspace decision needs its
    /// grant promoted into workspace configuration. Idempotent replays never
    /// set it: retried commands must not re-run the config write.
    promotion: Option<PendingGrantPromotion>,
}

struct CreatedChildRun {
    session_id: SessionId,
    run_id: RunId,
    committed_through: EventCursor,
}

/// One approve-for-workspace promotion carried out of the command
/// transaction. The durable config write happens after the approval commits,
/// so a promotion failure can never fail the approval that requested it.
struct PendingGrantPromotion {
    workspace_id: WorkspaceId,
    workspace_path: String,
    session_id: SessionId,
    run_id: RunId,
    command_id: CommandId,
    grant: ApprovalGrant,
}

fn create_child_run(
    connection: &mut Connection,
    store_id: StoreId,
    workspace_id: WorkspaceId,
    parent_session_id: SessionId,
    parent_run_id: RunId,
    model: ModelSelection,
    task: String,
) -> Result<CreatedChildRun, SessionRuntimeError> {
    validate_model_selection(&model)?;
    let task = task.trim().to_owned();
    if task.is_empty() {
        return Err(SessionRuntimeError::EmptyPrompt);
    }
    if task.len() > MAX_PROMPT_BYTES {
        return Err(SessionRuntimeError::PromptTooLarge);
    }
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    ensure_workspace(&transaction, workspace_id)?;
    let parent_workspace = transaction
        .query_row(
            "SELECT s.workspace_id
             FROM sessions s JOIN runs r ON r.session_id = s.id
             WHERE s.id = ?1 AND r.id = ?2 AND r.status = 'running'
               AND r.cancel_requested = 0",
            params![parent_session_id.to_string(), parent_run_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?
        .ok_or(SessionRuntimeError::RunNotFound)?;
    if parse_id::<WorkspaceId>(&parent_workspace)? != workspace_id {
        return Err(SessionRuntimeError::ParentWorkspaceMismatch);
    }
    let session_count: u32 = transaction
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE workspace_id = ?1",
            [workspace_id.to_string()],
            |row| row.get(0),
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if session_count >= MAX_SESSIONS_PER_WORKSPACE {
        return Err(SessionRuntimeError::SessionLimitReached);
    }

    let session_id = SessionId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    let run_id = RunId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    // This is one internal operation rather than a replayable client command;
    // a generated id satisfies the run's uniqueness contract and links both
    // child events to the same atomic cause.
    let command_id = CommandId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    let user_message_id = MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    let assistant_message_id =
        MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    let now = now_ms();
    transaction
        .execute(
            "INSERT INTO sessions(
                id, workspace_id, parent_id, owner_run_id, title, status, queued_prompts,
                model, max_output_tokens, organization, approval_mode,
                created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'queued', 1, ?6, ?7, ?8, 'read_only', ?9, ?9)",
            params![
                session_id.to_string(),
                workspace_id.to_string(),
                parent_session_id.to_string(),
                parent_run_id.to_string(),
                prompt_title(&task),
                model.model,
                model.max_output_tokens,
                model.organization,
                now,
            ],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "INSERT INTO runs(
                id, session_id, command_id, user_message_id, assistant_message_id,
                status, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6)",
            params![
                run_id.to_string(),
                session_id.to_string(),
                command_id.to_string(),
                user_message_id.to_string(),
                assistant_message_id.to_string(),
                now,
            ],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "INSERT INTO messages(
                id, session_id, run_id, ordinal, role, state, output, created_at_ms
             ) VALUES (?1, ?2, ?3, 1, 'user', 'queued', ?4, ?5)",
            params![
                user_message_id.to_string(),
                session_id.to_string(),
                run_id.to_string(),
                task,
                now,
            ],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;

    let session = load_session_summary(&transaction, session_id)?;
    let created = append_event(
        &transaction,
        EventContext {
            store_id,
            workspace_id,
            session_id,
            run_id: Some(run_id),
            caused_by: Some(command_id),
            occurred_at_ms: now,
        },
        SessionEvent::SessionCreated {
            session: session.clone(),
        },
    )?;
    let message = load_message(&transaction, user_message_id)?;
    let run = load_run(&transaction, run_id)?;
    let queued = append_event(
        &transaction,
        EventContext {
            store_id,
            workspace_id,
            session_id,
            run_id: Some(run_id),
            caused_by: Some(command_id),
            occurred_at_ms: now,
        },
        SessionEvent::PromptQueued {
            session,
            message,
            run,
            queue_position: 1,
        },
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    debug_assert_eq!(created.cursor.sequence + 1, queued.cursor.sequence);
    Ok(CreatedChildRun {
        session_id,
        run_id,
        committed_through: queued.cursor,
    })
}

fn execute_command(
    connection: &mut Connection,
    store_id: StoreId,
    command_id: CommandId,
    command: SessionCommand,
    seed: &WorkspaceGrantSeed,
) -> Result<AppliedCommand, SessionRuntimeError> {
    let request_json =
        serde_json::to_string(&command).map_err(|_| SessionRuntimeError::Persistence)?;
    if let Some((stored_request, stored_receipt)) = connection
        .query_row(
            "SELECT request_json, receipt_json FROM commands WHERE id = ?1",
            [command_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?
    {
        if stored_request != request_json {
            return Err(SessionRuntimeError::IdempotencyConflict);
        }
        let receipt =
            serde_json::from_str(&stored_receipt).map_err(|_| SessionRuntimeError::Persistence)?;
        return Ok(AppliedCommand {
            receipt,
            schedule: false,
            cascade_cancels: match &command {
                SessionCommand::CancelRun { run_id } => owned_running_run_ids(connection, *run_id)?,
                _ => Vec::new(),
            },
            promotion: None,
        });
    }
    let command_count: u32 = connection
        .query_row("SELECT COUNT(*) FROM commands", [], |row| row.get(0))
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if command_count >= MAX_COMMANDS {
        return Err(SessionRuntimeError::CommandLimitReached);
    }

    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let now = now_ms();
    let mut promotion = None;
    let mut cascade_cancels = Vec::new();
    let (receipt, schedule) = match command {
        SessionCommand::ResolveWorkspace { path } => {
            let path = path.trim();
            if path.is_empty() {
                return Err(SessionRuntimeError::EmptyWorkspace);
            }
            let canonical =
                std::fs::canonicalize(path).map_err(|_| SessionRuntimeError::InvalidWorkspace)?;
            if !canonical.is_dir() {
                return Err(SessionRuntimeError::InvalidWorkspace);
            }
            let path = canonical
                .to_str()
                .ok_or(SessionRuntimeError::InvalidWorkspace)?;
            let existing = transaction
                .query_row(
                    "SELECT id, next_sequence FROM workspaces WHERE path = ?1",
                    [path],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)),
                )
                .optional()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            let (workspace_id, sequence) = match existing {
                Some((id, sequence)) => (parse_id(&id)?, sequence),
                None => {
                    let workspace_count: u32 = transaction
                        .query_row("SELECT COUNT(*) FROM workspaces", [], |row| row.get(0))
                        .map_err(|_| SessionRuntimeError::Persistence)?;
                    if workspace_count >= MAX_WORKSPACES {
                        return Err(SessionRuntimeError::WorkspaceLimitReached);
                    }
                    let workspace_id =
                        WorkspaceId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
                    transaction
                        .execute(
                            "INSERT INTO workspaces(id, path, next_sequence) VALUES (?1, ?2, 0)",
                            params![workspace_id.to_string(), path],
                        )
                        .map_err(|_| SessionRuntimeError::Persistence)?;
                    (workspace_id, 0)
                }
            };
            (
                CommandReceipt {
                    command_id,
                    committed_through: EventCursor {
                        store_id,
                        workspace_id,
                        sequence,
                    },
                    outcome: CommandOutcome::WorkspaceResolved { workspace_id },
                },
                false,
            )
        }
        SessionCommand::CreateSession {
            workspace_id,
            parent_id,
            model,
            approval_mode,
        } => {
            validate_model_selection(&model)?;
            ensure_workspace(&transaction, workspace_id)?;
            let session_count: u32 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM sessions WHERE workspace_id = ?1",
                    [workspace_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            if session_count >= MAX_SESSIONS_PER_WORKSPACE {
                return Err(SessionRuntimeError::SessionLimitReached);
            }
            if let Some(parent_id) = parent_id {
                let parent_workspace = transaction
                    .query_row(
                        "SELECT workspace_id FROM sessions WHERE id = ?1",
                        [parent_id.to_string()],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(|_| SessionRuntimeError::Persistence)?
                    .ok_or(SessionRuntimeError::SessionNotFound)?;
                if parse_id::<WorkspaceId>(&parent_workspace)? != workspace_id {
                    return Err(SessionRuntimeError::ParentWorkspaceMismatch);
                }
            }
            let session_id = SessionId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            transaction
                .execute(
                    "INSERT INTO sessions(
                        id, workspace_id, parent_id, title, status, model,
                        max_output_tokens, organization, approval_mode,
                        created_at_ms, updated_at_ms
                     ) VALUES (?1, ?2, ?3, 'New session', 'idle', ?4, ?5, ?6, ?7, ?8, ?8)",
                    params![
                        session_id.to_string(),
                        workspace_id.to_string(),
                        parent_id.map(|id| id.to_string()),
                        model.model,
                        model.max_output_tokens,
                        model.organization,
                        approval_mode_str(approval_mode),
                        now,
                    ],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            insert_seed_grants(&transaction, session_id, seed, now)?;
            let summary = load_session_summary(&transaction, session_id)?;
            let event = append_event(
                &transaction,
                EventContext {
                    store_id,
                    workspace_id,
                    session_id,
                    run_id: None,
                    caused_by: Some(command_id),
                    occurred_at_ms: now,
                },
                SessionEvent::SessionCreated { session: summary },
            )?;
            (
                CommandReceipt {
                    command_id,
                    committed_through: event.cursor,
                    outcome: CommandOutcome::SessionCreated { session_id },
                },
                false,
            )
        }
        SessionCommand::SubmitPrompt { session_id, prompt } => {
            let prompt = prompt.trim().to_owned();
            if prompt.is_empty() {
                return Err(SessionRuntimeError::EmptyPrompt);
            }
            if prompt.len() > MAX_PROMPT_BYTES {
                return Err(SessionRuntimeError::PromptTooLarge);
            }
            let prompt = prompt
                .strip_prefix("//")
                .map_or(prompt.clone(), |literal| format!("/{literal}"));
            let (workspace_id, queued, title) = transaction
                .query_row(
                    "SELECT workspace_id, queued_prompts, title FROM sessions WHERE id = ?1",
                    [session_id.to_string()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, u16>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()
                .map_err(|_| SessionRuntimeError::Persistence)?
                .ok_or(SessionRuntimeError::SessionNotFound)?;
            if queued >= MAX_PENDING_PROMPTS {
                return Err(SessionRuntimeError::QueueFull);
            }
            // An over-budget prompt is admitted rather than rejected: claiming
            // it auto-compacts the session first and re-checks, so the hard
            // budget fails the run only after one compaction attempt could
            // not shrink the assembly under it (the last resort, not the
            // policy).
            let workspace_id = parse_id(&workspace_id)?;
            let run_id = RunId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            let message_id = MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            // Assistant message rows are created lazily, one per model turn,
            // at each turn's first text delta. The run row's
            // assistant_message_id starts as a placeholder and is updated to
            // the current turn's message as the run advances.
            let assistant_message_id =
                MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            let ordinal: u64 = transaction
                .query_row(
                    "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM messages WHERE session_id = ?1",
                    [session_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .execute(
                    "INSERT INTO runs(
                        id, session_id, command_id, user_message_id, assistant_message_id,
                        status, created_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6)",
                    params![
                        run_id.to_string(),
                        session_id.to_string(),
                        command_id.to_string(),
                        message_id.to_string(),
                        assistant_message_id.to_string(),
                        now,
                    ],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .execute(
                    "INSERT INTO messages(
                        id, session_id, run_id, ordinal, role, state, output, created_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, 'user', 'queued', ?5, ?6)",
                    params![
                        message_id.to_string(),
                        session_id.to_string(),
                        run_id.to_string(),
                        ordinal,
                        prompt,
                        now,
                    ],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            let next_queued = queued + 1;
            let next_title = if ordinal == 1 {
                prompt_title(&prompt)
            } else {
                title
            };
            transaction
                .execute(
                    "UPDATE sessions
                     SET title = ?2, status = CASE WHEN active_run_id IS NULL THEN 'queued' ELSE status END,
                         queued_prompts = ?3, updated_at_ms = ?4
                     WHERE id = ?1",
                    params![session_id.to_string(), next_title, next_queued, now],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            let summary = load_session_summary(&transaction, session_id)?;
            let message = load_message(&transaction, message_id)?;
            let run = load_run(&transaction, run_id)?;
            let event = append_event(
                &transaction,
                EventContext {
                    store_id,
                    workspace_id,
                    session_id,
                    run_id: Some(run_id),
                    caused_by: Some(command_id),
                    occurred_at_ms: now,
                },
                SessionEvent::PromptQueued {
                    session: summary,
                    message,
                    run,
                    queue_position: next_queued,
                },
            )?;
            (
                CommandReceipt {
                    command_id,
                    committed_through: event.cursor,
                    outcome: CommandOutcome::PromptQueued {
                        session_id,
                        run_id,
                        queue_position: next_queued,
                    },
                },
                true,
            )
        }
        SessionCommand::CancelRun { run_id } => {
            let (session_id, status, stored_outcome) = transaction
                .query_row(
                    "SELECT session_id, status, outcome_json FROM runs WHERE id = ?1",
                    [run_id.to_string()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .optional()
                .map_err(|_| SessionRuntimeError::Persistence)?
                .ok_or(SessionRuntimeError::RunNotFound)?;
            let session_id = parse_id(&session_id)?;
            let workspace_id = session_workspace(&transaction, session_id)?;
            if let Some(outcome) = stored_outcome {
                let outcome =
                    serde_json::from_str(&outcome).map_err(|_| SessionRuntimeError::Persistence)?;
                let sequence = workspace_sequence(&transaction, workspace_id)?;
                (
                    CommandReceipt {
                        command_id,
                        committed_through: EventCursor {
                            store_id,
                            workspace_id,
                            sequence,
                        },
                        outcome: CommandOutcome::RunAlreadyFinished { run_id, outcome },
                    },
                    false,
                )
            } else {
                transaction
                    .execute(
                        "UPDATE runs SET cancel_requested = 1 WHERE id = ?1",
                        [run_id.to_string()],
                    )
                    .map_err(|_| SessionRuntimeError::Persistence)?;
                let summary = load_session_summary(&transaction, session_id)?;
                let requested = append_event(
                    &transaction,
                    EventContext {
                        store_id,
                        workspace_id,
                        session_id,
                        run_id: Some(run_id),
                        caused_by: Some(command_id),
                        occurred_at_ms: now,
                    },
                    SessionEvent::CancellationRequested {
                        session: summary,
                        run_id,
                    },
                )?;
                let mut cursor = if status == "queued" {
                    let finished = finish_queued_run(
                        &transaction,
                        store_id,
                        workspace_id,
                        session_id,
                        run_id,
                        now,
                    )?;
                    // Cancelling the session's last queued prompt cascades to
                    // the auto-compaction running on its behalf: with nothing
                    // left to run after it, the summarization is pure cost. A
                    // manual compaction (auto_compaction = 0) is never
                    // cascaded — the user asked for it directly.
                    match cascade_auto_compaction_cancel(
                        &transaction,
                        store_id,
                        workspace_id,
                        session_id,
                        command_id,
                        now,
                    )? {
                        Some((compaction_run, event)) => {
                            cascade_cancels.push(compaction_run);
                            event.cursor
                        }
                        None => finished.cursor,
                    }
                } else {
                    requested.cursor
                };
                let owned =
                    cancel_owned_child_runs(&transaction, store_id, run_id, command_id, now)?;
                if let Some(child_cursor) = owned.committed_through {
                    cursor = child_cursor;
                }
                cascade_cancels.extend(owned.running);
                (
                    CommandReceipt {
                        command_id,
                        committed_through: cursor,
                        outcome: CommandOutcome::CancellationRequested { run_id },
                    },
                    status == "queued" || owned.settled_queued,
                )
            }
        }
        SessionCommand::RespondToolApproval {
            run_id,
            tool_call_id,
            decision,
        } => {
            let (call_run, state, resolution) = transaction
                .query_row(
                    "SELECT run_id, state, approval_resolution FROM tool_calls WHERE id = ?1",
                    [tool_call_id.to_string()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .optional()
                .map_err(|_| SessionRuntimeError::Persistence)?
                .ok_or(SessionRuntimeError::ToolCallNotFound)?;
            if parse_id::<RunId>(&call_run)? != run_id {
                return Err(SessionRuntimeError::ToolCallNotFound);
            }
            let session_id: SessionId = transaction
                .query_row(
                    "SELECT session_id FROM runs WHERE id = ?1",
                    [run_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .map_err(|_| SessionRuntimeError::Persistence)
                .and_then(|session| parse_id(&session))?;
            let workspace_id = session_workspace(&transaction, session_id)?;
            if let Some(resolution) = resolution {
                // Idempotent: a second response returns the recorded outcome
                // without touching the call again.
                let resolution = parse_approval_resolution(&resolution)?;
                let sequence = workspace_sequence(&transaction, workspace_id)?;
                (
                    CommandReceipt {
                        command_id,
                        committed_through: EventCursor {
                            store_id,
                            workspace_id,
                            sequence,
                        },
                        outcome: CommandOutcome::ToolApprovalResolved {
                            tool_call_id,
                            resolution,
                        },
                    },
                    false,
                )
            } else {
                if state != "awaiting_approval" {
                    return Err(SessionRuntimeError::ApprovalNotPending);
                }
                let resolution = match &decision {
                    ApprovalDecision::ApproveOnce => ApprovalResolution::ApprovedOnce,
                    ApprovalDecision::ApproveForSession { .. } => {
                        ApprovalResolution::ApprovedForSession
                    }
                    ApprovalDecision::ApproveForWorkspace { .. } => {
                        ApprovalResolution::ApprovedForWorkspace
                    }
                    ApprovalDecision::Deny => ApprovalResolution::Denied,
                };
                match &decision {
                    ApprovalDecision::ApproveOnce
                    | ApprovalDecision::ApproveForSession { .. }
                    | ApprovalDecision::ApproveForWorkspace { .. } => {
                        transaction
                            .execute(
                                "UPDATE tool_calls
                                 SET state = 'requested', approval_resolution = ?2,
                                     resolved_at_ms = ?3
                                 WHERE id = ?1 AND state = 'awaiting_approval'",
                                params![
                                    tool_call_id.to_string(),
                                    approval_resolution_str(resolution),
                                    now,
                                ],
                            )
                            .map_err(|_| SessionRuntimeError::Persistence)?;
                    }
                    ApprovalDecision::Deny => {
                        transaction
                            .execute(
                                "UPDATE tool_calls
                                 SET state = 'denied', result = ?2, is_error = 1,
                                     approval_resolution = ?3, resolved_at_ms = ?4,
                                     finished_at_ms = ?4
                                 WHERE id = ?1 AND state = 'awaiting_approval'",
                                params![
                                    tool_call_id.to_string(),
                                    approval::USER_DENIED_RESULT,
                                    approval_resolution_str(resolution),
                                    now,
                                ],
                            )
                            .map_err(|_| SessionRuntimeError::Persistence)?;
                    }
                }
                // Approve-for-workspace records the same session grant as
                // approve-for-session — the running session must proceed on
                // it immediately — and additionally schedules the promotion
                // below, outside this transaction.
                if let ApprovalDecision::ApproveForSession { grant }
                | ApprovalDecision::ApproveForWorkspace { grant } = &decision
                {
                    let (kind, value) = match grant {
                        ApprovalGrant::Tool { name } => ("tool", name.trim()),
                        ApprovalGrant::ShellPrefix { prefix } => ("shell_prefix", prefix.trim()),
                    };
                    if value.is_empty() || value.len() > MAX_GRANT_BYTES {
                        return Err(SessionRuntimeError::InvalidApprovalGrant);
                    }
                    let grant_count: u32 = transaction
                        .query_row(
                            "SELECT COUNT(*) FROM session_grants WHERE session_id = ?1",
                            [session_id.to_string()],
                            |row| row.get(0),
                        )
                        .map_err(|_| SessionRuntimeError::Persistence)?;
                    if grant_count >= MAX_SESSION_GRANTS {
                        return Err(SessionRuntimeError::InvalidApprovalGrant);
                    }
                    transaction
                        .execute(
                            "INSERT OR IGNORE INTO session_grants(
                                 session_id, kind, value, created_at_ms
                             ) VALUES (?1, ?2, ?3, ?4)",
                            params![session_id.to_string(), kind, value, now],
                        )
                        .map_err(|_| SessionRuntimeError::Persistence)?;
                }
                if let ApprovalDecision::ApproveForWorkspace { grant } = &decision {
                    let workspace_path: String = transaction
                        .query_row(
                            "SELECT path FROM workspaces WHERE id = ?1",
                            [workspace_id.to_string()],
                            |row| row.get(0),
                        )
                        .map_err(|_| SessionRuntimeError::Persistence)?;
                    promotion = Some(PendingGrantPromotion {
                        workspace_id,
                        workspace_path,
                        session_id,
                        run_id,
                        command_id,
                        grant: grant.clone(),
                    });
                }
                let tool_call = load_tool_call(&transaction, tool_call_id)?;
                let event = append_event(
                    &transaction,
                    EventContext {
                        store_id,
                        workspace_id,
                        session_id,
                        run_id: Some(run_id),
                        caused_by: Some(command_id),
                        occurred_at_ms: now,
                    },
                    SessionEvent::ToolApprovalResolved {
                        tool_call,
                        resolution,
                    },
                )?;
                (
                    CommandReceipt {
                        command_id,
                        committed_through: event.cursor,
                        outcome: CommandOutcome::ToolApprovalResolved {
                            tool_call_id,
                            resolution,
                        },
                    },
                    false,
                )
            }
        }
        SessionCommand::SetApprovalMode { session_id, mode } => {
            let workspace_id = session_workspace(&transaction, session_id)?;
            transaction
                .execute(
                    "UPDATE sessions SET approval_mode = ?2, updated_at_ms = ?3 WHERE id = ?1",
                    params![session_id.to_string(), approval_mode_str(mode), now],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            let sequence = workspace_sequence(&transaction, workspace_id)?;
            (
                CommandReceipt {
                    command_id,
                    committed_through: EventCursor {
                        store_id,
                        workspace_id,
                        sequence,
                    },
                    outcome: CommandOutcome::ApprovalModeSet { session_id, mode },
                },
                false,
            )
        }
        SessionCommand::SetSessionModel { session_id, model } => {
            validate_model_selection(&model)?;
            let workspace_id = session_workspace(&transaction, session_id)?;
            transaction
                .execute(
                    "UPDATE sessions
                     SET context_tokens = CASE
                             WHEN model IS ?2 THEN context_tokens ELSE NULL
                         END,
                         model = ?2, max_output_tokens = ?3, organization = ?4,
                         updated_at_ms = ?5
                     WHERE id = ?1",
                    params![
                        session_id.to_string(),
                        &model.model,
                        model.max_output_tokens,
                        &model.organization,
                        now,
                    ],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            // The new selection is read at claim time (`claim_next_run`), so
            // it applies to the next run; an executing run keeps the
            // `ClaimedRun` model it started with.
            let summary = load_session_summary(&transaction, session_id)?;
            let event = append_event(
                &transaction,
                EventContext {
                    store_id,
                    workspace_id,
                    session_id,
                    run_id: None,
                    caused_by: Some(command_id),
                    occurred_at_ms: now,
                },
                SessionEvent::SessionUpdated { session: summary },
            )?;
            (
                CommandReceipt {
                    command_id,
                    committed_through: event.cursor,
                    outcome: CommandOutcome::SessionModelSet { session_id, model },
                },
                false,
            )
        }
        SessionCommand::DeleteSession { session_id } => {
            let workspace_id = session_workspace(&transaction, session_id)?;
            let event = delete_idle_session(
                &transaction,
                store_id,
                workspace_id,
                session_id,
                command_id,
                now,
            )?;
            (
                CommandReceipt {
                    command_id,
                    committed_through: event.cursor,
                    outcome: CommandOutcome::SessionDeleted { session_id },
                },
                false,
            )
        }
        SessionCommand::PruneSessions { workspace_id } => {
            ensure_workspace(&transaction, workspace_id)?;
            // Idle sessions that never received a message: the residue left
            // by creating sessions without prompting them. Anything with a
            // run row (even a cancelled one) is history worth keeping.
            let mut statement = transaction
                .prepare(
                    "SELECT id FROM sessions
                     WHERE workspace_id = ?1 AND status = 'idle'
                       AND active_run_id IS NULL AND queued_prompts = 0
                       AND NOT EXISTS (
                           SELECT 1 FROM messages WHERE messages.session_id = sessions.id
                       )
                       AND NOT EXISTS (
                           SELECT 1 FROM runs WHERE runs.session_id = sessions.id
                       )
                     ORDER BY rowid",
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            let victims = statement
                .query_map([workspace_id.to_string()], |row| row.get::<_, String>(0))
                .map_err(|_| SessionRuntimeError::Persistence)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            drop(statement);
            let mut cursor = EventCursor {
                store_id,
                workspace_id,
                sequence: workspace_sequence(&transaction, workspace_id)?,
            };
            let mut deleted: u32 = 0;
            for victim in victims {
                let session_id: SessionId = parse_id(&victim)?;
                let event = delete_idle_session(
                    &transaction,
                    store_id,
                    workspace_id,
                    session_id,
                    command_id,
                    now,
                )?;
                cursor = event.cursor;
                deleted += 1;
            }
            (
                CommandReceipt {
                    command_id,
                    committed_through: cursor,
                    outcome: CommandOutcome::SessionsPruned {
                        workspace_id,
                        deleted,
                    },
                },
                false,
            )
        }
        SessionCommand::CompactSession { session_id } => {
            let workspace_id = session_workspace(&transaction, session_id)?;
            let (status, active_run, queued): (String, Option<String>, u16) = transaction
                .query_row(
                    "SELECT status, active_run_id, queued_prompts FROM sessions WHERE id = ?1",
                    [session_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(|_| SessionRuntimeError::Persistence)?
                .ok_or(SessionRuntimeError::SessionNotFound)?;
            // Compaction is valid only while the session is idle: a running
            // run keeps the context it started with, and a queued prompt
            // must not race the summarizer.
            if status != "idle" || active_run.is_some() || queued > 0 {
                return Err(SessionRuntimeError::SessionActive);
            }
            let run_id = RunId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            // Internal runs persist no message rows; the ids are placeholders
            // satisfying the runs schema.
            let user_message_id =
                MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            let assistant_message_id =
                MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            transaction
                .execute(
                    "INSERT INTO runs(
                        id, session_id, command_id, user_message_id, assistant_message_id,
                        status, kind, created_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, 'queued', 'compaction', ?6)",
                    params![
                        run_id.to_string(),
                        session_id.to_string(),
                        command_id.to_string(),
                        user_message_id.to_string(),
                        assistant_message_id.to_string(),
                        now,
                    ],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            // The internal run flows through the ordinary queue accounting so
            // claiming it decrements like any prompt.
            transaction
                .execute(
                    "UPDATE sessions
                     SET status = 'queued', queued_prompts = queued_prompts + 1,
                         updated_at_ms = ?2
                     WHERE id = ?1",
                    params![session_id.to_string(), now],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            let summary = load_session_summary(&transaction, session_id)?;
            let event = append_event(
                &transaction,
                EventContext {
                    store_id,
                    workspace_id,
                    session_id,
                    run_id: Some(run_id),
                    caused_by: Some(command_id),
                    occurred_at_ms: now,
                },
                SessionEvent::SessionUpdated { session: summary },
            )?;
            (
                CommandReceipt {
                    command_id,
                    committed_through: event.cursor,
                    outcome: CommandOutcome::CompactionQueued { session_id, run_id },
                },
                true,
            )
        }
    };
    let receipt_json =
        serde_json::to_string(&receipt).map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "INSERT INTO commands(id, request_json, receipt_json) VALUES (?1, ?2, ?3)",
            params![command_id.to_string(), request_json, receipt_json],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(AppliedCommand {
        receipt,
        schedule,
        cascade_cancels,
        promotion,
    })
}

fn claim_next_run(
    connection: &mut Connection,
    store_id: StoreId,
    children: bool,
) -> Result<Option<ClaimedRun>, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let row = transaction
        .query_row(
            "SELECT r.id, r.session_id, r.command_id, r.user_message_id, r.kind,
                    s.workspace_id, w.path, s.model, s.max_output_tokens, s.organization,
                    (SELECT c.request_json FROM commands c WHERE c.id = r.command_id)
             FROM runs r
             JOIN sessions s ON s.id = r.session_id
             JOIN workspaces w ON w.id = s.workspace_id
             WHERE r.status = 'queued' AND s.active_run_id IS NULL
               AND (s.parent_id IS NOT NULL) = ?1
             ORDER BY COALESCE((
                         SELECT MAX(previous.started_at_ms)
                         FROM runs previous
                         WHERE previous.session_id = r.session_id
                     ), 0),
                      r.created_at_ms, r.rowid
             LIMIT 1",
            [children],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<u32>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                ))
            },
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let Some((
        run,
        session,
        command,
        user_message,
        kind,
        workspace,
        workspace_path,
        model,
        max_tokens,
        organization,
        command_request,
    )) = row
    else {
        return Ok(None);
    };
    let run_id: RunId = parse_id(&run)?;
    let session_id: SessionId = parse_id(&session)?;
    let command_id: CommandId = parse_id(&command)?;
    let user_message_id = parse_id::<MessageId>(&user_message)?;
    let (user_initiated, literal_slash) = match command_request {
        Some(request) => {
            let request = serde_json::from_str::<SessionCommand>(&request)
                .map_err(|_| SessionRuntimeError::Persistence)?;
            let literal_slash = matches!(
                &request,
                SessionCommand::SubmitPrompt { prompt, .. }
                    if prompt.trim().starts_with("//")
            );
            (true, literal_slash)
        }
        None => (false, false),
    };
    let kind = parse_run_kind(&kind)?;
    let workspace_id: WorkspaceId = parse_id(&workspace)?;
    let now = now_ms();
    let model = ModelSelection {
        model,
        max_output_tokens: max_tokens,
        organization,
    };
    // Threshold trigger: a prompt about to run on an oversized assembly
    // compacts first; the prompt stays queued and runs right after. This is
    // evaluated only here — between runs — so a run in flight always
    // completes on the context it started with. The guard on the session's
    // most recently finished run yields exactly one automatic attempt per
    // prompt: straight after a compaction (auto or manual, whatever its
    // outcome) the prompt proceeds regardless, so a summarizer failure — or
    // a pathological summary that did not shrink the assembly under the
    // threshold — can never loop.
    let mut over_budget = false;
    if kind == RunKind::Prompt {
        let assembled = assembled_context_bytes(&transaction, session_id)?;
        // The queued prompt has not joined the assembly yet (its message row
        // is still 'queued'); measure what the claimed run would send.
        let prompt_bytes: u64 = transaction
            .query_row(
                "SELECT length(CAST(output AS BLOB)) FROM messages WHERE id = ?1",
                [user_message_id.to_string()],
                |row| row.get(0),
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        let total = assembled.saturating_add(usize::try_from(prompt_bytes).unwrap_or(usize::MAX));
        // Two compaction triggers share the one-attempt no-thrash guard: the
        // assembled byte budget, and a provider context-window overflow on
        // the session's previous run. Bytes approximate tokens loosely, so a
        // token-dense context can overflow the model window while still
        // under the byte threshold; the failure-driven trigger makes the
        // next prompt compact-then-continue instead of failing again.
        if total > AUTO_COMPACT_CONTEXT_BYTES
            || last_run_failed_with_context_overflow(&transaction, session_id)?
        {
            if !last_finished_run_was_compaction(&transaction, session_id)? {
                return claim_auto_compaction(
                    transaction,
                    store_id,
                    workspace_id,
                    workspace_path,
                    session_id,
                    model,
                    now,
                );
            }
            // The one attempt already happened; past the hard budget the run
            // fails with the context policy failure instead of reaching the
            // model.
            over_budget = total > MAX_CONTEXT_BYTES;
        }
    }
    transaction
        .execute(
            "UPDATE runs SET status = 'running', started_at_ms = ?2 WHERE id = ?1",
            params![run, now],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "UPDATE messages SET state = 'complete' WHERE id = ?1",
            [user_message],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "UPDATE sessions
             SET active_run_id = ?2, status = 'running', queued_prompts = queued_prompts - 1,
                 updated_at_ms = ?3
             WHERE id = ?1",
            params![session, run, now],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let messages = match kind {
        RunKind::Prompt => {
            let user_ordinal: u64 = transaction
                .query_row(
                    "SELECT ordinal FROM messages WHERE id = ?1",
                    [user_message_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            load_model_context(&transaction, session_id, user_ordinal)?
        }
        RunKind::Compaction => {
            // The summarization request is the session's assembled context —
            // latest summary plus verbatim span, with result pruning — and
            // the fixed instruction as the final user message. A prior
            // summary therefore folds into the next one naturally.
            let mut context = load_model_context(&transaction, session_id, u64::MAX)?;
            context.push(Message::user(compaction_instruction(
                &transaction,
                session_id,
            )?));
            context
        }
    };
    let summary = load_session_summary(&transaction, session_id)?;
    let started = append_event(
        &transaction,
        EventContext {
            store_id,
            workspace_id,
            session_id,
            run_id: Some(run_id),
            caused_by: None,
            occurred_at_ms: now,
        },
        SessionEvent::RunStarted {
            session: summary,
            run_id,
        },
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(Some(ClaimedRun {
        workspace_id,
        workspace: workspace_path,
        session_id,
        run_id,
        command_id,
        kind,
        child: children,
        user_initiated,
        literal_slash,
        model,
        messages,
        started,
        over_budget,
    }))
}

/// True when the session's most recently finished run — any outcome — was a
/// compaction. The claim path consults this as its no-thrash guard: crossing
/// the threshold triggers at most one automatic compaction per prompt, and a
/// fresh trigger requires the context to grow past the threshold again after
/// some other run.
fn last_finished_run_was_compaction(
    transaction: &Transaction<'_>,
    session_id: SessionId,
) -> Result<bool, SessionRuntimeError> {
    let kind: Option<String> = transaction
        .query_row(
            "SELECT kind FROM runs
             WHERE session_id = ?1 AND outcome_json IS NOT NULL
             ORDER BY finished_at_ms DESC, rowid DESC LIMIT 1",
            [session_id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(kind.as_deref() == Some("compaction"))
}

/// True when the session's most recently finished prompt run failed because
/// the provider rejected the request as exceeding the model context window.
/// The claim path uses this as a compaction trigger so the next prompt
/// recovers instead of hitting the same wall.
fn last_run_failed_with_context_overflow(
    transaction: &Transaction<'_>,
    session_id: SessionId,
) -> Result<bool, SessionRuntimeError> {
    let outcome_json: Option<String> = transaction
        .query_row(
            "SELECT outcome_json FROM runs
             WHERE session_id = ?1 AND outcome_json IS NOT NULL
             ORDER BY finished_at_ms DESC, rowid DESC LIMIT 1",
            [session_id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let Some(outcome_json) = outcome_json else {
        return Ok(false);
    };
    let Ok(outcome) = serde_json::from_str::<RunOutcome>(&outcome_json) else {
        // An unreadable historical outcome must not block claiming.
        return Ok(false);
    };
    Ok(matches!(
        outcome,
        RunOutcome::Failed {
            failure: RunFailure {
                kind: RunFailureKind::ProviderContextExceeded,
                ..
            }
        }
    ))
}

/// Claims an automatic compaction run for `session_id` in place of the
/// queued prompt that crossed the context threshold. The prompt run is left
/// untouched — still queued, still counted, its user message still pending —
/// so it is the session's next claim once the compaction settles. The
/// compaction run is ordinary in every other way: same kind, events, usage
/// and cost accounting, and internal-run transcript exclusion as a manual
/// `CompactSession`; `auto_compaction = 1` marks its provenance in the run
/// row.
fn claim_auto_compaction(
    transaction: Transaction<'_>,
    store_id: StoreId,
    workspace_id: WorkspaceId,
    workspace_path: String,
    session_id: SessionId,
    model: ModelSelection,
    now: u64,
) -> Result<Option<ClaimedRun>, SessionRuntimeError> {
    let run_id = RunId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    // No client command requested this run; a generated id satisfies the
    // unique command column without joining the commands table.
    let command_id = CommandId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    // Internal runs persist no message rows; the ids are placeholders
    // satisfying the runs schema.
    let user_message_id = MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    let assistant_message_id =
        MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    transaction
        .execute(
            "INSERT INTO runs(
                id, session_id, command_id, user_message_id, assistant_message_id,
                status, kind, auto_compaction, created_at_ms, started_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'running', 'compaction', 1, ?6, ?6)",
            params![
                run_id.to_string(),
                session_id.to_string(),
                command_id.to_string(),
                user_message_id.to_string(),
                assistant_message_id.to_string(),
                now,
            ],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    // `queued_prompts` keeps counting the waiting prompt; it runs next.
    transaction
        .execute(
            "UPDATE sessions
             SET active_run_id = ?2, status = 'running', updated_at_ms = ?3
             WHERE id = ?1",
            params![session_id.to_string(), run_id.to_string(), now],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    // The summarization request assembles exactly like a manual compaction:
    // the queued prompt's message is still pending and therefore excluded.
    let mut context = load_model_context(&transaction, session_id, u64::MAX)?;
    context.push(Message::user(compaction_instruction(
        &transaction,
        session_id,
    )?));
    let summary = load_session_summary(&transaction, session_id)?;
    let started = append_event(
        &transaction,
        EventContext {
            store_id,
            workspace_id,
            session_id,
            run_id: Some(run_id),
            caused_by: None,
            occurred_at_ms: now,
        },
        SessionEvent::RunStarted {
            session: summary,
            run_id,
        },
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(Some(ClaimedRun {
        workspace_id,
        workspace: workspace_path,
        session_id,
        run_id,
        command_id,
        kind: RunKind::Compaction,
        // Compaction runs are internal: execute_run never installs the
        // subagent spawner for them, so child-ness is irrelevant here.
        child: false,
        user_initiated: false,
        literal_slash: false,
        model,
        messages: context,
        started,
        over_budget: false,
    }))
}

/// Creates the assistant message for one model turn and appends its first
/// text chunk in a single transaction. The message row is created lazily at
/// the turn's first delta — never at turn start — so call-only turns persist
/// no message row at all. The run row's `assistant_message_id` is repointed
/// here: crash recovery interrupts only the still-streaming current message.
fn begin_assistant_message(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    message_id: MessageId,
    turn_ordinal: u16,
    channel: TextChannel,
    text: &str,
) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
    if text.is_empty() {
        return Err(SessionRuntimeError::Persistence);
    }
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    ensure_context_capacity(&transaction, claimed.session_id, text.len())?;
    let now = now_ms();
    let ordinal: u64 = transaction
        .query_row(
            "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM messages WHERE session_id = ?1",
            [claimed.session_id.to_string()],
            |row| row.get(0),
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "INSERT INTO messages(
                id, session_id, run_id, ordinal, turn_ordinal, role, state, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'assistant', 'streaming', ?6)",
            params![
                message_id.to_string(),
                claimed.session_id.to_string(),
                claimed.run_id.to_string(),
                ordinal,
                turn_ordinal,
                now,
            ],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "UPDATE runs SET assistant_message_id = ?2 WHERE id = ?1",
            params![claimed.run_id.to_string(), message_id.to_string()],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let message = load_message(&transaction, message_id)?;
    let started = append_event(
        &transaction,
        EventContext {
            store_id,
            workspace_id: claimed.workspace_id,
            session_id: claimed.session_id,
            run_id: Some(claimed.run_id),
            caused_by: Some(claimed.command_id),
            occurred_at_ms: now,
        },
        SessionEvent::AssistantMessageStarted { message },
    )?;
    let column = match channel {
        TextChannel::Output => "output",
        TextChannel::Refusal => "refusal",
    };
    let sql = format!("UPDATE messages SET {column} = ?2 WHERE id = ?1");
    transaction
        .execute(&sql, params![message_id.to_string(), text])
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let appended = append_event(
        &transaction,
        EventContext {
            store_id,
            workspace_id: claimed.workspace_id,
            session_id: claimed.session_id,
            run_id: Some(claimed.run_id),
            caused_by: Some(claimed.command_id),
            occurred_at_ms: now,
        },
        SessionEvent::TextAppended {
            message_id,
            channel,
            text: text.to_owned(),
        },
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(vec![started, appended])
}

fn append_text(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    message_id: MessageId,
    channel: TextChannel,
    text: String,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    if text.is_empty() {
        return Err(SessionRuntimeError::Persistence);
    }
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    ensure_context_capacity(&transaction, claimed.session_id, text.len())?;
    let column = match channel {
        TextChannel::Output => "output",
        TextChannel::Refusal => "refusal",
    };
    let sql = format!(
        "UPDATE messages SET {column} = {column} || ?2 WHERE id = ?1 AND state = 'streaming'"
    );
    let updated = transaction
        .execute(&sql, params![message_id.to_string(), text])
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if updated != 1 {
        return Err(SessionRuntimeError::Unavailable);
    }
    let event = append_event(
        &transaction,
        EventContext {
            store_id,
            workspace_id: claimed.workspace_id,
            session_id: claimed.session_id,
            run_id: Some(claimed.run_id),
            caused_by: Some(claimed.command_id),
            occurred_at_ms: now_ms(),
        },
        SessionEvent::TextAppended {
            message_id,
            channel,
            text,
        },
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(event)
}

fn persist_model_turn(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    turn: &ModelTurnCommit,
) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
    let ModelTurnCommit {
        turn_ordinal,
        message,
        calls,
        turn_message,
        context_tokens,
        usage,
        estimated_cost_usd_nanos,
        accounting,
    } = turn;
    if message.role() != Role::Assistant {
        return Err(SessionRuntimeError::Persistence);
    }
    let content = message
        .content()
        .iter()
        .map(PersistedContentBlock::from)
        .collect::<Vec<_>>();
    let content_json =
        serde_json::to_string(&content).map_err(|_| SessionRuntimeError::Persistence)?;
    let model_json =
        serde_json::to_string(&claimed.model).map_err(|_| SessionRuntimeError::Persistence)?;
    let usage_json = usage
        .map(|usage| serde_json::to_string(&usage))
        .transpose()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let turn_cost = estimated_cost_usd_nanos
        .map(i64::try_from)
        .transpose()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let now = now_ms();
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let persisted_calls = if claimed.kind == RunKind::Prompt {
        calls.as_slice()
    } else {
        &[]
    };
    let argument_bytes = persisted_calls.iter().fold(0_usize, |total, call| {
        total.saturating_add(call.arguments.len())
    });
    ensure_context_capacity(&transaction, claimed.session_id, argument_bytes)?;
    // Completing the turn's message in the same transaction as the turn row
    // keeps message state and turn persistence atomic: after a crash, a
    // streaming message always identifies exactly the turn that never
    // committed, and recovery interrupts only that message.
    if let Some(message_id) = turn_message {
        let updated = transaction
            .execute(
                "UPDATE messages SET state = 'complete'
                 WHERE id = ?1 AND run_id = ?2 AND state = 'streaming'",
                params![message_id.to_string(), claimed.run_id.to_string()],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        if updated != 1 {
            return Err(SessionRuntimeError::Unavailable);
        }
    }
    transaction
        .execute(
            "INSERT INTO model_turns(
                 run_id, turn_ordinal, assistant_content_json, model_json,
                 usage_json, estimated_cost_usd_nanos, completed_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                claimed.run_id.to_string(),
                turn_ordinal,
                content_json,
                model_json,
                usage_json,
                turn_cost,
                now,
            ],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let mut events = Vec::with_capacity(persisted_calls.len().saturating_add(3));
    events.push(append_event(
        &transaction,
        EventContext {
            store_id,
            workspace_id: claimed.workspace_id,
            session_id: claimed.session_id,
            run_id: Some(claimed.run_id),
            caused_by: Some(claimed.command_id),
            occurred_at_ms: now,
        },
        SessionEvent::ModelTurnCompleted {
            run_id: claimed.run_id,
            turn_ordinal: *turn_ordinal,
            model: claimed.model.clone(),
            usage: *usage,
            estimated_cost_usd_nanos: *estimated_cost_usd_nanos,
        },
    )?);
    for call in persisted_calls {
        transaction
            .execute(
                "INSERT INTO tool_calls(
                     id, run_id, turn_ordinal, call_ordinal, provider_call_id, name,
                     arguments_json, state, requested_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'requested', ?8)",
                params![
                    call.id.to_string(),
                    claimed.run_id.to_string(),
                    call.turn_ordinal,
                    call.call_ordinal,
                    call.provider_call_id,
                    call.name,
                    call.arguments,
                    now,
                ],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        let tool_call = load_tool_call(&transaction, call.id)?;
        events.push(append_event(
            &transaction,
            EventContext {
                store_id,
                workspace_id: claimed.workspace_id,
                session_id: claimed.session_id,
                run_id: Some(claimed.run_id),
                caused_by: Some(claimed.command_id),
                occurred_at_ms: now,
            },
            SessionEvent::ToolCallRequested { tool_call },
        )?);
    }
    let usage_json = accounting
        .as_ref()
        .and_then(|accounting| accounting.usage)
        .map(|usage| serde_json::to_string(&usage))
        .transpose()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let estimated_cost_usd_nanos = accounting
        .as_ref()
        .and_then(|accounting| accounting.estimated_cost_usd_nanos)
        .and_then(|cost| i64::try_from(cost).ok());
    // Persist-before-publish like every other event. A missing provider value
    // clears the previous turn's audit value instead of leaving stale data.
    transaction
        .execute(
            "UPDATE runs
             SET context_tokens = ?2, usage_json = ?3, estimated_cost_usd_nanos = ?4
             WHERE id = ?1",
            params![
                claimed.run_id.to_string(),
                context_tokens,
                usage_json,
                estimated_cost_usd_nanos,
            ],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if let Some(context_tokens) = context_tokens {
        events.push(append_event(
            &transaction,
            EventContext {
                store_id,
                workspace_id: claimed.workspace_id,
                session_id: claimed.session_id,
                run_id: Some(claimed.run_id),
                caused_by: Some(claimed.command_id),
                occurred_at_ms: now,
            },
            SessionEvent::RunContextUpdated {
                run_id: claimed.run_id,
                context_tokens: *context_tokens,
            },
        )?);
    }
    let session_context_updated = if claimed.kind == RunKind::Prompt {
        transaction
            .execute(
                "UPDATE sessions SET context_tokens = ?2
                 WHERE id = ?1 AND model IS ?3",
                params![
                    claimed.session_id.to_string(),
                    context_tokens,
                    &claimed.model.model,
                ],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?
            == 1
    } else {
        false
    };
    if session_context_updated {
        events.push(append_event(
            &transaction,
            EventContext {
                store_id,
                workspace_id: claimed.workspace_id,
                session_id: claimed.session_id,
                run_id: Some(claimed.run_id),
                caused_by: Some(claimed.command_id),
                occurred_at_ms: now,
            },
            SessionEvent::SessionContextUpdated {
                run_id: claimed.run_id,
                context_tokens: *context_tokens,
            },
        )?);
    }
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(events)
}

fn start_tool_call(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    tool_call_id: ToolCallId,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let now = now_ms();
    let updated = transaction
        .execute(
            "UPDATE tool_calls SET state = 'running', started_at_ms = ?2
             WHERE id = ?1 AND run_id = ?3 AND state = 'requested'",
            params![tool_call_id.to_string(), now, claimed.run_id.to_string()],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if updated != 1 {
        return Err(SessionRuntimeError::Unavailable);
    }
    let tool_call = load_tool_call(&transaction, tool_call_id)?;
    let event = append_event(
        &transaction,
        EventContext {
            store_id,
            workspace_id: claimed.workspace_id,
            session_id: claimed.session_id,
            run_id: Some(claimed.run_id),
            caused_by: Some(claimed.command_id),
            occurred_at_ms: now,
        },
        SessionEvent::ToolCallStarted { tool_call },
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(event)
}

/// Appends replaceable liveness information for an active run. Activity is
/// retained in the event log for reconnect/replay, but does not alter model
/// context or transcript rows.
fn append_run_activity(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    activity: RunActivity,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let running = transaction
        .query_row(
            "SELECT 1 FROM runs WHERE id = ?1 AND session_id = ?2 AND status = 'running'",
            params![claimed.run_id.to_string(), claimed.session_id.to_string()],
            |_| Ok(()),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if running.is_none() {
        return Err(SessionRuntimeError::Unavailable);
    }
    let event = append_event(
        &transaction,
        EventContext {
            store_id,
            workspace_id: claimed.workspace_id,
            session_id: claimed.session_id,
            run_id: Some(claimed.run_id),
            caused_by: Some(claimed.command_id),
            occurred_at_ms: now_ms(),
        },
        SessionEvent::RunActivityChanged {
            run_id: claimed.run_id,
            activity,
        },
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(event)
}

fn append_reasoning(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    reasoning: ReasoningEvent,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let running = transaction
        .query_row(
            "SELECT 1 FROM runs WHERE id = ?1 AND session_id = ?2 AND status = 'running'",
            params![claimed.run_id.to_string(), claimed.session_id.to_string()],
            |_| Ok(()),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if running.is_none() {
        return Err(SessionRuntimeError::Unavailable);
    }
    let event = match reasoning {
        ReasoningEvent::Started { kind } => SessionEvent::ReasoningStarted {
            run_id: claimed.run_id,
            kind,
        },
        ReasoningEvent::Delta { kind, text } => SessionEvent::ReasoningDelta {
            run_id: claimed.run_id,
            kind,
            text,
        },
        ReasoningEvent::Completed { kind } => SessionEvent::ReasoningCompleted {
            run_id: claimed.run_id,
            kind,
        },
    };
    let event = append_event(
        &transaction,
        EventContext {
            store_id,
            workspace_id: claimed.workspace_id,
            session_id: claimed.session_id,
            run_id: Some(claimed.run_id),
            caused_by: Some(claimed.command_id),
            occurred_at_ms: now_ms(),
        },
        event,
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(event)
}

/// Appends a `ToolCallOutputDelta` event for a running call. The chunk lives
/// only in the event log (batched like text deltas so long builds render
/// live); the call's bounded result remains the single durable output.
fn append_tool_call_output(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    tool_call_id: ToolCallId,
    chunk: String,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let running = transaction
        .query_row(
            "SELECT 1 FROM tool_calls WHERE id = ?1 AND run_id = ?2 AND state = 'running'",
            params![tool_call_id.to_string(), claimed.run_id.to_string()],
            |_| Ok(()),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if running.is_none() {
        return Err(SessionRuntimeError::ToolCallNotFound);
    }
    let event = append_event(
        &transaction,
        EventContext {
            store_id,
            workspace_id: claimed.workspace_id,
            session_id: claimed.session_id,
            run_id: Some(claimed.run_id),
            caused_by: Some(claimed.command_id),
            occurred_at_ms: now_ms(),
        },
        SessionEvent::ToolCallOutputDelta {
            tool_call_id,
            chunk,
        },
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(event)
}

#[expect(
    clippy::too_many_arguments,
    reason = "one persisted row update; bundling the columns adds nothing"
)]
fn finish_tool_call(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    tool_call_id: ToolCallId,
    result: String,
    is_error: bool,
    file_state: Option<FileStateUpdate>,
    display: Option<ToolCallDisplay>,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    // The display payload is deliberately absent from the capacity check: it
    // never enters model context, so it cannot crowd the context budget.
    ensure_context_capacity(&transaction, claimed.session_id, result.len())?;
    let now = now_ms();
    let state = if is_error { "failed" } else { "completed" };
    let display_json = display
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let updated = transaction
        .execute(
            "UPDATE tool_calls
             SET state = ?2, result = ?3, is_error = ?4, finished_at_ms = ?5, display_json = ?7
             WHERE id = ?1 AND run_id = ?6 AND state = 'running'",
            params![
                tool_call_id.to_string(),
                state,
                result,
                is_error,
                now,
                claimed.run_id.to_string(),
                display_json,
            ],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if updated != 1 {
        return Err(SessionRuntimeError::Unavailable);
    }
    if let Some(update) = file_state {
        record_session_file(&transaction, claimed.session_id, &update, now)?;
    }
    let tool_call = load_tool_call(&transaction, tool_call_id)?;
    let event = append_event(
        &transaction,
        EventContext {
            store_id,
            workspace_id: claimed.workspace_id,
            session_id: claimed.session_id,
            run_id: Some(claimed.run_id),
            caused_by: Some(claimed.command_id),
            occurred_at_ms: now,
        },
        SessionEvent::ToolCallFinished { tool_call },
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(event)
}

/// Upserts one file-state entry, evicting the least-recently recorded paths
/// when the per-session bound is exceeded; an evicted file simply needs a
/// re-read before its next edit.
fn record_session_file(
    transaction: &Transaction<'_>,
    session_id: SessionId,
    update: &FileStateUpdate,
    now: u64,
) -> Result<(), SessionRuntimeError> {
    transaction
        .execute(
            "INSERT INTO session_files(session_id, path, content_hash, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session_id, path) DO UPDATE
             SET content_hash = excluded.content_hash,
                 updated_at_ms = excluded.updated_at_ms",
            params![session_id.to_string(), update.path, update.hash, now],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "DELETE FROM session_files
             WHERE session_id = ?1 AND rowid NOT IN (
                 SELECT rowid FROM session_files WHERE session_id = ?1
                 ORDER BY updated_at_ms DESC, rowid DESC LIMIT ?2
             )",
            params![session_id.to_string(), MAX_SESSION_FILES],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(())
}

/// Copies the workspace's effective config grants into the new session's
/// grant set, inside the CreateSession transaction. From here on the gate
/// consults only `session_grants`, so config-seeded and approve-for-session
/// grants are indistinguishable. Malformed or excess entries are skipped
/// rather than failing creation: the config layer already validated
/// well-formed grants, and a clamped seed only means more prompting.
fn insert_seed_grants(
    transaction: &Transaction<'_>,
    session_id: SessionId,
    seed: &WorkspaceGrantSeed,
    now: u64,
) -> Result<(), SessionRuntimeError> {
    let tools = seed.tools.iter().map(|value| ("tool", value));
    let prefixes = seed
        .shell_prefixes
        .iter()
        .map(|value| ("shell_prefix", value));
    let mut remaining = MAX_SESSION_GRANTS;
    for (kind, value) in tools.chain(prefixes) {
        let value = value.trim();
        if value.is_empty() || value.len() > MAX_GRANT_BYTES {
            continue;
        }
        if remaining == 0 {
            break;
        }
        remaining -= 1;
        transaction
            .execute(
                "INSERT OR IGNORE INTO session_grants(
                     session_id, kind, value, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![session_id.to_string(), kind, value, now],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    Ok(())
}

fn load_approval_policy(
    connection: &mut Connection,
    session_id: SessionId,
) -> Result<(ApprovalMode, approval::SessionGrants), SessionRuntimeError> {
    let mode = connection
        .query_row(
            "SELECT approval_mode FROM sessions WHERE id = ?1",
            [session_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?
        .ok_or(SessionRuntimeError::SessionNotFound)?;
    let mode = parse_approval_mode(&mode)?;
    let mut statement = connection
        .prepare("SELECT kind, value FROM session_grants WHERE session_id = ?1")
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let rows = statement
        .query_map([session_id.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let mut grants = approval::SessionGrants::default();
    for (kind, value) in rows {
        match kind.as_str() {
            "tool" => {
                grants.tools.insert(value);
            }
            "shell_prefix" => grants.shell_prefixes.push(value),
            _ => return Err(SessionRuntimeError::Persistence),
        }
    }
    Ok((mode, grants))
}

fn deny_tool_call(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    tool_call_id: ToolCallId,
    message: &str,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let now = now_ms();
    let updated = transaction
        .execute(
            "UPDATE tool_calls
             SET state = 'denied', result = ?2, is_error = 1, finished_at_ms = ?3
             WHERE id = ?1 AND run_id = ?4 AND state = 'requested'",
            params![
                tool_call_id.to_string(),
                message,
                now,
                claimed.run_id.to_string(),
            ],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if updated != 1 {
        return Err(SessionRuntimeError::Unavailable);
    }
    let tool_call = load_tool_call(&transaction, tool_call_id)?;
    let event = append_event(
        &transaction,
        EventContext {
            store_id,
            workspace_id: claimed.workspace_id,
            session_id: claimed.session_id,
            run_id: Some(claimed.run_id),
            caused_by: Some(claimed.command_id),
            occurred_at_ms: now,
        },
        SessionEvent::ToolCallFinished { tool_call },
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(event)
}

fn request_tool_approval(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    tool_call_id: ToolCallId,
    shell: Option<ShellCommandPreview>,
    edit: Option<EditPreview>,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let now = now_ms();
    let updated = transaction
        .execute(
            "UPDATE tool_calls SET state = 'awaiting_approval'
             WHERE id = ?1 AND run_id = ?2 AND state = 'requested'",
            params![tool_call_id.to_string(), claimed.run_id.to_string()],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if updated != 1 {
        return Err(SessionRuntimeError::Unavailable);
    }
    let tool_call = load_tool_call(&transaction, tool_call_id)?;
    let event = append_event(
        &transaction,
        EventContext {
            store_id,
            workspace_id: claimed.workspace_id,
            session_id: claimed.session_id,
            run_id: Some(claimed.run_id),
            caused_by: Some(claimed.command_id),
            occurred_at_ms: now,
        },
        SessionEvent::ToolApprovalRequested {
            tool_call,
            shell,
            edit,
        },
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(event)
}

/// Resolves one awaiting approval as reviewer-approved, unless a client
/// resolution already committed — the client always wins the race. Returns
/// the resolution event to publish when the reviewer's approval landed, and
/// `None` when the call was no longer awaiting (already resolved, or the run
/// finished and interrupted it).
fn resolve_approval_by_reviewer(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    tool_call_id: ToolCallId,
) -> Result<Option<SessionEventEnvelope>, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let now = now_ms();
    let updated = transaction
        .execute(
            "UPDATE tool_calls
             SET state = 'requested', approval_resolution = ?2, resolved_at_ms = ?3
             WHERE id = ?1 AND run_id = ?4 AND state = 'awaiting_approval'
               AND approval_resolution IS NULL",
            params![
                tool_call_id.to_string(),
                approval_resolution_str(ApprovalResolution::ApprovedByReviewer),
                now,
                claimed.run_id.to_string(),
            ],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if updated != 1 {
        return Ok(None);
    }
    let tool_call = load_tool_call(&transaction, tool_call_id)?;
    let event = append_event(
        &transaction,
        EventContext {
            store_id,
            workspace_id: claimed.workspace_id,
            session_id: claimed.session_id,
            run_id: Some(claimed.run_id),
            caused_by: Some(claimed.command_id),
            occurred_at_ms: now,
        },
        SessionEvent::ToolApprovalResolved {
            tool_call,
            resolution: ApprovalResolution::ApprovedByReviewer,
        },
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(Some(event))
}

fn conclude_tool_approval(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    tool_call_id: ToolCallId,
    timed_out: bool,
) -> Result<ConcludedApproval, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let (state, resolution, result) = transaction
        .query_row(
            "SELECT state, approval_resolution, result FROM tool_calls
             WHERE id = ?1 AND run_id = ?2",
            params![tool_call_id.to_string(), claimed.run_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?
        .ok_or(SessionRuntimeError::ToolCallNotFound)?;
    if let Some(resolution) = resolution {
        // A client resolution won the race; its transaction already
        // persisted the state change and published the event.
        return match parse_approval_resolution(&resolution)? {
            ApprovalResolution::ApprovedOnce
            | ApprovalResolution::ApprovedForSession
            | ApprovalResolution::ApprovedForWorkspace
            | ApprovalResolution::ApprovedByReviewer => Ok(ConcludedApproval::Approved),
            ApprovalResolution::Denied
            | ApprovalResolution::DeniedTimeout
            | ApprovalResolution::DeniedByReviewer => Ok(ConcludedApproval::Denied {
                message: result.unwrap_or_else(|| approval::USER_DENIED_RESULT.to_owned()),
                event: None,
            }),
        };
    }
    if !timed_out || state != "awaiting_approval" {
        return Ok(ConcludedApproval::StillWaiting);
    }
    let now = now_ms();
    transaction
        .execute(
            "UPDATE tool_calls
             SET state = 'denied', result = ?2, is_error = 1,
                 approval_resolution = 'denied_timeout', resolved_at_ms = ?3,
                 finished_at_ms = ?3
             WHERE id = ?1 AND state = 'awaiting_approval'",
            params![
                tool_call_id.to_string(),
                approval::TIMEOUT_DENIED_RESULT,
                now,
            ],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let tool_call = load_tool_call(&transaction, tool_call_id)?;
    let event = append_event(
        &transaction,
        EventContext {
            store_id,
            workspace_id: claimed.workspace_id,
            session_id: claimed.session_id,
            run_id: Some(claimed.run_id),
            caused_by: None,
            occurred_at_ms: now,
        },
        SessionEvent::ToolApprovalResolved {
            tool_call,
            resolution: ApprovalResolution::DeniedTimeout,
        },
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(ConcludedApproval::Denied {
        message: approval::TIMEOUT_DENIED_RESULT.to_owned(),
        event: Some(Box::new(event)),
    })
}

fn record_grant_promotion(
    connection: &mut Connection,
    store_id: StoreId,
    promotion: &PendingGrantPromotion,
    outcome: WorkspaceGrantOutcome,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let outcome = match outcome {
        WorkspaceGrantOutcome::Failed { message } => WorkspaceGrantOutcome::Failed {
            message: truncate_utf8(message, MAX_FAILURE_MESSAGE_BYTES),
        },
        outcome => outcome,
    };
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    // Only the workspace's event log is touched: the promotion outcome stays
    // publishable even when the session was deleted in the meantime.
    let event = append_event(
        &transaction,
        EventContext {
            store_id,
            workspace_id: promotion.workspace_id,
            session_id: promotion.session_id,
            run_id: Some(promotion.run_id),
            caused_by: Some(promotion.command_id),
            occurred_at_ms: now_ms(),
        },
        SessionEvent::WorkspaceGrantPromoted {
            grant: promotion.grant.clone(),
            outcome,
        },
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(event)
}

fn ensure_context_capacity(
    transaction: &Transaction<'_>,
    session_id: SessionId,
    additional: usize,
) -> Result<(), SessionRuntimeError> {
    // The budget measures the assembled context — after the compaction
    // cutoff and with stale read-only results pruned — because that is what
    // the next request actually carries; raw persisted rows may be far
    // larger without ever reaching the provider.
    let assembled = assembled_context_bytes(transaction, session_id)?;
    if assembled.saturating_add(additional) > MAX_CONTEXT_BYTES {
        return Err(SessionRuntimeError::OutputTooLarge);
    }
    Ok(())
}

fn append_parent_session_update(
    transaction: &Transaction<'_>,
    store_id: StoreId,
    workspace_id: WorkspaceId,
    child_session_id: SessionId,
    caused_by: CommandId,
    occurred_at_ms: u64,
    events: &mut Vec<SessionEventEnvelope>,
) -> Result<(), SessionRuntimeError> {
    let Some(parent_id) = session_parent(transaction, child_session_id)? else {
        return Ok(());
    };
    let session = load_session_summary(transaction, parent_id)?;
    events.push(append_event(
        transaction,
        EventContext {
            store_id,
            workspace_id,
            session_id: parent_id,
            run_id: None,
            caused_by: Some(caused_by),
            occurred_at_ms,
        },
        SessionEvent::SessionUpdated { session },
    )?);
    Ok(())
}

fn complete_run(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    outcome: RunOutcome,
    accounting: Option<RunAccounting>,
) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let mut events = vec![finalize_run(
        &transaction,
        store_id,
        claimed,
        outcome,
        accounting,
    )?];
    append_parent_session_update(
        &transaction,
        store_id,
        claimed.workspace_id,
        claimed.session_id,
        claimed.command_id,
        now_ms(),
        &mut events,
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(events)
}

/// Commits a compaction: the summary row and cutoff marker persist in the
/// same transaction that settles the internal run, so a crash anywhere
/// before the commit leaves no marker and the command can simply be retried.
/// Events (RunFinished, then SessionCompacted) are appended before commit —
/// persist-before-publish like every other event.
fn complete_compaction(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    summary: String,
    accounting: Option<RunAccounting>,
) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    // A cancel that raced the summarizer's completion wins: the run settles
    // cancelled and no marker is committed.
    let outcome = cancellation_wins(&transaction, claimed.run_id, RunOutcome::Completed)?;
    let mut events = Vec::with_capacity(2);
    if matches!(outcome, RunOutcome::Completed) {
        let now = now_ms();
        let before_bytes = assembled_context_bytes(&transaction, claimed.session_id)?;
        // The cutoff covers exactly the span the summary replaced: the
        // messages assembly showed the summarizer. A prompt still queued
        // behind an auto-compaction has an ordinal but was not summarized —
        // it must stay after the marker so its run still sends it.
        let cutoff_ordinal: u64 = transaction
            .query_row(
                "SELECT COALESCE(MAX(ordinal), 0) FROM messages
                 WHERE session_id = ?1 AND state IN ('complete', 'interrupted')",
                [claimed.session_id.to_string()],
                |row| row.get(0),
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        transaction
            .execute(
                "INSERT INTO session_compactions(
                     session_id, run_id, summary, cutoff_ordinal,
                     before_bytes, after_bytes, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)",
                params![
                    claimed.session_id.to_string(),
                    claimed.run_id.to_string(),
                    summary,
                    cutoff_ordinal,
                    u64::try_from(before_bytes).unwrap_or(u64::MAX),
                    now,
                ],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        // With the marker in place, assembly is the summary alone.
        let after_bytes = assembled_context_bytes(&transaction, claimed.session_id)?;
        transaction
            .execute(
                "UPDATE session_compactions SET after_bytes = ?3
                 WHERE session_id = ?1 AND run_id = ?2",
                params![
                    claimed.session_id.to_string(),
                    claimed.run_id.to_string(),
                    u64::try_from(after_bytes).unwrap_or(u64::MAX),
                ],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        // Bounded history, newest rows kept; no eager deletion beyond it so
        // a future rollback command can restore the previous compaction.
        transaction
            .execute(
                "DELETE FROM session_compactions
                 WHERE session_id = ?1 AND rowid NOT IN (
                     SELECT rowid FROM session_compactions WHERE session_id = ?1
                     ORDER BY rowid DESC LIMIT ?2
                 )",
                params![claimed.session_id.to_string(), COMPACTION_HISTORY_ROWS],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        // The compaction request measured the context that was just
        // replaced, not the summary now occupying the session. Keep the
        // session unknown until its next prompt turn reports exact usage.
        transaction
            .execute(
                "UPDATE sessions SET context_tokens = NULL WHERE id = ?1",
                [claimed.session_id.to_string()],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        events.push(finalize_run(
            &transaction,
            store_id,
            claimed,
            outcome,
            accounting,
        )?);
        let session = load_session_summary(&transaction, claimed.session_id)?;
        events.push(append_event(
            &transaction,
            EventContext {
                store_id,
                workspace_id: claimed.workspace_id,
                session_id: claimed.session_id,
                run_id: Some(claimed.run_id),
                caused_by: Some(claimed.command_id),
                occurred_at_ms: now,
            },
            SessionEvent::SessionCompacted {
                session,
                summary: Some(truncate_utf8(summary, MAX_EVENT_SUMMARY_BYTES)),
                before_bytes: u64::try_from(before_bytes).unwrap_or(u64::MAX),
                after_bytes: u64::try_from(after_bytes).unwrap_or(u64::MAX),
            },
        )?);
    } else {
        events.push(finalize_run(
            &transaction,
            store_id,
            claimed,
            outcome,
            accounting,
        )?);
    }
    append_parent_session_update(
        &transaction,
        store_id,
        claimed.workspace_id,
        claimed.session_id,
        claimed.command_id,
        now_ms(),
        &mut events,
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(events)
}

/// Settles a claimed run inside an open transaction: outcome, usage and cost
/// accounting, message states, session status, and the `RunFinished` event.
fn finalize_run(
    transaction: &Transaction<'_>,
    store_id: StoreId,
    claimed: &ClaimedRun,
    outcome: RunOutcome,
    accounting: Option<RunAccounting>,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let now = now_ms();
    let outcome = cancellation_wins(transaction, claimed.run_id, outcome)?;
    interrupt_active_tool_calls(
        transaction,
        store_id,
        claimed,
        &outcome,
        Some(claimed.command_id),
        now,
    )?;
    let (run_status, message_state) = outcome_states(&outcome);
    let outcome_json =
        serde_json::to_string(&outcome).map_err(|_| SessionRuntimeError::Persistence)?;
    let usage = accounting.as_ref().and_then(|accounting| accounting.usage);
    let usage_json = usage
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let cost = accounting
        .as_ref()
        .and_then(|accounting| accounting.estimated_cost_usd_nanos)
        .and_then(|cost| i64::try_from(cost).ok());
    let reported_context_tokens = accounting
        .as_ref()
        .and_then(|accounting| accounting.context_tokens);
    let saw_turn = accounting
        .as_ref()
        .is_some_and(|accounting| accounting.saw_turn);
    let (current_cost, current_cost_known) = transaction
        .query_row(
            "SELECT estimated_cost_usd_nanos, cost_known FROM sessions WHERE id = ?1",
            [claimed.session_id.to_string()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?)),
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let (next_cost, next_cost_known) = if saw_turn {
        match cost.and_then(|cost| current_cost.checked_add(cost)) {
            Some(cost) if current_cost_known => (cost, true),
            _ => (current_cost, false),
        }
    } else {
        (current_cost, current_cost_known)
    };
    // Terminal accounting owns the final per-turn figure only when a model
    // turn completed. No completed turn preserves an earlier committed value;
    // an unmeasured completed turn explicitly clears it.
    transaction
        .execute(
            "UPDATE runs
             SET status = ?2, outcome_json = ?3, finished_at_ms = ?4,
                 usage_json = ?5, estimated_cost_usd_nanos = ?6,
                 context_tokens = CASE WHEN ?8 THEN ?7 ELSE context_tokens END
             WHERE id = ?1 AND outcome_json IS NULL",
            params![
                claimed.run_id.to_string(),
                run_status,
                outcome_json,
                now,
                usage_json,
                cost,
                reported_context_tokens,
                saw_turn,
            ],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let context_tokens = run_context_tokens(transaction, claimed.run_id)?;
    transaction
        .execute(
            "UPDATE messages SET state = ?2
             WHERE run_id = ?1 AND role = 'assistant' AND state = 'streaming'",
            params![claimed.run_id.to_string(), message_state],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "UPDATE sessions
             SET active_run_id = NULL,
                  status = CASE WHEN queued_prompts > 0 THEN 'queued' ELSE 'idle' END,
                  estimated_cost_usd_nanos = ?4,
                  cost_known = ?5,
                  context_tokens = CASE
                      WHEN ?6 AND ?7 AND model IS ?9
                      THEN ?8
                      ELSE context_tokens
                  END,
                  updated_at_ms = ?2
             WHERE id = ?1 AND active_run_id = ?3",
            params![
                claimed.session_id.to_string(),
                now,
                claimed.run_id.to_string(),
                next_cost,
                next_cost_known,
                claimed.kind == RunKind::Prompt,
                saw_turn,
                reported_context_tokens,
                &claimed.model.model,
            ],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let summary = load_session_summary(transaction, claimed.session_id)?;
    append_event(
        transaction,
        EventContext {
            store_id,
            workspace_id: claimed.workspace_id,
            session_id: claimed.session_id,
            run_id: Some(claimed.run_id),
            caused_by: Some(claimed.command_id),
            occurred_at_ms: now,
        },
        SessionEvent::RunFinished {
            session: summary,
            run_id: claimed.run_id,
            outcome,
            usage,
            context_tokens,
        },
    )
}

/// The run row's persisted context occupancy: the input-token total of its
/// last committed model turn, NULL until a turn reports usage.
fn run_context_tokens(
    connection: &Connection,
    run_id: RunId,
) -> Result<Option<u64>, SessionRuntimeError> {
    connection
        .query_row(
            "SELECT context_tokens FROM runs WHERE id = ?1",
            [run_id.to_string()],
            |row| row.get(0),
        )
        .map_err(|_| SessionRuntimeError::Persistence)
}

fn finish_queued_run(
    transaction: &Transaction<'_>,
    store_id: StoreId,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    run_id: RunId,
    now: u64,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let outcome = RunOutcome::Cancelled;
    let outcome_json =
        serde_json::to_string(&outcome).map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "UPDATE runs
             SET status = 'cancelled', outcome_json = ?2, finished_at_ms = ?3
             WHERE id = ?1 AND status = 'queued'",
            params![run_id.to_string(), outcome_json, now],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "UPDATE messages SET state = 'cancelled' WHERE run_id = ?1",
            [run_id.to_string()],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "UPDATE sessions
             SET queued_prompts = queued_prompts - 1,
                 status = CASE
                     WHEN active_run_id IS NOT NULL THEN 'running'
                     WHEN queued_prompts > 1 THEN 'queued'
                     ELSE 'idle'
                 END,
                 updated_at_ms = ?2
             WHERE id = ?1",
            params![session_id.to_string(), now],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let summary = load_session_summary(transaction, session_id)?;
    append_event(
        transaction,
        EventContext {
            store_id,
            workspace_id,
            session_id,
            run_id: Some(run_id),
            caused_by: None,
            occurred_at_ms: now,
        },
        SessionEvent::RunFinished {
            session: summary,
            run_id,
            outcome,
            usage: None,
            // A queued run never reached the model; no context to report.
            context_tokens: None,
        },
    )
}

struct OwnedChildCancellations {
    committed_through: Option<EventCursor>,
    running: Vec<RunId>,
    settled_queued: bool,
}

/// Returns running child work whose durable cancellation still needs its
/// in-memory signal. This is also used when an idempotent parent cancellation
/// is replayed after its first caller disappeared before signalling children.
fn owned_running_run_ids(
    connection: &Connection,
    owner_run_id: RunId,
) -> Result<Vec<RunId>, SessionRuntimeError> {
    let mut statement = connection
        .prepare(
            "SELECT r.id
             FROM sessions child JOIN runs r ON r.session_id = child.id
             WHERE child.owner_run_id = ?1
               AND r.status = 'running' AND r.cancel_requested = 1
             ORDER BY r.created_at_ms, r.rowid",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    statement
        .query_map([owner_run_id.to_string()], |row| row.get::<_, String>(0))
        .map_err(|_| SessionRuntimeError::Persistence)?
        .map(|row| {
            let run = row.map_err(|_| SessionRuntimeError::Persistence)?;
            parse_id(&run)
        })
        .collect()
}

/// Cancels every unfinished run in a session spawned by `owner_run_id`.
/// Running children are returned for their in-memory signal; queued children
/// are settled in this transaction, so either ordering between parent
/// cancellation and atomic child creation has a durable outcome.
fn cancel_owned_child_runs(
    transaction: &Transaction<'_>,
    store_id: StoreId,
    owner_run_id: RunId,
    command_id: CommandId,
    now: u64,
) -> Result<OwnedChildCancellations, SessionRuntimeError> {
    let mut statement = transaction
        .prepare(
            "SELECT r.id, child.id, child.workspace_id, r.status
             FROM sessions child JOIN runs r ON r.session_id = child.id
             WHERE child.owner_run_id = ?1 AND r.status IN ('queued', 'running')
             ORDER BY r.created_at_ms, r.rowid",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let owned = statement
        .query_map([owner_run_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    drop(statement);

    let mut committed_through = None;
    let mut running = Vec::new();
    let mut settled_queued = false;
    for (run, session, workspace, status) in owned {
        let run_id: RunId = parse_id(&run)?;
        let session_id: SessionId = parse_id(&session)?;
        let workspace_id: WorkspaceId = parse_id(&workspace)?;
        transaction
            .execute(
                "UPDATE runs SET cancel_requested = 1 WHERE id = ?1",
                [run_id.to_string()],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        let summary = load_session_summary(transaction, session_id)?;
        let requested = append_event(
            transaction,
            EventContext {
                store_id,
                workspace_id,
                session_id,
                run_id: Some(run_id),
                caused_by: Some(command_id),
                occurred_at_ms: now,
            },
            SessionEvent::CancellationRequested {
                session: summary,
                run_id,
            },
        )?;
        if status == "queued" {
            committed_through = Some(
                finish_queued_run(transaction, store_id, workspace_id, session_id, run_id, now)?
                    .cursor,
            );
            settled_queued = true;
        } else {
            committed_through = Some(requested.cursor);
            running.push(run_id);
        }
    }
    Ok(OwnedChildCancellations {
        committed_through,
        running,
        settled_queued,
    })
}

/// Requests cancellation of the session's active auto-compaction when no
/// queued prompt remains to run after it. Returns the compaction's run id
/// (for the in-memory cancellation signal) and the appended
/// `CancellationRequested` event, or `None` when there is nothing to cascade
/// to: prompts still queued, a manual compaction, an ordinary prompt run, or
/// a cancellation already underway.
fn cascade_auto_compaction_cancel(
    transaction: &Transaction<'_>,
    store_id: StoreId,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    command_id: CommandId,
    now: u64,
) -> Result<Option<(RunId, SessionEventEnvelope)>, SessionRuntimeError> {
    let compaction_run = transaction
        .query_row(
            "SELECT r.id FROM sessions s JOIN runs r ON r.id = s.active_run_id
             WHERE s.id = ?1 AND s.queued_prompts = 0
               AND r.kind = 'compaction' AND r.auto_compaction = 1
               AND r.outcome_json IS NULL AND r.cancel_requested = 0",
            [session_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let Some(compaction_run) = compaction_run else {
        return Ok(None);
    };
    let compaction_run: RunId = parse_id(&compaction_run)?;
    transaction
        .execute(
            "UPDATE runs SET cancel_requested = 1 WHERE id = ?1",
            [compaction_run.to_string()],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let summary = load_session_summary(transaction, session_id)?;
    let event = append_event(
        transaction,
        EventContext {
            store_id,
            workspace_id,
            session_id,
            run_id: Some(compaction_run),
            caused_by: Some(command_id),
            occurred_at_ms: now,
        },
        SessionEvent::CancellationRequested {
            session: summary,
            run_id: compaction_run,
        },
    )?;
    Ok(Some((compaction_run, event)))
}

fn recover_interrupted_runs(
    connection: &mut Connection,
    store_id: StoreId,
) -> Result<Vec<EventCursor>, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    // A spawned child can be durably queued while its owner was running when
    // the process stopped. Settle it before recovering running rows: once the
    // owner is marked interrupted its session no longer advertises an active
    // run, but the explicit owner id still proves this child has no waiter.
    let mut statement = transaction
        .prepare(
            "SELECT child_run.id, child.id, child.workspace_id
             FROM runs child_run
             JOIN sessions child ON child.id = child_run.session_id
             JOIN runs owner ON owner.id = child.owner_run_id
             WHERE child_run.status = 'queued'
               AND owner.status IN ('running', 'completed', 'cancelled', 'failed', 'interrupted')
             ORDER BY child_run.created_at_ms, child_run.rowid",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let abandoned_children = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    drop(statement);
    let mut cursors = Vec::with_capacity(abandoned_children.len());
    let recovery_started_at = now_ms();
    for (run, session, workspace) in abandoned_children {
        let event = finish_queued_run(
            &transaction,
            store_id,
            parse_id(&workspace)?,
            parse_id(&session)?,
            parse_id(&run)?,
            recovery_started_at,
        )?;
        cursors.push(event.cursor);
    }
    let mut statement = transaction
        .prepare(
            "SELECT r.id, r.session_id, s.workspace_id
             FROM runs r JOIN sessions s ON s.id = r.session_id
             WHERE r.status = 'running'",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    drop(statement);
    cursors.reserve(rows.len());
    for (run, session, workspace) in rows {
        let run_id = parse_id(&run)?;
        let session_id = parse_id(&session)?;
        let workspace_id = parse_id(&workspace)?;
        let claimed = ClaimedRun {
            workspace_id,
            workspace: String::new(),
            session_id,
            run_id,
            command_id: CommandId::from_bytes([0; 16]),
            // Recovery settles the run row generically; a crashed compaction
            // committed no marker, so interrupting it leaves nothing behind
            // and the command can simply be retried. `child` is likewise
            // irrelevant here: recovery never runs the tool loop.
            kind: RunKind::Prompt,
            child: false,
            user_initiated: false,
            literal_slash: false,
            model: ModelSelection::default(),
            messages: Vec::new(),
            over_budget: false,
            started: SessionEventEnvelope {
                cursor: EventCursor {
                    store_id,
                    workspace_id,
                    sequence: 0,
                },
                session_id,
                run_id: Some(run_id),
                caused_by: None,
                occurred_at_ms: 0,
                event: SessionEvent::RunStarted {
                    session: load_session_summary(&transaction, session_id)?,
                    run_id,
                },
            },
        };
        let event =
            complete_run_in_transaction(&transaction, store_id, &claimed, RunOutcome::Interrupted)?;
        cursors.push(event.cursor);
    }
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(cursors)
}

fn complete_run_in_transaction(
    transaction: &Transaction<'_>,
    store_id: StoreId,
    claimed: &ClaimedRun,
    outcome: RunOutcome,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let now = now_ms();
    let outcome = cancellation_wins(transaction, claimed.run_id, outcome)?;
    interrupt_active_tool_calls(transaction, store_id, claimed, &outcome, None, now)?;
    let (run_status, message_state) = outcome_states(&outcome);
    let outcome_json =
        serde_json::to_string(&outcome).map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "UPDATE runs SET status = ?2, outcome_json = ?3, finished_at_ms = ?4 WHERE id = ?1",
            params![claimed.run_id.to_string(), run_status, outcome_json, now],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "UPDATE messages SET state = ?2
             WHERE run_id = ?1 AND role = 'assistant' AND state = 'streaming'",
            params![claimed.run_id.to_string(), message_state],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "UPDATE sessions
             SET active_run_id = NULL,
                 status = CASE WHEN queued_prompts > 0 THEN 'queued' ELSE 'idle' END,
                 updated_at_ms = ?2
             WHERE id = ?1",
            params![claimed.session_id.to_string(), now],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let summary = load_session_summary(transaction, claimed.session_id)?;
    append_event(
        transaction,
        EventContext {
            store_id,
            workspace_id: claimed.workspace_id,
            session_id: claimed.session_id,
            run_id: Some(claimed.run_id),
            caused_by: None,
            occurred_at_ms: now,
        },
        SessionEvent::RunFinished {
            session: summary,
            run_id: claimed.run_id,
            outcome,
            // Recovery knows no summed usage, but the run row keeps the last
            // committed turn's context occupancy.
            usage: None,
            context_tokens: run_context_tokens(transaction, claimed.run_id)?,
        },
    )
}

fn interrupt_active_tool_calls(
    transaction: &Transaction<'_>,
    store_id: StoreId,
    claimed: &ClaimedRun,
    outcome: &RunOutcome,
    caused_by: Option<CommandId>,
    now: u64,
) -> Result<(), SessionRuntimeError> {
    let mut statement = transaction
        .prepare(
            "SELECT id, state = 'running' FROM tool_calls
             WHERE run_id = ?1 AND state IN ('requested', 'awaiting_approval', 'running')
             ORDER BY turn_ordinal, call_ordinal",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let ids = statement
        .query_map([claimed.run_id.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
        })
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    drop(statement);
    let not_executed_result = match outcome {
        RunOutcome::Completed if ids.is_empty() => return Ok(()),
        RunOutcome::Completed => return Err(SessionRuntimeError::Persistence),
        RunOutcome::Cancelled => "Tool execution did not start before the run was cancelled.",
        RunOutcome::Interrupted => "Tool execution did not start before the run was interrupted.",
        RunOutcome::Failed { .. } => "Tool execution did not start before the run failed.",
    };
    for (id, execution_started) in ids {
        let id = parse_id::<ToolCallId>(&id)?;
        let result = if execution_started {
            INTERRUPTED_TOOL_RESULT
        } else {
            not_executed_result
        };
        transaction
            .execute(
                "UPDATE tool_calls
                 SET state = 'interrupted', result = ?2, is_error = 1, finished_at_ms = ?3
                 WHERE id = ?1 AND state IN ('requested', 'awaiting_approval', 'running')",
                params![id.to_string(), result, now],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        let tool_call = load_tool_call(transaction, id)?;
        append_event(
            transaction,
            EventContext {
                store_id,
                workspace_id: claimed.workspace_id,
                session_id: claimed.session_id,
                run_id: Some(claimed.run_id),
                caused_by,
                occurred_at_ms: now,
            },
            SessionEvent::ToolCallFinished { tool_call },
        )?;
    }
    Ok(())
}

fn cancellation_wins(
    transaction: &Transaction<'_>,
    run_id: RunId,
    outcome: RunOutcome,
) -> Result<RunOutcome, SessionRuntimeError> {
    if matches!(outcome, RunOutcome::Cancelled) {
        return Ok(outcome);
    }
    let requested = transaction
        .query_row(
            "SELECT cancel_requested FROM runs WHERE id = ?1",
            [run_id.to_string()],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(if requested {
        RunOutcome::Cancelled
    } else {
        outcome
    })
}

fn outcome_states(outcome: &RunOutcome) -> (&'static str, &'static str) {
    match outcome {
        RunOutcome::Completed => ("completed", "complete"),
        RunOutcome::Cancelled => ("cancelled", "cancelled"),
        RunOutcome::Interrupted => ("interrupted", "interrupted"),
        RunOutcome::Failed { .. } => ("failed", "failed"),
    }
}

fn load_snapshot(
    connection: &mut Connection,
    store_id: StoreId,
    request: SnapshotRequest,
) -> Result<WorkspaceSnapshot, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let (path, sequence) = transaction
        .query_row(
            "SELECT path, next_sequence FROM workspaces WHERE id = ?1",
            [request.workspace_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?
        .ok_or(SessionRuntimeError::WorkspaceNotFound)?;
    let mut statement = transaction
        .prepare(
            "SELECT id FROM sessions WHERE workspace_id = ?1
             ORDER BY updated_at_ms DESC, rowid DESC LIMIT ?2",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let ids = statement
        .query_map(
            params![
                request.workspace_id.to_string(),
                u64::from(request.session_limit) + 1
            ],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    drop(statement);
    let has_older_sessions = ids.len() > usize::from(request.session_limit);
    let mut sessions = Vec::with_capacity(ids.len().min(usize::from(request.session_limit)));
    for id in ids.into_iter().take(usize::from(request.session_limit)) {
        sessions.push(load_session_summary(&transaction, parse_id(&id)?)?);
    }
    let focused = request
        .focused_session_id
        .map(|session_id| {
            if session_workspace(&transaction, session_id)? != request.workspace_id {
                return Err(SessionRuntimeError::SessionNotFound);
            }
            load_session_snapshot(&transaction, session_id, request.message_limit)
        })
        .transpose()?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(WorkspaceSnapshot {
        cursor: EventCursor {
            store_id,
            workspace_id: request.workspace_id,
            sequence,
        },
        workspace: WorkspaceSummary {
            id: request.workspace_id,
            path,
        },
        sessions,
        focused,
        has_older_sessions,
    })
}

fn load_session_snapshot(
    transaction: &Transaction<'_>,
    session_id: SessionId,
    message_limit: u16,
) -> Result<SessionSnapshot, SessionRuntimeError> {
    let summary = load_session_summary(transaction, session_id)?;
    // Messages order by run first, then by ordinal within the run, so a
    // prompt queued while a run streams does not interleave with that run's
    // later per-turn messages (which receive higher session ordinals).
    let mut statement = transaction
        .prepare(
            "SELECT m.id FROM messages m JOIN runs r ON r.id = m.run_id
             WHERE m.session_id = ?1 AND NOT (m.role = 'assistant' AND m.state = 'queued')
             ORDER BY r.created_at_ms DESC, r.rowid DESC, m.ordinal DESC LIMIT ?2",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let mut message_ids = statement
        .query_map(
            params![session_id.to_string(), u64::from(message_limit) + 1],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    drop(statement);
    let has_older_messages = message_ids.len() > usize::from(message_limit);
    message_ids.truncate(usize::from(message_limit));
    message_ids.reverse();
    let mut messages = Vec::with_capacity(message_ids.len());
    for id in message_ids {
        messages.push(load_message(transaction, parse_id(&id)?)?);
    }
    let mut statement = transaction
        .prepare(
            "SELECT id FROM runs WHERE session_id = ?1
             ORDER BY created_at_ms DESC, rowid DESC LIMIT ?2",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let mut run_ids = statement
        .query_map(params![session_id.to_string(), message_limit], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    drop(statement);
    run_ids.reverse();
    let mut runs = Vec::with_capacity(run_ids.len());
    for id in run_ids {
        runs.push(load_run(transaction, parse_id(&id)?)?);
    }
    let mut statement = transaction
        .prepare(
            "SELECT t.id FROM tool_calls t JOIN runs r ON r.id = t.run_id
             WHERE r.session_id = ?1
             ORDER BY r.created_at_ms DESC, t.turn_ordinal DESC, t.call_ordinal DESC
             LIMIT ?2",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let mut tool_call_ids = statement
        .query_map(
            params![
                session_id.to_string(),
                u64::try_from(MAX_SNAPSHOT_TOOL_CALLS + 1).expect("snapshot bound fits u64")
            ],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    drop(statement);
    let has_older_tool_calls = tool_call_ids.len() > MAX_SNAPSHOT_TOOL_CALLS;
    tool_call_ids.truncate(MAX_SNAPSHOT_TOOL_CALLS);
    tool_call_ids.reverse();
    let mut tool_calls = Vec::with_capacity(tool_call_ids.len());
    for id in tool_call_ids {
        tool_calls.push(load_tool_call(transaction, parse_id(&id)?)?);
    }
    Ok(SessionSnapshot {
        summary,
        messages,
        runs,
        tool_calls,
        has_older_tool_calls,
        has_older_messages,
    })
}

fn read_events(
    connection: &mut Connection,
    workspace_id: WorkspaceId,
    after: u64,
    limit: u16,
) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
    ensure_workspace(connection, workspace_id)?;
    let mut statement = connection
        .prepare(
            "SELECT envelope_json FROM events
             WHERE workspace_id = ?1 AND sequence > ?2
             ORDER BY sequence LIMIT ?3",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    statement
        .query_map(params![workspace_id.to_string(), after, limit], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|_| SessionRuntimeError::Persistence)?
        .map(|row| {
            let encoded = row.map_err(|_| SessionRuntimeError::Persistence)?;
            serde_json::from_str(&encoded).map_err(|_| SessionRuntimeError::Persistence)
        })
        .collect()
}

#[derive(Clone, Copy)]
struct EventContext {
    store_id: StoreId,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    run_id: Option<RunId>,
    caused_by: Option<CommandId>,
    occurred_at_ms: u64,
}

fn append_event(
    transaction: &Transaction<'_>,
    context: EventContext,
    event: SessionEvent,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    transaction
        .execute(
            "UPDATE workspaces SET next_sequence = next_sequence + 1 WHERE id = ?1",
            [context.workspace_id.to_string()],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let sequence = workspace_sequence(transaction, context.workspace_id)?;
    let envelope = SessionEventEnvelope {
        cursor: EventCursor {
            store_id: context.store_id,
            workspace_id: context.workspace_id,
            sequence,
        },
        session_id: context.session_id,
        run_id: context.run_id,
        caused_by: context.caused_by,
        occurred_at_ms: context.occurred_at_ms,
        event,
    };
    let encoded = serde_json::to_string(&envelope).map_err(|_| SessionRuntimeError::Persistence)?;
    if encoded.len() > MAX_PERSISTED_EVENT_BYTES {
        return Err(SessionRuntimeError::EventTooLarge);
    }
    transaction
        .execute(
            "INSERT INTO events(workspace_id, sequence, envelope_json) VALUES (?1, ?2, ?3)",
            params![context.workspace_id.to_string(), sequence, encoded],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(envelope)
}

fn session_parent(
    connection: &Connection,
    session_id: SessionId,
) -> Result<Option<SessionId>, SessionRuntimeError> {
    connection
        .query_row(
            "SELECT parent_id FROM sessions WHERE id = ?1",
            [session_id.to_string()],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?
        .ok_or(SessionRuntimeError::SessionNotFound)?
        .as_deref()
        .map(parse_id)
        .transpose()
}

#[derive(Default)]
struct AccountingAggregate {
    usage: TokenUsage,
    usage_known: bool,
    saw_usage: bool,
    cost: Option<u64>,
}

impl AccountingAggregate {
    fn known_zero() -> Self {
        Self {
            usage_known: true,
            saw_usage: true,
            cost: Some(0),
            ..Self::default()
        }
    }

    fn add(&mut self, usage: TokenUsage, cost: Option<u64>) -> Result<(), SessionRuntimeError> {
        self.usage =
            add_usage(self.usage, usage).ok_or(SessionRuntimeError::AccountingUnavailable)?;
        self.saw_usage = true;
        self.cost = match (self.cost, cost) {
            (Some(total), Some(cost)) => Some(
                total
                    .checked_add(cost)
                    .ok_or(SessionRuntimeError::AccountingUnavailable)?,
            ),
            _ => None,
        };
        Ok(())
    }

    fn mark_unknown(&mut self) {
        self.usage_known = false;
        self.cost = None;
    }

    fn total(self) -> AccountingTotal {
        AccountingTotal {
            usage: (self.saw_usage && self.usage_known).then_some(self.usage),
            estimated_cost_usd_nanos: self.cost,
        }
    }
}

fn load_session_accounting(
    connection: &Connection,
    session_id: SessionId,
) -> Result<SessionAccounting, SessionRuntimeError> {
    let mut statement = connection
        .prepare(
            "SELECT r.session_id, r.status, r.usage_json, r.estimated_cost_usd_nanos,
                    EXISTS(SELECT 1 FROM model_turns turn WHERE turn.run_id = r.id)
             FROM runs r
             JOIN sessions owner ON owner.id = r.session_id
             WHERE owner.id = ?1 OR owner.parent_id = ?1
             ORDER BY r.rowid",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let rows = statement
        .query_map([session_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<u64>>(3)?,
                row.get::<_, bool>(4)?,
            ))
        })
        .map_err(|_| SessionRuntimeError::Persistence)?;

    let session_id = session_id.to_string();
    let mut direct = AccountingAggregate::known_zero();
    let mut inclusive = AccountingAggregate::known_zero();
    for row in rows {
        let (owner_id, status, encoded_usage, cost, saw_turn) =
            row.map_err(|_| SessionRuntimeError::Persistence)?;
        let Some(encoded_usage) = encoded_usage else {
            let terminal = matches!(
                status.as_str(),
                "completed" | "cancelled" | "failed" | "interrupted"
            );
            // A cancelled run with no committed model turn spent no measured
            // request and preserves known prior accounting. Other terminal
            // rows without usage stay unknown for legacy/provider-failure
            // compatibility; a committed turn is always an explicit unknown.
            if saw_turn || (terminal && status != "cancelled") {
                inclusive.mark_unknown();
                if owner_id == session_id {
                    direct.mark_unknown();
                }
            }
            continue;
        };
        let usage = match serde_json::from_str::<TokenUsage>(&encoded_usage) {
            Ok(usage) => usage,
            Err(_) => {
                inclusive.mark_unknown();
                if owner_id == session_id {
                    direct.mark_unknown();
                }
                continue;
            }
        };
        inclusive.add(usage, cost)?;
        if owner_id == session_id {
            direct.add(usage, cost)?;
        }
    }
    Ok(SessionAccounting {
        direct: direct.total(),
        inclusive: inclusive.total(),
    })
}

fn load_session_summary(
    connection: &Connection,
    session_id: SessionId,
) -> Result<SessionSummary, SessionRuntimeError> {
    let accounting = load_session_accounting(connection, session_id)?;
    connection
        .query_row(
            "SELECT s.workspace_id, s.parent_id, s.title, s.status, s.active_run_id,
                     s.queued_prompts, s.model, s.context_tokens, s.updated_at_ms,
                     (SELECT outcome_json FROM runs
                      WHERE session_id = s.id AND outcome_json IS NOT NULL
                      ORDER BY finished_at_ms DESC, rowid DESC LIMIT 1)
              FROM sessions s WHERE s.id = ?1",
            [session_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, u16>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<u64>>(7)?,
                    row.get::<_, u64>(8)?,
                    row.get::<_, Option<String>>(9)?,
                ))
            },
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?
        .ok_or(SessionRuntimeError::SessionNotFound)
        .and_then(
            |(
                workspace,
                parent,
                title,
                status,
                active,
                queued,
                model,
                context_tokens,
                updated,
                last_outcome,
            )| {
                let direct_cost = accounting.direct.estimated_cost_usd_nanos;
                Ok(SessionSummary {
                    id: session_id,
                    workspace_id: parse_id(&workspace)?,
                    parent_id: parent.as_deref().map(parse_id).transpose()?,
                    title,
                    status: parse_session_status(&status)?,
                    active_run_id: active.as_deref().map(parse_id).transpose()?,
                    queued_prompts: queued,
                    model,
                    context_tokens,
                    accounting: Some(accounting),
                    estimated_cost_usd_nanos: direct_cost,
                    updated_at_ms: updated,
                    last_outcome: last_outcome
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()
                        .map_err(|_| SessionRuntimeError::Persistence)?,
                })
            },
        )
}

fn load_message(
    connection: &Connection,
    message_id: MessageId,
) -> Result<MessageSnapshot, SessionRuntimeError> {
    connection
        .query_row(
            "SELECT session_id, run_id, turn_ordinal, role, state, output, refusal, created_at_ms
             FROM messages WHERE id = ?1",
            [message_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u16>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, u64>(7)?,
                ))
            },
        )
        .map_err(|_| SessionRuntimeError::Persistence)
        .and_then(
            |(session, run, turn_ordinal, role, state, output, refusal, created)| {
                Ok(MessageSnapshot {
                    id: message_id,
                    session_id: parse_id(&session)?,
                    run_id: parse_id(&run)?,
                    turn_ordinal,
                    role: parse_message_role(&role)?,
                    state: parse_message_state(&state)?,
                    output,
                    refusal,
                    created_at_ms: created,
                })
            },
        )
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PersistedContentBlock {
    Text {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
}

impl From<&ContentBlock> for PersistedContentBlock {
    fn from(block: &ContentBlock) -> Self {
        match block {
            ContentBlock::Text { text } => Self::Text { text: text.clone() },
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => Self::ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            },
            ContentBlock::ToolResult {
                call_id,
                content,
                is_error,
            } => Self::ToolResult {
                call_id: call_id.clone(),
                content: content.clone(),
                is_error: *is_error,
            },
        }
    }
}

impl From<PersistedContentBlock> for ContentBlock {
    fn from(block: PersistedContentBlock) -> Self {
        match block {
            PersistedContentBlock::Text { text } => Self::Text { text },
            PersistedContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => Self::ToolCall {
                id,
                name,
                arguments,
            },
            PersistedContentBlock::ToolResult {
                call_id,
                content,
                is_error,
            } => Self::ToolResult {
                call_id,
                content,
                is_error,
            },
        }
    }
}

/// Assembles the provider messages for one session: the latest compaction
/// summary (when one exists), then the verbatim transcript after its cutoff,
/// with read-only tool results outside the recency window replaced by stubs.
/// The stored rows are never modified — pruning and summarization are
/// properties of assembly alone.
fn load_model_context(
    transaction: &Transaction<'_>,
    session_id: SessionId,
    through_ordinal: u64,
) -> Result<Vec<Message>, SessionRuntimeError> {
    let compaction = latest_compaction(transaction, session_id)?;
    let cutoff_ordinal = compaction
        .as_ref()
        .map_or(0, |compaction| compaction.cutoff_ordinal);
    // SQLite integers are i64; `u64::MAX` means "everything".
    let through_ordinal = through_ordinal.min(u64::try_from(i64::MAX).unwrap_or(u64::MAX));
    let mut statement = transaction
        .prepare(
            "SELECT id FROM messages
             WHERE session_id = ?1 AND ordinal <= ?2 AND ordinal > ?3
               AND role = 'user'
               AND state IN ('complete', 'cancelled', 'failed', 'interrupted')
             ORDER BY ordinal",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let message_ids = statement
        .query_map(
            params![session_id.to_string(), through_ordinal, cutoff_ordinal],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    drop(statement);

    let mut context = Vec::new();
    if let Some(compaction) = compaction {
        context.push(Message::user(format!(
            "{COMPACTION_SUMMARY_PREAMBLE}\n\n{}",
            compaction.summary
        )));
    }
    for id in message_ids {
        let snapshot = load_message(transaction, parse_id(&id)?)?;
        if snapshot.role != MessageRole::User {
            return Err(SessionRuntimeError::Persistence);
        }
        context.push(Message::user(snapshot.output));
        // Reconstruct each run immediately after its prompt rather than
        // following message-row ordinals. Follow-up prompts can be queued
        // while the prior run is active, so its later committed output still
        // belongs before the follow-up in model context.
        let status: String = transaction
            .query_row(
                "SELECT status FROM runs WHERE id = ?1",
                [snapshot.run_id.to_string()],
                |row| row.get(0),
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        if matches!(
            status.as_str(),
            "completed" | "cancelled" | "failed" | "interrupted" | "running"
        ) {
            let has_turns: bool = transaction
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM model_turns WHERE run_id = ?1)",
                    [snapshot.run_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            if has_turns {
                append_run_turns(transaction, snapshot.run_id, &mut context)?;
            } else {
                append_legacy_run_messages(transaction, snapshot.run_id, &mut context)?;
            }
        }
        if matches!(status.as_str(), "cancelled" | "failed" | "interrupted") {
            let outcome_json: String = transaction
                .query_row(
                    "SELECT outcome_json FROM runs WHERE id = ?1",
                    [snapshot.run_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            let outcome: RunOutcome = serde_json::from_str(&outcome_json)
                .map_err(|_| SessionRuntimeError::Persistence)?;
            if let Some(notice) = runtime_notice(&outcome) {
                context.push(Message::user(notice));
            }
        }
    }
    prune_stale_tool_results(&mut context);
    Ok(context)
}

fn append_legacy_run_messages(
    connection: &Connection,
    run_id: RunId,
    context: &mut Vec<Message>,
) -> Result<(), SessionRuntimeError> {
    let mut statement = connection
        .prepare(
            "SELECT output, refusal FROM messages
             WHERE run_id = ?1 AND role = 'assistant' AND state = 'complete'
             ORDER BY turn_ordinal, ordinal",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let messages = statement
        .query_map([run_id.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    for (output, refusal) in messages {
        let content = if output.is_empty() { refusal } else { output };
        if !content.trim().is_empty() {
            context.push(Message::assistant(content));
        }
    }
    Ok(())
}

fn runtime_notice(outcome: &RunOutcome) -> Option<String> {
    let status = match outcome {
        RunOutcome::Completed => return None,
        RunOutcome::Cancelled => "The previous run was cancelled.".to_owned(),
        RunOutcome::Interrupted => "The previous run was interrupted before completion.".to_owned(),
        RunOutcome::Failed { failure } => format!("The previous run failed: {}", failure.message),
    };
    Some(format!(
        "{RUNTIME_NOTICE_PREAMBLE}\n{status}\n{RUNTIME_NOTICE_GUIDANCE}"
    ))
}

/// One persisted compaction: the summary that replaces everything at or
/// before `cutoff_ordinal` in assembly.
struct CompactionRow {
    summary: String,
    cutoff_ordinal: u64,
}

fn latest_compaction(
    connection: &Connection,
    session_id: SessionId,
) -> Result<Option<CompactionRow>, SessionRuntimeError> {
    connection
        .query_row(
            "SELECT summary, cutoff_ordinal FROM session_compactions
             WHERE session_id = ?1 ORDER BY rowid DESC LIMIT 1",
            [session_id.to_string()],
            |row| {
                Ok(CompactionRow {
                    summary: row.get(0)?,
                    cutoff_ordinal: row.get(1)?,
                })
            },
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)
}

/// Replaces read-only tool results older than the recency window with
/// one-line stubs. Only the built-in read-only tools are prunable — their
/// results are re-derivable on demand; mutating, shell, and MCP outputs are
/// not. The window keeps the last [`CONTEXT_PRUNE_KEEP_TURNS`] model turns
/// (assistant messages) verbatim. `is_error` is preserved so an error result
/// stays an error stub.
fn prune_stale_tool_results(context: &mut [Message]) {
    let assistant_positions = context
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role() == Role::Assistant)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let Some(&window_start) = assistant_positions
        .len()
        .checked_sub(CONTEXT_PRUNE_KEEP_TURNS)
        .and_then(|index| assistant_positions.get(index))
    else {
        return;
    };
    // Map provider call ids to the tool that produced them; the ToolResult
    // block alone does not name its tool.
    let mut calls = HashMap::new();
    for message in context.iter() {
        for block in message.content() {
            if let ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } = block
            {
                calls.insert(id.clone(), (name.clone(), arguments.to_string()));
            }
        }
    }
    for message in &mut context[..window_start] {
        let needs_pruning = message.content().iter().any(|block| {
            matches!(block, ContentBlock::ToolResult { call_id, content, .. }
                if prunable_stub(&calls, call_id, content).is_some())
        });
        if !needs_pruning {
            continue;
        }
        let content = message
            .content()
            .iter()
            .map(|block| match block {
                ContentBlock::ToolResult {
                    call_id,
                    content,
                    is_error,
                } => match prunable_stub(&calls, call_id, content) {
                    Some(stub) => ContentBlock::ToolResult {
                        call_id: call_id.clone(),
                        content: stub,
                        is_error: *is_error,
                    },
                    None => block.clone(),
                },
                block => block.clone(),
            })
            .collect();
        *message = Message::new(message.role(), content);
    }
}

/// The stub replacing a prunable read-only result, or `None` when the result
/// must stay verbatim (unknown call, non-read-only tool, or already smaller
/// than the stub would be).
fn prunable_stub(
    calls: &HashMap<String, (String, String)>,
    call_id: &str,
    content: &str,
) -> Option<String> {
    let (name, arguments) = calls.get(call_id)?;
    if !PRUNABLE_READ_ONLY_TOOLS.contains(&name.as_str()) {
        return None;
    }
    let mut arguments = arguments.clone();
    if arguments.len() > CONTEXT_PRUNE_STUB_ARGUMENT_BYTES {
        arguments = truncate_utf8(arguments, CONTEXT_PRUNE_STUB_ARGUMENT_BYTES);
        arguments.push_str("...");
    }
    let stub = format!(
        "[pruned: {name} {arguments} returned {} bytes; call it again if needed]",
        content.len()
    );
    (content.len() > stub.len()).then_some(stub)
}

/// The byte weight the assembled context contributes to the session budget:
/// message text, tool-call names and arguments, and (pruned) tool results.
fn context_bytes(messages: &[Message]) -> usize {
    messages
        .iter()
        .flat_map(Message::content)
        .map(|block| match block {
            ContentBlock::Text { text } => text.len(),
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => id.len() + name.len() + arguments.to_string().len(),
            ContentBlock::ToolResult {
                call_id, content, ..
            } => call_id.len() + content.len(),
        })
        .fold(0_usize, usize::saturating_add)
}

/// Measures the session's context as the next run would assemble it —
/// summary plus post-cutoff transcript with pruning applied — plus any text
/// still streaming into the current turn's message, which has not joined a
/// committed turn yet but will.
fn assembled_context_bytes(
    transaction: &Transaction<'_>,
    session_id: SessionId,
) -> Result<usize, SessionRuntimeError> {
    let context = load_model_context(transaction, session_id, u64::MAX)?;
    let streaming_bytes: u64 = transaction
        .query_row(
            "SELECT COALESCE(SUM(
                 length(CAST(output AS BLOB)) + length(CAST(refusal AS BLOB))
             ), 0) FROM messages WHERE session_id = ?1 AND state = 'streaming'",
            [session_id.to_string()],
            |row| row.get(0),
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(context_bytes(&context)
        .saturating_add(usize::try_from(streaming_bytes).unwrap_or(usize::MAX)))
}

/// The final user message of a compaction run: the fixed structured-schema
/// instruction plus the file list seeded mechanically from the session's
/// file-state table.
fn compaction_instruction(
    connection: &Connection,
    session_id: SessionId,
) -> Result<String, SessionRuntimeError> {
    let mut statement = connection
        .prepare("SELECT path FROM session_files WHERE session_id = ?1 ORDER BY path")
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let paths = statement
        .query_map([session_id.to_string()], |row| row.get::<_, String>(0))
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let mut instruction = String::from(COMPACTION_INSTRUCTION);
    instruction.push_str("\n\nFiles touched (seeded from the session file-state table):\n");
    if paths.is_empty() {
        instruction.push_str("(none recorded)\n");
    } else {
        for path in paths {
            instruction.push_str("- ");
            instruction.push_str(&path);
            instruction.push('\n');
        }
    }
    Ok(instruction)
}

/// Replays one run's persisted model turns (assistant content and tool
/// results) into `context`, in turn order.
fn append_run_turns(
    transaction: &Transaction<'_>,
    run_id: RunId,
    context: &mut Vec<Message>,
) -> Result<(), SessionRuntimeError> {
    let mut statement = transaction
        .prepare(
            "SELECT turn_ordinal, assistant_content_json FROM model_turns
             WHERE run_id = ?1 ORDER BY turn_ordinal",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let turns = statement
        .query_map([run_id.to_string()], |row| {
            Ok((row.get::<_, u16>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    drop(statement);
    for (turn_ordinal, content_json) in turns {
        let content: Vec<ContentBlock> =
            serde_json::from_str::<Vec<PersistedContentBlock>>(&content_json)
                .map_err(|_| SessionRuntimeError::Persistence)?
                .into_iter()
                .map(ContentBlock::from)
                .collect();

        let mut statement = transaction
            .prepare(
                "SELECT provider_call_id, result, is_error FROM tool_calls
                 WHERE run_id = ?1 AND turn_ordinal = ?2 AND result IS NOT NULL
                 ORDER BY call_ordinal",
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        let mut recorded = statement
            .query_map(params![run_id.to_string(), turn_ordinal], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (row.get::<_, String>(1)?, row.get::<_, bool>(2)?),
                ))
            })
            .map_err(|_| SessionRuntimeError::Persistence)?
            .collect::<Result<HashMap<String, (String, bool)>, _>>()
            .map_err(|_| SessionRuntimeError::Persistence)?;
        drop(statement);
        // Emit exactly one result per ToolCall block, in block order.
        // A block without a recorded result (a crash between the
        // turn commit and its tool_calls rows in an older store)
        // gets an explicit interrupted result so replayed context
        // stays provider-valid instead of poisoning the session.
        let results = content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall { id, .. } => Some(match recorded.remove(id) {
                    Some((content, is_error)) => ContentBlock::ToolResult {
                        call_id: id.clone(),
                        content,
                        is_error,
                    },
                    None => ContentBlock::ToolResult {
                        call_id: id.clone(),
                        content: INTERRUPTED_TOOL_RESULT.to_owned(),
                        is_error: true,
                    },
                }),
                ContentBlock::Text { .. } | ContentBlock::ToolResult { .. } => None,
            })
            .collect::<Vec<_>>();
        context.push(Message::new(Role::Assistant, content));
        if !results.is_empty() {
            context.push(Message::tool_results(results));
        }
    }
    Ok(())
}

fn load_tool_call(
    connection: &Connection,
    tool_call_id: ToolCallId,
) -> Result<ToolCallSnapshot, SessionRuntimeError> {
    connection
        .query_row(
            "SELECT r.session_id, t.run_id, t.turn_ordinal, t.call_ordinal,
                    t.provider_call_id, t.name, t.arguments_json, t.state, t.result, t.is_error,
                    t.display_json
             FROM tool_calls t JOIN runs r ON r.id = t.run_id WHERE t.id = ?1",
            [tool_call_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u16>(2)?,
                    row.get::<_, u16>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, bool>(9)?,
                    row.get::<_, Option<String>>(10)?,
                ))
            },
        )
        .map_err(|_| SessionRuntimeError::Persistence)
        .and_then(
            |(
                session,
                run,
                turn,
                call,
                provider_id,
                name,
                arguments,
                state,
                result,
                is_error,
                display,
            )| {
                Ok(ToolCallSnapshot {
                    id: tool_call_id,
                    session_id: parse_id(&session)?,
                    run_id: parse_id(&run)?,
                    turn_ordinal: turn,
                    call_ordinal: call,
                    provider_call_id: provider_id,
                    name,
                    arguments,
                    state: parse_tool_call_state(&state)?,
                    result,
                    is_error,
                    display: display
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()
                        .map_err(|_| SessionRuntimeError::Persistence)?,
                })
            },
        )
}

fn load_run(connection: &Connection, run_id: RunId) -> Result<RunSnapshot, SessionRuntimeError> {
    connection
        .query_row(
            "SELECT session_id, status, outcome_json, prompt_identity_json,
                    usage_json, context_tokens,
                    estimated_cost_usd_nanos
             FROM runs WHERE id = ?1",
            [run_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<u64>>(5)?,
                    row.get::<_, Option<u64>>(6)?,
                ))
            },
        )
        .map_err(|_| SessionRuntimeError::Persistence)
        .and_then(
            |(session, status, outcome, prompt_identity, usage, context_tokens, cost)| {
                Ok(RunSnapshot {
                    id: run_id,
                    session_id: parse_id(&session)?,
                    status: parse_run_status(&status)?,
                    outcome: outcome
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()
                        .map_err(|_| SessionRuntimeError::Persistence)?,
                    prompt_identity: prompt_identity
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()
                        .map_err(|_| SessionRuntimeError::Persistence)?
                        .map(Box::new),
                    usage: usage
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()
                        .map_err(|_| SessionRuntimeError::Persistence)?,
                    context_tokens,
                    estimated_cost_usd_nanos: cost,
                })
            },
        )
}

fn run_cost(usage: TokenUsage, pricing: &ModelPricing) -> Option<u64> {
    let total_input = usage
        .input_tokens
        .checked_add(usage.cache_read_input_tokens)?
        .checked_add(usage.cache_write_input_tokens)?;
    let tier = pricing
        .context_tier
        .as_ref()
        .filter(|tier| total_input > tier.above_input_tokens);
    let input_rate = tier.map_or(pricing.input_usd_nanos_per_token, |tier| {
        tier.input_usd_nanos_per_token
    });
    let output_rate = tier.map_or(pricing.output_usd_nanos_per_token, |tier| {
        tier.output_usd_nanos_per_token
    });
    let cache_read_price = tier
        .and_then(|tier| tier.cache_read_usd_nanos_per_token)
        .or(pricing.cache_read_usd_nanos_per_token);
    let cache_write_price = tier
        .and_then(|tier| tier.cache_write_usd_nanos_per_token)
        .or(pricing.cache_write_usd_nanos_per_token);
    let cache_read_rate = if usage.cache_read_input_tokens == 0 {
        0
    } else {
        cache_read_price?
    };
    let cache_write_rate = if usage.cache_write_input_tokens == 0 {
        0
    } else {
        cache_write_price?
    };
    let total = u128::from(usage.input_tokens)
        .checked_mul(u128::from(input_rate))?
        .checked_add(u128::from(usage.output_tokens).checked_mul(u128::from(output_rate))?)?
        .checked_add(
            u128::from(usage.cache_read_input_tokens).checked_mul(u128::from(cache_read_rate))?,
        )?
        .checked_add(
            u128::from(usage.cache_write_input_tokens).checked_mul(u128::from(cache_write_rate))?,
        )?;
    u64::try_from(total).ok()
}

/// The model-selection rules shared by `CreateSession` and `SetSessionModel`:
/// a bounded `provider/model` route, a nonzero token budget, and a bounded
/// organization.
fn validate_model_selection(model: &ModelSelection) -> Result<(), SessionRuntimeError> {
    if !model.model.as_ref().is_some_and(|value| {
        value.len() <= MAX_MODEL_SELECTION_BYTES
            && value
                .split_once('/')
                .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
    }) || model.max_output_tokens == Some(0)
        || model
            .organization
            .as_ref()
            .is_some_and(|value| value.len() > MAX_MODEL_SELECTION_BYTES)
    {
        return Err(SessionRuntimeError::InvalidModelSelection);
    }
    Ok(())
}

/// Deletes one idle session and every row it owns, then appends
/// `SessionDeleted`, all inside the caller's transaction.
///
/// Refused while the session has an active run. That guard also keeps the
/// runtime's in-memory maps clean without extra plumbing: cancellation
/// senders and pending approvals exist only for claimed (executing) runs and
/// are removed when the run finishes, so a deletable session can have none.
///
/// The session's rows in the `events` log are deliberately kept. Workspace
/// cursors promise a gapless `previous + 1` sequence to subscribers (and the
/// sequence counter lives on the workspace row, so deletion could never
/// regress it), which means deleting event rows would break every replay
/// that spans the deletion. Replaying the kept events is harmless:
/// `SessionDeleted` removes the child, and a following parent
/// `SessionUpdated` refreshes the live inclusive projection when needed.
fn delete_idle_session(
    transaction: &Transaction<'_>,
    store_id: StoreId,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    command_id: CommandId,
    now: u64,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let active_run: Option<String> = transaction
        .query_row(
            "SELECT active_run_id FROM sessions WHERE id = ?1",
            [session_id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?
        .ok_or(SessionRuntimeError::SessionNotFound)?;
    if active_run.is_some() {
        return Err(SessionRuntimeError::SessionActive);
    }
    let parent_id = session_parent(transaction, session_id)?;
    let session = session_id.to_string();
    // Children survive their parent as root sessions; their spawn ownership
    // ends with that parent because its run rows are deleted below.
    for statement in [
        "UPDATE sessions SET parent_id = NULL, owner_run_id = NULL WHERE parent_id = ?1",
        "DELETE FROM tool_calls WHERE run_id IN (SELECT id FROM runs WHERE session_id = ?1)",
        "DELETE FROM model_turns WHERE run_id IN (SELECT id FROM runs WHERE session_id = ?1)",
        "DELETE FROM messages WHERE session_id = ?1",
        "DELETE FROM runs WHERE session_id = ?1",
        "DELETE FROM session_grants WHERE session_id = ?1",
        "DELETE FROM session_files WHERE session_id = ?1",
        "DELETE FROM session_compactions WHERE session_id = ?1",
        "DELETE FROM sessions WHERE id = ?1",
    ] {
        transaction
            .execute(statement, [&session])
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    let deleted = append_event(
        transaction,
        EventContext {
            store_id,
            workspace_id,
            session_id,
            run_id: None,
            caused_by: Some(command_id),
            occurred_at_ms: now,
        },
        SessionEvent::SessionDeleted { session_id },
    )?;
    let Some(parent_id) = parent_id else {
        return Ok(deleted);
    };
    let session = load_session_summary(transaction, parent_id)?;
    append_event(
        transaction,
        EventContext {
            store_id,
            workspace_id,
            session_id: parent_id,
            run_id: None,
            caused_by: Some(command_id),
            occurred_at_ms: now,
        },
        SessionEvent::SessionUpdated { session },
    )
}

fn ensure_workspace(
    connection: &Connection,
    workspace_id: WorkspaceId,
) -> Result<(), SessionRuntimeError> {
    let found = connection
        .query_row(
            "SELECT 1 FROM workspaces WHERE id = ?1",
            [workspace_id.to_string()],
            |_| Ok(()),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    found.ok_or(SessionRuntimeError::WorkspaceNotFound)
}

fn session_workspace(
    connection: &Connection,
    session_id: SessionId,
) -> Result<WorkspaceId, SessionRuntimeError> {
    let workspace = connection
        .query_row(
            "SELECT workspace_id FROM sessions WHERE id = ?1",
            [session_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?
        .ok_or(SessionRuntimeError::SessionNotFound)?;
    parse_id(&workspace)
}

fn workspace_sequence(
    connection: &Connection,
    workspace_id: WorkspaceId,
) -> Result<u64, SessionRuntimeError> {
    connection
        .query_row(
            "SELECT next_sequence FROM workspaces WHERE id = ?1",
            [workspace_id.to_string()],
            |row| row.get(0),
        )
        .map_err(|_| SessionRuntimeError::Persistence)
}

fn parse_id<T>(value: &str) -> Result<T, SessionRuntimeError>
where
    T: std::str::FromStr,
{
    value.parse().map_err(|_| SessionRuntimeError::Persistence)
}

fn parse_session_status(value: &str) -> Result<SessionStatus, SessionRuntimeError> {
    match value {
        "idle" => Ok(SessionStatus::Idle),
        "queued" => Ok(SessionStatus::Queued),
        "running" => Ok(SessionStatus::Running),
        _ => Err(SessionRuntimeError::Persistence),
    }
}

fn parse_run_status(value: &str) -> Result<RunStatus, SessionRuntimeError> {
    match value {
        "queued" => Ok(RunStatus::Queued),
        "running" => Ok(RunStatus::Running),
        "completed" => Ok(RunStatus::Completed),
        "cancelled" => Ok(RunStatus::Cancelled),
        "failed" => Ok(RunStatus::Failed),
        "interrupted" => Ok(RunStatus::Interrupted),
        _ => Err(SessionRuntimeError::Persistence),
    }
}

fn parse_message_role(value: &str) -> Result<MessageRole, SessionRuntimeError> {
    match value {
        "user" => Ok(MessageRole::User),
        "assistant" => Ok(MessageRole::Assistant),
        _ => Err(SessionRuntimeError::Persistence),
    }
}

fn parse_message_state(value: &str) -> Result<MessageState, SessionRuntimeError> {
    match value {
        "queued" => Ok(MessageState::Queued),
        "streaming" => Ok(MessageState::Streaming),
        "complete" => Ok(MessageState::Complete),
        "cancelled" => Ok(MessageState::Cancelled),
        "failed" => Ok(MessageState::Failed),
        "interrupted" => Ok(MessageState::Interrupted),
        _ => Err(SessionRuntimeError::Persistence),
    }
}

const fn approval_mode_str(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::ReadOnly => "read_only",
        ApprovalMode::Ask => "ask",
        ApprovalMode::Auto => "auto",
        ApprovalMode::Full => "full",
    }
}

fn parse_approval_mode(value: &str) -> Result<ApprovalMode, SessionRuntimeError> {
    match value {
        "read_only" => Ok(ApprovalMode::ReadOnly),
        "ask" => Ok(ApprovalMode::Ask),
        "auto" => Ok(ApprovalMode::Auto),
        "full" => Ok(ApprovalMode::Full),
        _ => Err(SessionRuntimeError::Persistence),
    }
}

const fn approval_resolution_str(resolution: ApprovalResolution) -> &'static str {
    match resolution {
        ApprovalResolution::ApprovedOnce => "approved_once",
        ApprovalResolution::ApprovedForSession => "approved_for_session",
        ApprovalResolution::ApprovedForWorkspace => "approved_for_workspace",
        ApprovalResolution::ApprovedByReviewer => "approved_by_reviewer",
        ApprovalResolution::Denied => "denied",
        ApprovalResolution::DeniedTimeout => "denied_timeout",
        ApprovalResolution::DeniedByReviewer => "denied_by_reviewer",
    }
}

fn parse_approval_resolution(value: &str) -> Result<ApprovalResolution, SessionRuntimeError> {
    match value {
        "approved_once" => Ok(ApprovalResolution::ApprovedOnce),
        "approved_for_session" => Ok(ApprovalResolution::ApprovedForSession),
        "approved_for_workspace" => Ok(ApprovalResolution::ApprovedForWorkspace),
        "approved_by_reviewer" => Ok(ApprovalResolution::ApprovedByReviewer),
        "denied" => Ok(ApprovalResolution::Denied),
        "denied_timeout" => Ok(ApprovalResolution::DeniedTimeout),
        "denied_by_reviewer" => Ok(ApprovalResolution::DeniedByReviewer),
        _ => Err(SessionRuntimeError::Persistence),
    }
}

fn parse_tool_call_state(value: &str) -> Result<ToolCallState, SessionRuntimeError> {
    match value {
        "requested" => Ok(ToolCallState::Requested),
        "awaiting_approval" => Ok(ToolCallState::AwaitingApproval),
        "running" => Ok(ToolCallState::Running),
        "completed" => Ok(ToolCallState::Completed),
        "failed" => Ok(ToolCallState::Failed),
        "denied" => Ok(ToolCallState::Denied),
        "interrupted" => Ok(ToolCallState::Interrupted),
        _ => Err(SessionRuntimeError::Persistence),
    }
}

fn prompt_title(prompt: &str) -> String {
    let mut title = String::new();
    let mut characters = 0;
    let mut pending_space = false;
    let mut truncated = false;
    for character in prompt.chars() {
        if character.is_whitespace() {
            pending_space = !title.is_empty();
            continue;
        }
        if character.is_control()
            || matches!(
                character,
                '\u{061c}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            )
        {
            continue;
        }
        if pending_space {
            if characters + 1 >= 48 {
                truncated = true;
                break;
            }
            title.push(' ');
            characters += 1;
            pending_space = false;
        }
        if characters == 48 {
            truncated = true;
            break;
        }
        title.push(character);
        characters += 1;
    }
    if truncated {
        title.push_str("...");
    }
    if title.is_empty() {
        "New session".to_owned()
    } else {
        title
    }
}

fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use async_stream::stream as async_stream;
    use futures_util::{StreamExt, stream};
    use qq_provider::{ModelRequest, Provider, ProviderStream};
    use tempfile::TempDir;

    use super::*;

    fn usage(input_tokens: u64, output_tokens: u64) -> TokenUsage {
        TokenUsage {
            input_tokens,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens,
        }
    }

    #[test]
    fn accounting_aggregate_sums_known_usage_and_cost() {
        let mut aggregate = AccountingAggregate::known_zero();
        aggregate.add(usage(3, 5), Some(11)).unwrap();
        aggregate.add(usage(7, 13), Some(17)).unwrap();

        assert_eq!(
            aggregate.total(),
            AccountingTotal {
                usage: Some(usage(10, 18)),
                estimated_cost_usd_nanos: Some(28),
            }
        );
    }

    #[test]
    fn accounting_aggregate_keeps_unknown_cost_distinct_from_zero() {
        let mut aggregate = AccountingAggregate::known_zero();
        aggregate.add(usage(3, 5), Some(11)).unwrap();
        aggregate.add(usage(7, 13), None).unwrap();
        aggregate.add(usage(1, 2), Some(17)).unwrap();

        let total = aggregate.total();
        assert_eq!(total.usage, Some(usage(11, 20)));
        assert_eq!(total.estimated_cost_usd_nanos, None);
    }

    #[test]
    fn accounting_aggregate_keeps_zero_usage_cost_unknown() {
        let mut aggregate = AccountingAggregate::known_zero();
        aggregate.add(usage(0, 0), None).unwrap();

        assert_eq!(
            aggregate.total(),
            AccountingTotal {
                usage: Some(usage(0, 0)),
                estimated_cost_usd_nanos: None,
            }
        );
    }

    #[test]
    fn accounting_aggregate_rejects_usage_and_cost_overflow() {
        let mut usage_overflow = AccountingAggregate::known_zero();
        usage_overflow.add(usage(u64::MAX, 0), Some(0)).unwrap();
        assert!(matches!(
            usage_overflow.add(usage(1, 0), Some(0)),
            Err(SessionRuntimeError::AccountingUnavailable)
        ));

        let mut cost_overflow = AccountingAggregate::known_zero();
        cost_overflow.add(usage(1, 0), Some(u64::MAX)).unwrap();
        assert!(matches!(
            cost_overflow.add(usage(1, 0), Some(1)),
            Err(SessionRuntimeError::AccountingUnavailable)
        ));
    }

    fn insert_accounting_session(
        connection: &Connection,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        parent_id: Option<SessionId>,
    ) {
        connection
            .execute(
                "INSERT INTO sessions(
                     id, workspace_id, parent_id, title, status,
                     created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, ?3, 'accounting test', 'idle', 1, 1)",
                params![
                    session_id.to_string(),
                    workspace_id.to_string(),
                    parent_id.map(|id| id.to_string()),
                ],
            )
            .unwrap();
    }

    fn insert_accounting_run(
        connection: &Connection,
        session_id: SessionId,
        status: &str,
        usage: Option<TokenUsage>,
        cost: Option<u64>,
    ) -> RunId {
        let run_id = RunId::generate().unwrap();
        connection
            .execute(
                "INSERT INTO runs(
                     id, session_id, command_id, user_message_id,
                     assistant_message_id, status, outcome_json, usage_json,
                     estimated_cost_usd_nanos, created_at_ms, finished_at_ms
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8, 1, 2)",
                params![
                    run_id.to_string(),
                    session_id.to_string(),
                    CommandId::generate().unwrap().to_string(),
                    MessageId::generate().unwrap().to_string(),
                    MessageId::generate().unwrap().to_string(),
                    status,
                    usage.map(|usage| serde_json::to_string(&usage).unwrap()),
                    cost,
                ],
            )
            .unwrap();
        run_id
    }

    #[test]
    fn store_accounting_projects_direct_and_immediate_child_runs_after_restart_and_pruning() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let workspace_id = WorkspaceId::generate().unwrap();
        let parent_id = SessionId::generate().unwrap();
        let first_child_id = SessionId::generate().unwrap();
        let second_child_id = SessionId::generate().unwrap();
        let grandchild_id = SessionId::generate().unwrap();
        {
            let (connection, _) = open_database(&database_path).unwrap();
            connection
                .execute(
                    "INSERT INTO workspaces(id, path) VALUES (?1, '/accounting-test')",
                    [workspace_id.to_string()],
                )
                .unwrap();
            insert_accounting_session(&connection, workspace_id, parent_id, None);
            insert_accounting_session(&connection, workspace_id, first_child_id, Some(parent_id));
            insert_accounting_session(&connection, workspace_id, second_child_id, Some(parent_id));
            insert_accounting_session(
                &connection,
                workspace_id,
                grandchild_id,
                Some(first_child_id),
            );
            insert_accounting_run(
                &connection,
                parent_id,
                "completed",
                Some(usage(2, 3)),
                Some(5),
            );
            insert_accounting_run(
                &connection,
                first_child_id,
                "failed",
                Some(usage(7, 11)),
                Some(13),
            );
            insert_accounting_run(
                &connection,
                second_child_id,
                "cancelled",
                Some(usage(17, 19)),
                None,
            );
            insert_accounting_run(
                &connection,
                grandchild_id,
                "completed",
                Some(usage(23, 29)),
                Some(31),
            );
        }

        let (connection, _) = open_database(&database_path).unwrap();
        let parent = load_session_accounting(&connection, parent_id).unwrap();
        assert_eq!(
            parent.direct,
            AccountingTotal {
                usage: Some(usage(2, 3)),
                estimated_cost_usd_nanos: Some(5),
            }
        );
        assert_eq!(parent.inclusive.usage, Some(usage(26, 33)));
        assert_eq!(parent.inclusive.estimated_cost_usd_nanos, None);

        let first_child = load_session_accounting(&connection, first_child_id).unwrap();
        assert_eq!(first_child.direct.usage, Some(usage(7, 11)));
        assert_eq!(first_child.inclusive.usage, Some(usage(30, 40)));
        assert_eq!(first_child.inclusive.estimated_cost_usd_nanos, Some(44));

        connection
            .execute(
                "DELETE FROM runs WHERE session_id = ?1",
                [second_child_id.to_string()],
            )
            .unwrap();
        connection
            .execute(
                "DELETE FROM sessions WHERE id = ?1",
                [second_child_id.to_string()],
            )
            .unwrap();
        let pruned = load_session_accounting(&connection, parent_id).unwrap();
        assert_eq!(pruned.inclusive.usage, Some(usage(9, 14)));
        assert_eq!(pruned.inclusive.estimated_cost_usd_nanos, Some(18));
    }

    #[test]
    fn store_accounting_includes_measured_active_rows_and_keeps_alias_consistent() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let workspace_id = WorkspaceId::generate().unwrap();
        let session_id = SessionId::generate().unwrap();
        let (connection, _) = open_database(&database_path).unwrap();
        connection
            .execute(
                "INSERT INTO workspaces(id, path) VALUES (?1, '/unknown-accounting-test')",
                [workspace_id.to_string()],
            )
            .unwrap();
        insert_accounting_session(&connection, workspace_id, session_id, None);
        insert_accounting_run(
            &connection,
            session_id,
            "completed",
            Some(usage(2, 3)),
            Some(5),
        );
        let active_run = insert_accounting_run(&connection, session_id, "running", None, None);
        assert_eq!(
            load_session_accounting(&connection, session_id)
                .unwrap()
                .direct,
            AccountingTotal {
                usage: Some(usage(2, 3)),
                estimated_cost_usd_nanos: Some(5),
            }
        );

        connection
            .execute(
                "INSERT INTO model_turns(run_id, turn_ordinal, assistant_content_json)
                 VALUES (?1, 1, '[]')",
                [active_run.to_string()],
            )
            .unwrap();
        assert_eq!(
            load_session_accounting(&connection, session_id)
                .unwrap()
                .direct,
            AccountingTotal {
                usage: None,
                estimated_cost_usd_nanos: None,
            }
        );

        connection
            .execute(
                "UPDATE runs
                 SET usage_json = ?2, estimated_cost_usd_nanos = 13
                 WHERE session_id = ?1 AND status = 'running'",
                params![
                    session_id.to_string(),
                    serde_json::to_string(&usage(7, 11)).unwrap(),
                ],
            )
            .unwrap();
        let summary = load_session_summary(&connection, session_id).unwrap();
        assert_eq!(
            summary.accounting.unwrap().direct,
            AccountingTotal {
                usage: Some(usage(9, 14)),
                estimated_cost_usd_nanos: Some(18),
            }
        );
        assert_eq!(summary.estimated_cost_usd_nanos, Some(18));

        insert_accounting_run(&connection, session_id, "failed", None, None);
        assert_eq!(
            load_session_accounting(&connection, session_id)
                .unwrap()
                .direct,
            AccountingTotal {
                usage: None,
                estimated_cost_usd_nanos: None,
            }
        );
    }

    #[test]
    fn store_accounting_marks_malformed_persisted_usage_unknown() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let workspace_id = WorkspaceId::generate().unwrap();
        let session_id = SessionId::generate().unwrap();
        let (connection, _) = open_database(&database_path).unwrap();
        connection
            .execute(
                "INSERT INTO workspaces(id, path) VALUES (?1, '/malformed-accounting-test')",
                [workspace_id.to_string()],
            )
            .unwrap();
        insert_accounting_session(&connection, workspace_id, session_id, None);
        insert_accounting_run(
            &connection,
            session_id,
            "completed",
            Some(usage(2, 3)),
            Some(5),
        );
        connection
            .execute(
                "UPDATE runs SET usage_json = '{not-json' WHERE session_id = ?1",
                [session_id.to_string()],
            )
            .unwrap();

        let summary = load_session_summary(&connection, session_id).unwrap();
        assert_eq!(
            summary.accounting.unwrap().direct,
            AccountingTotal {
                usage: None,
                estimated_cost_usd_nanos: None,
            }
        );
    }

    #[test]
    fn deleting_child_persists_deleted_then_refreshed_parent_projection() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let workspace_id = WorkspaceId::generate().unwrap();
        let parent_id = SessionId::generate().unwrap();
        let child_id = SessionId::generate().unwrap();
        let command_id = CommandId::generate().unwrap();
        let (mut connection, store_id) = open_database(&database_path).unwrap();
        connection
            .execute(
                "INSERT INTO workspaces(id, path) VALUES (?1, '/delete-accounting-test')",
                [workspace_id.to_string()],
            )
            .unwrap();
        insert_accounting_session(&connection, workspace_id, parent_id, None);
        insert_accounting_session(&connection, workspace_id, child_id, Some(parent_id));
        insert_accounting_run(
            &connection,
            child_id,
            "completed",
            Some(usage(7, 11)),
            Some(13),
        );
        assert_eq!(
            load_session_accounting(&connection, parent_id)
                .unwrap()
                .inclusive
                .usage,
            Some(usage(7, 11))
        );

        let transaction = connection.transaction().unwrap();
        let committed_through = delete_idle_session(
            &transaction,
            store_id,
            workspace_id,
            child_id,
            command_id,
            17,
        )
        .unwrap();
        transaction.commit().unwrap();

        let events = read_events(&mut connection, workspace_id, 0, 10).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0].event,
            SessionEvent::SessionDeleted { session_id } if session_id == child_id
        ));
        let SessionEvent::SessionUpdated { session } = &events[1].event else {
            panic!("expected refreshed parent projection");
        };
        assert_eq!(events[1].session_id, parent_id);
        assert_eq!(events[1].cursor.sequence, events[0].cursor.sequence + 1);
        assert_eq!(committed_through.cursor, events[1].cursor);
        assert_eq!(
            session.accounting.unwrap().inclusive,
            AccountingTotal {
                usage: Some(usage(0, 0)),
                estimated_cost_usd_nanos: Some(0),
            }
        );
    }

    struct ScriptedLoader;

    impl RuntimeLoader for ScriptedLoader {
        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            Box::pin(async {
                Runtime::new(ScriptedProvider, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(runtime),
                        pricing: Some(ModelPricing {
                            input_usd_nanos_per_token: 1_000,
                            output_usd_nanos_per_token: 2_000,
                            cache_read_usd_nanos_per_token: Some(100),
                            cache_write_usd_nanos_per_token: Some(300),
                            context_tier: None,
                            provenance: "test".to_owned(),
                        }),
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct ScriptedProvider;

    impl Provider for ScriptedProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "hel".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "l".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "o".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed {
                    usage: Some(qq_provider::ProviderUsage {
                        input_tokens: 10,
                        cache_read_input_tokens: 2,
                        cache_write_input_tokens: 1,
                        output_tokens: 5,
                    }),
                }),
            ]))
        }
    }

    struct PricedHangingLoader;

    impl RuntimeLoader for PricedHangingLoader {
        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            Box::pin(async {
                Runtime::new(HangingProvider, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(runtime),
                        pricing: Some(ModelPricing {
                            input_usd_nanos_per_token: 1_000,
                            output_usd_nanos_per_token: 2_000,
                            cache_read_usd_nanos_per_token: Some(100),
                            cache_write_usd_nanos_per_token: Some(300),
                            context_tier: None,
                            provenance: "test".to_owned(),
                        }),
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct UsageSequenceLoader {
        usages: StdMutex<Vec<Option<qq_provider::ProviderUsage>>>,
    }

    impl RuntimeLoader for UsageSequenceLoader {
        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let usage = self.usages.lock().unwrap().remove(0);
            Box::pin(async move {
                Runtime::new(UsageSequenceProvider { usage }, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(runtime),
                        pricing: None,
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct UsageSequenceProvider {
        usage: Option<qq_provider::ProviderUsage>,
    }

    impl Provider for UsageSequenceProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "answer".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage: self.usage }),
            ]))
        }
    }

    struct ReasoningLoader {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    impl RuntimeLoader for ReasoningLoader {
        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let requests = Arc::clone(&self.requests);
            Box::pin(async move {
                Runtime::new(ReasoningProvider { requests }, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(runtime),
                        pricing: None,
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct ReasoningProvider {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    impl Provider for ReasoningProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::ReasoningStarted {
                    kind: qq_provider::ReasoningKind::Summary,
                }),
                Ok(qq_provider::ProviderEvent::ReasoningDelta {
                    kind: qq_provider::ReasoningKind::Summary,
                    text: "private rationale".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ReasoningCompleted {
                    kind: qq_provider::ReasoningKind::Summary,
                }),
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "answer".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
            ]))
        }
    }

    struct ChunkingLoader;

    impl RuntimeLoader for ChunkingLoader {
        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            Box::pin(async {
                Runtime::new(ChunkingProvider, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(runtime),
                        pricing: None,
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct ChunkingProvider;

    impl Provider for ChunkingProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: String::new(),
                }),
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "é".repeat(MAX_TEXT_CHUNK_BYTES / 2 + 8),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
            ]))
        }
    }

    struct CapturingLoader {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    impl RuntimeLoader for CapturingLoader {
        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let requests = Arc::clone(&self.requests);
            Box::pin(async move {
                Runtime::new(DelayedProvider { requests }, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(runtime),
                        pricing: None,
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct DelayedProvider {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    struct ToolLoopLoader {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    impl RuntimeLoader for ToolLoopLoader {
        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let requests = Arc::clone(&self.requests);
            Box::pin(async move {
                Runtime::new(ToolLoopProvider { requests }, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(runtime),
                        pricing: None,
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct ToolLoopProvider {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    impl Provider for ToolLoopProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut requests = self.requests.lock().unwrap();
            let turn = requests.len();
            requests.push(request);
            drop(requests);
            if turn == 0 {
                Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: "call_0".to_owned(),
                        name: "read_file".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: "call_0".to_owned(),
                        json: r#"{"path":"note.txt"}"#.to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                        id: "call_0".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed {
                        usage: Some(qq_provider::ProviderUsage {
                            input_tokens: 4,
                            cache_read_input_tokens: 0,
                            cache_write_input_tokens: 0,
                            output_tokens: 2,
                        }),
                    }),
                ]))
            } else {
                Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed {
                        usage: Some(qq_provider::ProviderUsage {
                            input_tokens: 6,
                            cache_read_input_tokens: 0,
                            cache_write_input_tokens: 0,
                            output_tokens: 1,
                        }),
                    }),
                ]))
            }
        }
    }

    struct RenewableSliceLoader {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        checkpoint_wait: Option<Arc<tokio::sync::Notify>>,
        metered_empty_checkpoint: bool,
    }

    impl RuntimeLoader for RenewableSliceLoader {
        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let requests = Arc::clone(&self.requests);
            let checkpoint_wait = self.checkpoint_wait.clone();
            let metered_empty_checkpoint = self.metered_empty_checkpoint;
            Box::pin(async move {
                Runtime::new(
                    RenewableSliceProvider {
                        requests,
                        checkpoint_wait,
                        metered_empty_checkpoint,
                    },
                    "test-model",
                    256,
                )
                .map(|runtime| LoadedRuntime {
                    runtime: Arc::new(runtime),
                    pricing: metered_empty_checkpoint.then_some(ModelPricing {
                        input_usd_nanos_per_token: 1_000,
                        output_usd_nanos_per_token: 2_000,
                        cache_read_usd_nanos_per_token: Some(100),
                        cache_write_usd_nanos_per_token: Some(300),
                        context_tier: None,
                        provenance: "test".to_owned(),
                    }),
                })
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
            })
        }
    }

    struct RenewableSliceProvider {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        checkpoint_wait: Option<Arc<tokio::sync::Notify>>,
        metered_empty_checkpoint: bool,
    }

    impl Provider for RenewableSliceProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut requests = self.requests.lock().unwrap();
            let current = requests.len();
            requests.push(request.clone());
            drop(requests);

            let tool_turns = crate::MAX_TOOL_CALLS_PER_SLICE / crate::MAX_TOOL_CALLS_PER_TURN;
            if request.tools().is_empty() {
                if let Some(checkpoint_wait) = &self.checkpoint_wait {
                    checkpoint_wait.notify_one();
                    return Box::pin(stream::pending());
                }
                if self.metered_empty_checkpoint {
                    return Box::pin(stream::iter([Ok(qq_provider::ProviderEvent::Completed {
                        usage: Some(qq_provider::ProviderUsage {
                            input_tokens: 3,
                            cache_read_input_tokens: 1,
                            cache_write_input_tokens: 2,
                            output_tokens: 5,
                        }),
                    })]));
                }
                return Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::OutputTextDelta {
                        text: "slice checkpoint".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                ]));
            }
            if current > tool_turns {
                return Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::OutputTextDelta {
                        text: "task complete".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                ]));
            }

            let mut events = Vec::with_capacity(crate::MAX_TOOL_CALLS_PER_TURN * 3 + 1);
            for index in 0..crate::MAX_TOOL_CALLS_PER_TURN {
                let id = format!("call-{current}-{index}");
                let (name, json) = if current == 0 && index == 0 {
                    ("read_file", r#"{"path":"slice-effects.txt"}"#)
                } else if current == 0 && index == 1 {
                    (
                        "edit_file",
                        r#"{"path":"slice-effects.txt","old_string":"seed","new_string":"seedx"}"#,
                    )
                } else {
                    ("read_file", r#"{"path":"note.txt"}"#)
                };
                events.push(Ok(qq_provider::ProviderEvent::ToolCallStarted {
                    id: id.clone(),
                    name: name.to_owned(),
                }));
                events.push(Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                    id: id.clone(),
                    json: json.to_owned(),
                }));
                events.push(Ok(qq_provider::ProviderEvent::ToolCallCompleted { id }));
            }
            events.push(Ok(qq_provider::ProviderEvent::Completed {
                usage: self
                    .metered_empty_checkpoint
                    .then_some(qq_provider::ProviderUsage {
                        input_tokens: 1,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                        output_tokens: 1,
                    }),
            }));
            Box::pin(stream::iter(events))
        }
    }

    /// Turn one streams text and then requests a tool call; turn two streams
    /// closing text. Exercises per-turn assistant messages around calls.
    struct TurnTextLoader {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    impl RuntimeLoader for TurnTextLoader {
        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let requests = Arc::clone(&self.requests);
            Box::pin(async move {
                Runtime::new(TurnTextProvider { requests }, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(runtime),
                        pricing: None,
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct TurnTextProvider {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    impl Provider for TurnTextProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut requests = self.requests.lock().unwrap();
            let turn = requests.len();
            requests.push(request);
            drop(requests);
            if turn == 0 {
                Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::OutputTextDelta {
                        text: "Let me look. ".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: "call_0".to_owned(),
                        name: "read_file".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: "call_0".to_owned(),
                        json: r#"{"path":"note.txt"}"#.to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                        id: "call_0".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                ]))
            } else {
                Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
    }

    struct RefusalProvider;

    impl Provider for RefusalProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::RefusalDelta {
                    text: "cannot complete that task".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
            ]))
        }
    }

    impl Provider for DelayedProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            Box::pin(async_stream! {
                tokio::time::sleep(Duration::from_millis(20)).await;
                yield Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "answer".to_owned(),
                });
                yield Ok(qq_provider::ProviderEvent::Completed { usage: None });
            })
        }
    }

    /// Requests `tool` once per tool turn, then completes with text.
    struct ApprovalLoader {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        tool: &'static str,
        arguments: &'static str,
        tool_turns: usize,
    }

    impl RuntimeLoader for ApprovalLoader {
        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let provider = ApprovalProvider {
                requests: Arc::clone(&self.requests),
                turn: StdMutex::new(0),
                tool: self.tool,
                arguments: self.arguments,
                tool_turns: self.tool_turns,
                usage: None,
            };
            Box::pin(async move {
                Runtime::new(provider, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(runtime),
                        pricing: None,
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct ApprovalProvider {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        turn: StdMutex<usize>,
        tool: &'static str,
        arguments: &'static str,
        tool_turns: usize,
        usage: Option<qq_provider::ProviderUsage>,
    }

    impl Provider for ApprovalProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            let mut current = self.turn.lock().unwrap();
            let turn = *current;
            *current += 1;
            drop(current);
            if turn < self.tool_turns {
                Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: format!("call_{turn}"),
                        name: self.tool.to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: format!("call_{turn}"),
                        json: self.arguments.to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                        id: format!("call_{turn}"),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed { usage: self.usage }),
                ]))
            } else {
                Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed { usage: self.usage }),
                ]))
            }
        }
    }

    /// Replays a fixed tool-call script per run: run N issues its scripted
    /// calls one per model turn, then completes with text.
    struct ScriptedRunsLoader {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        runs: Vec<Vec<(&'static str, String)>>,
        loads: StdMutex<usize>,
    }

    impl RuntimeLoader for ScriptedRunsLoader {
        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let mut loads = self.loads.lock().unwrap();
            let script = self.runs.get(*loads).cloned().unwrap_or_default();
            *loads += 1;
            drop(loads);
            let provider = ScriptedRunProvider {
                requests: Arc::clone(&self.requests),
                script,
                turn: StdMutex::new(0),
            };
            Box::pin(async move {
                Runtime::new(provider, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(runtime),
                        pricing: None,
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct ScriptedRunProvider {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        script: Vec<(&'static str, String)>,
        turn: StdMutex<usize>,
    }

    impl Provider for ScriptedRunProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            let mut turn = self.turn.lock().unwrap();
            let current = *turn;
            *turn += 1;
            drop(turn);
            match self.script.get(current) {
                Some((name, arguments)) => Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: format!("call_{current}"),
                        name: (*name).to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: format!("call_{current}"),
                        json: arguments.clone(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                        id: format!("call_{current}"),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                ])),
                None => Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                ])),
            }
        }
    }

    struct ScriptedRunsHarness {
        _directory: TempDir,
        runtime: SessionRuntime,
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        workspace_path: PathBuf,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        events: SessionEventStream,
    }

    async fn scripted_runs_harness(
        mode: ApprovalMode,
        runs: Vec<Vec<(&'static str, String)>>,
    ) -> ScriptedRunsHarness {
        scripted_runs_harness_with_authority(mode, runs, None).await
    }

    async fn scripted_runs_harness_with_authority(
        mode: ApprovalMode,
        runs: Vec<Vec<(&'static str, String)>>,
        grant_authority: Option<Arc<dyn WorkspaceGrantAuthority>>,
    ) -> ScriptedRunsHarness {
        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
        options.grant_authority = grant_authority;
        let runtime = SessionRuntime::open(
            options,
            Arc::new(ScriptedRunsLoader {
                requests: Arc::clone(&requests),
                runs,
                loads: StdMutex::new(0),
            }),
        )
        .await
        .unwrap();
        let workspace_path = directory.path().to_owned();
        let (workspace_id, _) = resolve_workspace(&runtime, &workspace_path).await;
        let created = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: mode,
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        ScriptedRunsHarness {
            _directory: directory,
            runtime,
            requests,
            workspace_path,
            workspace_id,
            session_id,
            events,
        }
    }

    async fn submit_prompt(harness: &ScriptedRunsHarness, prompt: &str) -> RunId {
        let queued = harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id: harness.session_id,
                    prompt: prompt.to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
            panic!("unexpected receipt")
        };
        run_id
    }

    struct ApprovalHarness {
        _directory: TempDir,
        runtime: SessionRuntime,
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        run_id: RunId,
        events: SessionEventStream,
    }

    async fn approval_harness(
        mode: ApprovalMode,
        tool: &'static str,
        arguments: &'static str,
        tool_turns: usize,
        approval_timeout: Duration,
    ) -> ApprovalHarness {
        approval_harness_with_authority(mode, tool, arguments, tool_turns, approval_timeout, None)
            .await
    }

    async fn approval_harness_with_authority(
        mode: ApprovalMode,
        tool: &'static str,
        arguments: &'static str,
        tool_turns: usize,
        approval_timeout: Duration,
        grant_authority: Option<Arc<dyn WorkspaceGrantAuthority>>,
    ) -> ApprovalHarness {
        approval_harness_with_reviewer(
            mode,
            tool,
            arguments,
            tool_turns,
            approval_timeout,
            grant_authority,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn approval_harness_with_reviewer(
        mode: ApprovalMode,
        tool: &'static str,
        arguments: &'static str,
        tool_turns: usize,
        approval_timeout: Duration,
        grant_authority: Option<Arc<dyn WorkspaceGrantAuthority>>,
        approval_reviewer: Option<Arc<dyn ApprovalReviewer>>,
    ) -> ApprovalHarness {
        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions {
                database_path: directory.path().join("sessions.sqlite3"),
                max_active_runs: 1,
                approval_timeout,
                grant_authority,
                approval_reviewer,
            },
            Arc::new(ApprovalLoader {
                requests: Arc::clone(&requests),
                tool,
                arguments,
                tool_turns,
            }),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: mode,
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        let queued = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "mutate something".to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
            panic!("unexpected receipt")
        };
        ApprovalHarness {
            _directory: directory,
            runtime,
            requests,
            workspace_id,
            session_id,
            run_id,
            events,
        }
    }

    async fn collect_until_approval_requested(
        events: &mut SessionEventStream,
    ) -> (Vec<SessionEventEnvelope>, ToolCallSnapshot) {
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut observed = Vec::new();
            loop {
                let event = events.next().await.unwrap().unwrap();
                let requested = match &event.event {
                    SessionEvent::ToolApprovalRequested { tool_call, .. } => {
                        Some(tool_call.clone())
                    }
                    _ => None,
                };
                observed.push(event);
                if let Some(tool_call) = requested {
                    return (observed, tool_call);
                }
            }
        })
        .await
        .unwrap()
    }

    async fn respond_approval(
        runtime: &SessionRuntime,
        run_id: RunId,
        tool_call_id: ToolCallId,
        decision: ApprovalDecision,
    ) -> Result<CommandReceipt, SessionRuntimeError> {
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::RespondToolApproval {
                    run_id,
                    tool_call_id,
                    decision,
                },
            )
            .await
    }

    async fn test_runtime() -> (TempDir, SessionRuntime) {
        let directory = tempfile::tempdir().unwrap();
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(ScriptedLoader),
        )
        .await
        .unwrap();
        (directory, runtime)
    }

    #[tokio::test]
    async fn run_snapshot_preserves_prompt_identity_across_restart() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("AGENTS.md"),
            "Keep provenance stable.\n",
        )
        .unwrap();
        let skill = directory.path().join(".qq/skills/stable");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "Retain exact run provenance.\n").unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(database_path.clone()),
            Arc::new(ScriptedLoader),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        let run_id = submit_prompt_to(&runtime, session_id, "/stable finish the work").await;
        collect_through_finished(&mut events).await;

        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 8,
                message_limit: 32,
            })
            .await
            .unwrap();
        let run = snapshot
            .focused
            .unwrap()
            .runs
            .into_iter()
            .find(|run| run.id == run_id)
            .unwrap();
        let prompt_identity = run
            .prompt_identity
            .expect("a sent prompt must retain its prompt identity");
        assert_eq!(prompt_identity.version, crate::AGENT_PROMPT_VERSION);
        assert_eq!(prompt_identity.instruction_hash.to_string().len(), 64);
        assert_eq!(
            prompt_identity
                .system_prompt_hash
                .expect("new runs must retain their full prompt hash")
                .to_string()
                .len(),
            64
        );
        assert_eq!(
            prompt_identity
                .tool_schema_hash
                .expect("new runs must retain their tool schema hash")
                .to_string()
                .len(),
            64
        );
        let guidance = prompt_identity
            .selected_guidance
            .as_deref()
            .expect("the selected skill must retain provenance");
        assert_eq!(guidance.kind, qq_protocol::GuidanceKind::Skill);
        assert_eq!(guidance.name, "stable");
        assert_eq!(guidance.source, ".qq/skills/stable/SKILL.md");
        assert_eq!(guidance.version, None);

        runtime.shutdown().await.unwrap();
        drop(runtime);
        let reopened = SessionRuntime::open(
            SessionRuntimeOptions::new(database_path),
            Arc::new(ScriptedLoader),
        )
        .await
        .unwrap();
        let snapshot = reopened
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 8,
                message_limit: 32,
            })
            .await
            .unwrap();
        let run = snapshot
            .focused
            .unwrap()
            .runs
            .into_iter()
            .find(|run| run.id == run_id)
            .unwrap();
        assert_eq!(run.prompt_identity, Some(prompt_identity));
        reopened.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn escaped_slash_is_persisted_as_the_provider_visible_prompt() {
        let mut harness =
            scripted_runs_harness(ApprovalMode::Ask, vec![Vec::new(), Vec::new()]).await;

        submit_prompt(&harness, "//review literally").await;
        let first_events = collect_through_finished(&mut harness.events).await;
        assert!(first_events.iter().any(|event| matches!(
            &event.event,
            SessionEvent::PromptQueued { message, .. }
                if message.output == "/review literally"
        )));
        submit_prompt(&harness, "follow up").await;
        collect_through_finished(&mut harness.events).await;

        {
            let requests = harness.requests.lock().unwrap();
            assert_eq!(
                requests[0].messages()[0],
                Message::user("/review literally")
            );
            assert_eq!(
                requests[1].messages()[0],
                Message::user("/review literally")
            );
            assert!(
                requests[1]
                    .messages()
                    .iter()
                    .all(|message| message != &Message::user("//review literally"))
            );
        }

        let snapshot = harness
            .runtime
            .snapshot(SnapshotRequest {
                workspace_id: harness.workspace_id,
                focused_session_id: Some(harness.session_id),
                session_limit: 8,
                message_limit: 32,
            })
            .await
            .unwrap()
            .focused
            .unwrap();
        let first_user = snapshot
            .messages
            .iter()
            .find(|message| message.role == qq_protocol::MessageRole::User)
            .unwrap();
        assert_eq!(first_user.output, "/review literally");
    }

    #[tokio::test]
    async fn prompt_identity_persistence_failure_starts_no_provider_work() {
        struct CountingLoader {
            provider_calls: Arc<AtomicUsize>,
        }

        impl RuntimeLoader for CountingLoader {
            fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
                let provider_calls = Arc::clone(&self.provider_calls);
                Box::pin(async move {
                    let runtime =
                        Runtime::new(CountingProvider { provider_calls }, "test-model", 256)
                            .map_err(|error| RuntimeLoadError {
                                kind: RunFailureKind::Configuration,
                                message: error.to_string(),
                            })?;
                    Ok(LoadedRuntime {
                        runtime: Arc::new(runtime),
                        pricing: None,
                    })
                })
            }
        }

        struct CountingProvider {
            provider_calls: Arc<AtomicUsize>,
        }

        impl Provider for CountingProvider {
            fn stream(&self, _request: ModelRequest) -> ProviderStream {
                self.provider_calls.fetch_add(1, Ordering::AcqRel);
                Box::pin(stream::iter([Ok(qq_provider::ProviderEvent::Completed {
                    usage: None,
                })]))
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let provider_calls = Arc::new(AtomicUsize::new(0));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(database_path.clone()),
            Arc::new(CountingLoader {
                provider_calls: Arc::clone(&provider_calls),
            }),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        Connection::open(&database_path)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_prompt_identity
                 BEFORE UPDATE OF prompt_identity_json ON runs
                 WHEN NEW.prompt_identity_json IS NOT NULL
                 BEGIN
                     SELECT RAISE(FAIL, 'forced prompt identity failure');
                 END;",
            )
            .unwrap();

        let run_id = submit_prompt_to(&runtime, session_id, "work").await;
        let observed = collect_through_finished(&mut events).await;

        assert_eq!(provider_calls.load(Ordering::Acquire), 0);
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished {
                run_id: finished,
                outcome: RunOutcome::Failed { failure },
                ..
            } if *finished == run_id
                && failure.kind == RunFailureKind::Server
                && failure.message.contains("failed to persist the run prompt identity")
        )));
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn instruction_hash_tracks_selected_path_and_bytes_deterministically() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("AGENTS.md"), "same\n").unwrap();
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(ScriptedLoader),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();

        let first =
            completed_instruction_hash(&runtime, workspace_id, session_id, &mut events, "first")
                .await;
        let second =
            completed_instruction_hash(&runtime, workspace_id, session_id, &mut events, "second")
                .await;
        assert_eq!(
            first,
            "6aba264a3fed8588d4e09f84ce073452fb551b53e8c3beae1b4aaf6bbb55a0c4"
        );
        assert_eq!(second, first);

        std::fs::remove_file(directory.path().join("AGENTS.md")).unwrap();
        std::fs::write(directory.path().join("CLAUDE.md"), "same\n").unwrap();
        let fallback =
            completed_instruction_hash(&runtime, workspace_id, session_id, &mut events, "fallback")
                .await;
        assert_eq!(
            fallback,
            "6d0da1256387dfa8d1521b942048efc90d6fa610b0d8f3e9d9dc7d0e4733d73e"
        );
        assert_ne!(fallback, first);

        std::fs::remove_file(directory.path().join("CLAUDE.md")).unwrap();
        let empty =
            completed_instruction_hash(&runtime, workspace_id, session_id, &mut events, "empty")
                .await;
        assert_eq!(
            empty,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        runtime.shutdown().await.unwrap();
    }

    /// Records the model each run is loaded with, then behaves like a
    /// one-tool-turn approval run: the run parks at a `__test_mutate`
    /// approval, which gives tests a deterministic "run is active" point.
    struct RecordingApprovalLoader {
        models: Arc<StdMutex<Vec<Option<String>>>>,
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    impl RuntimeLoader for RecordingApprovalLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            self.models.lock().unwrap().push(request.model.model);
            let provider = ApprovalProvider {
                requests: Arc::clone(&self.requests),
                turn: StdMutex::new(0),
                tool: "__test_mutate",
                arguments: "{}",
                tool_turns: 1,
                usage: Some(qq_provider::ProviderUsage {
                    input_tokens: 7,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    output_tokens: 1,
                }),
            };
            Box::pin(async move {
                Runtime::new(provider, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(runtime),
                        pricing: None,
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct SessionManagementHarness {
        directory: TempDir,
        runtime: SessionRuntime,
        models: Arc<StdMutex<Vec<Option<String>>>>,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        store_id: StoreId,
        events: SessionEventStream,
    }

    async fn session_management_harness() -> SessionManagementHarness {
        let directory = tempfile::tempdir().unwrap();
        let models = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(RecordingApprovalLoader {
                models: Arc::clone(&models),
                requests: Arc::new(StdMutex::new(Vec::new())),
            }),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        // These management tests park a run at its tool approval, so the
        // session must ask rather than auto-execute the scripted mutation.
        let created =
            create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::Ask).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        SessionManagementHarness {
            directory,
            runtime,
            models,
            workspace_id,
            session_id,
            store_id: created.committed_through.store_id,
            events,
        }
    }

    #[tokio::test]
    async fn set_session_model_applies_to_the_next_run_but_not_the_active_one() {
        let mut harness = session_management_harness().await;
        let queued = harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id: harness.session_id,
                    prompt: "first run".to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
            panic!("unexpected receipt")
        };
        // The run is now claimed and parked at its tool approval.
        let (before_switch, tool_call) =
            collect_until_approval_requested(&mut harness.events).await;
        assert!(before_switch.iter().any(|event| matches!(
            event.event,
            SessionEvent::RunContextUpdated {
                context_tokens: 7,
                ..
            }
        )));
        assert!(before_switch.iter().any(|event| matches!(
            event.event,
            SessionEvent::SessionContextUpdated {
                context_tokens: Some(7),
                ..
            }
        )));

        let receipt = harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SetSessionModel {
                    session_id: harness.session_id,
                    model: ModelSelection {
                        model: Some("test/model-b".to_owned()),
                        max_output_tokens: Some(512),
                        organization: None,
                    },
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            &receipt.outcome,
            CommandOutcome::SessionModelSet { session_id, model }
                if *session_id == harness.session_id
                    && model.model.as_deref() == Some("test/model-b")
        ));
        let updated = harness.events.next().await.unwrap().unwrap();
        assert!(matches!(
            &updated.event,
            SessionEvent::SessionUpdated { session }
                if session.model.as_deref() == Some("test/model-b")
                    && session.active_run_id == Some(run_id)
                    && session.context_tokens.is_none()
        ));

        respond_approval(
            &harness.runtime,
            run_id,
            tool_call.id,
            ApprovalDecision::Deny,
        )
        .await
        .unwrap();
        let finished_old_model = collect_through_finished(&mut harness.events).await;
        assert!(
            finished_old_model
                .iter()
                .any(|event| matches!(event.event, SessionEvent::RunContextUpdated { .. }))
        );
        assert!(
            finished_old_model
                .iter()
                .all(|event| !matches!(event.event, SessionEvent::SessionContextUpdated { .. }))
        );
        assert!(finished_old_model.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished {
                session: SessionSummary {
                    context_tokens: None,
                    ..
                },
                ..
            }
        )));

        let queued = harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id: harness.session_id,
                    prompt: "second run".to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
            panic!("unexpected receipt")
        };
        let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        respond_approval(
            &harness.runtime,
            run_id,
            tool_call.id,
            ApprovalDecision::Deny,
        )
        .await
        .unwrap();
        collect_through_finished(&mut harness.events).await;

        // The active run kept its claimed model; only the next run loads the
        // repointed one.
        assert_eq!(
            *harness.models.lock().unwrap(),
            vec![
                Some("test/model".to_owned()),
                Some("test/model-b".to_owned())
            ]
        );
        let snapshot = harness
            .runtime
            .snapshot(SnapshotRequest {
                workspace_id: harness.workspace_id,
                focused_session_id: Some(harness.session_id),
                session_limit: 1,
                message_limit: 8,
            })
            .await
            .unwrap();
        assert_eq!(snapshot.focused.unwrap().summary.context_tokens, Some(7));

        // The same validation as CreateSession applies.
        assert_eq!(
            harness
                .runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::SetSessionModel {
                        session_id: harness.session_id,
                        model: ModelSelection::default(),
                    },
                )
                .await
                .unwrap_err(),
            SessionRuntimeError::InvalidModelSelection
        );
    }

    #[tokio::test]
    async fn delete_session_is_refused_while_running_then_cascades_completely() {
        let mut harness = session_management_harness().await;
        let queued = harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id: harness.session_id,
                    prompt: "do work".to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
            panic!("unexpected receipt")
        };
        let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;

        // Refused while the run is active; the client cancels first.
        assert_eq!(
            harness
                .runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::DeleteSession {
                        session_id: harness.session_id,
                    },
                )
                .await
                .unwrap_err(),
            SessionRuntimeError::SessionActive
        );

        // Approving for the session also writes a grant row to cascade over.
        respond_approval(
            &harness.runtime,
            run_id,
            tool_call.id,
            ApprovalDecision::ApproveForSession {
                grant: ApprovalGrant::Tool {
                    name: "__test_mutate".to_owned(),
                },
            },
        )
        .await
        .unwrap();
        collect_through_finished(&mut harness.events).await;

        let command_id = CommandId::generate().unwrap();
        let command = SessionCommand::DeleteSession {
            session_id: harness.session_id,
        };
        let receipt = harness
            .runtime
            .command(command_id, command.clone())
            .await
            .unwrap();
        assert!(matches!(
            receipt.outcome,
            CommandOutcome::SessionDeleted { session_id } if session_id == harness.session_id
        ));
        // Idempotent: the retry returns the original durable receipt.
        assert_eq!(
            harness.runtime.command(command_id, command).await.unwrap(),
            receipt
        );

        // Every session-owned row is gone in one transaction; the event log
        // keeps its rows so replays stay gapless.
        let connection =
            Connection::open(harness.directory.path().join("sessions.sqlite3")).unwrap();
        for table in [
            "sessions",
            "runs",
            "messages",
            "tool_calls",
            "model_turns",
            "session_grants",
            "session_files",
            "session_compactions",
        ] {
            let count: u32 = connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table} must be empty after the delete");
        }
        let events: u32 = connection
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert!(events > 0, "the workspace event log is append-only");
        drop(connection);

        let snapshot = harness
            .runtime
            .snapshot(SnapshotRequest {
                workspace_id: harness.workspace_id,
                focused_session_id: None,
                session_limit: 8,
                message_limit: 8,
            })
            .await
            .unwrap();
        assert!(snapshot.sessions.is_empty());

        // A replay across the deletion stays contiguous and ends deleted.
        let mut replay = harness
            .runtime
            .subscribe(SubscribeRequest {
                workspace_id: harness.workspace_id,
                after: EventCursor {
                    store_id: harness.store_id,
                    workspace_id: harness.workspace_id,
                    sequence: 0,
                },
            })
            .unwrap();
        let mut expected_sequence = 0;
        loop {
            let event = tokio::time::timeout(Duration::from_secs(2), replay.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            expected_sequence += 1;
            assert_eq!(event.cursor.sequence, expected_sequence);
            if matches!(event.event, SessionEvent::SessionDeleted { session_id }
                if session_id == harness.session_id)
            {
                break;
            }
        }

        // Deleting again with a fresh command is an ordinary not-found.
        assert_eq!(
            harness
                .runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::DeleteSession {
                        session_id: harness.session_id,
                    },
                )
                .await
                .unwrap_err(),
            SessionRuntimeError::SessionNotFound
        );
    }

    #[tokio::test]
    async fn prune_deletes_only_idle_sessions_without_messages() {
        let (directory, runtime) = test_runtime().await;
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let kept = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id: kept } = kept.outcome else {
            panic!("unexpected receipt")
        };
        let mut empties = Vec::new();
        for _ in 0..2 {
            let CommandOutcome::SessionCreated { session_id } =
                create_session(&runtime, workspace_id, None).await.outcome
            else {
                panic!("unexpected receipt")
            };
            empties.push(session_id);
        }
        let queued = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id: kept,
                    prompt: "keep me".to_owned(),
                },
            )
            .await
            .unwrap();
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: queued.committed_through,
            })
            .unwrap();
        collect_through_finished(&mut events).await;

        let receipt = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::PruneSessions { workspace_id },
            )
            .await
            .unwrap();
        assert!(matches!(
            receipt.outcome,
            CommandOutcome::SessionsPruned { deleted: 2, .. }
        ));

        // One SessionDeleted per victim; the prompted session survives.
        let mut deleted = Vec::new();
        while deleted.len() < 2 {
            let event = tokio::time::timeout(Duration::from_secs(2), events.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if let SessionEvent::SessionDeleted { session_id } = event.event {
                deleted.push(session_id);
            }
        }
        assert_eq!(deleted, empties);
        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: None,
                session_limit: 8,
                message_limit: 8,
            })
            .await
            .unwrap();
        assert_eq!(
            snapshot
                .sessions
                .iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            vec![kept]
        );

        // A second prune finds nothing left to delete.
        let receipt = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::PruneSessions { workspace_id },
            )
            .await
            .unwrap();
        assert!(matches!(
            receipt.outcome,
            CommandOutcome::SessionsPruned { deleted: 0, .. }
        ));
    }

    #[test]
    fn version_one_migration_is_atomic_and_marks_historical_cost_unknown() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO metadata VALUES ('schema_version', '1');
                 CREATE TABLE workspaces (
                     id TEXT PRIMARY KEY, path TEXT NOT NULL UNIQUE,
                     next_sequence INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO workspaces VALUES ('workspace', '/workspace', 0);
                 CREATE TABLE sessions (
                     id TEXT PRIMARY KEY,
                     workspace_id TEXT NOT NULL REFERENCES workspaces(id),
                     parent_id TEXT REFERENCES sessions(id),
                     title TEXT NOT NULL, status TEXT NOT NULL, active_run_id TEXT,
                     queued_prompts INTEGER NOT NULL DEFAULT 0, model TEXT,
                     max_output_tokens INTEGER, organization TEXT,
                     created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
                 );
                 INSERT INTO sessions VALUES (
                     'old', 'workspace', NULL, 'Old', 'idle', NULL, 0,
                     'openai/gpt-test', 100, NULL, 1, 1
                 );
                 CREATE TABLE runs (
                     id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
                     command_id TEXT NOT NULL UNIQUE, user_message_id TEXT NOT NULL,
                     assistant_message_id TEXT NOT NULL, status TEXT NOT NULL,
                     cancel_requested INTEGER NOT NULL DEFAULT 0, outcome_json TEXT,
                     created_at_ms INTEGER NOT NULL, started_at_ms INTEGER,
                     finished_at_ms INTEGER
                 );",
            )
            .unwrap();
        drop(connection);

        let (connection, _) = open_database(&path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT value FROM metadata WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "14"
        );
        assert!(
            !connection
                .query_row(
                    "SELECT cost_known FROM sessions WHERE id = 'old'",
                    [],
                    |row| { row.get::<_, bool>(0) }
                )
                .unwrap()
        );
        assert!(has_column(&connection, "runs", "usage_json").unwrap());
        assert!(has_column(&connection, "tool_calls", "provider_call_id").unwrap());
        assert!(has_column(&connection, "model_turns", "assistant_content_json").unwrap());
        assert!(has_column(&connection, "model_turns", "model_json").unwrap());
        assert!(has_column(&connection, "model_turns", "usage_json").unwrap());
        assert!(has_column(&connection, "model_turns", "estimated_cost_usd_nanos").unwrap());
        assert!(has_column(&connection, "model_turns", "completed_at_ms").unwrap());
        assert!(has_column(&connection, "tool_calls", "approval_resolution").unwrap());
        assert!(has_column(&connection, "tool_calls", "display_json").unwrap());
        assert_eq!(
            connection
                .query_row(
                    "SELECT approval_mode FROM sessions WHERE id = 'old'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "ask"
        );
        assert!(has_column(&connection, "session_grants", "value").unwrap());
        assert!(has_column(&connection, "session_files", "content_hash").unwrap());
        assert!(has_column(&connection, "messages", "turn_ordinal").unwrap());
    }

    #[test]
    fn version_five_migration_defaults_existing_messages_to_turn_zero() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        {
            // A version-5 store whose messages table predates turn_ordinal,
            // holding one completed legacy run.
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                     INSERT INTO metadata VALUES ('schema_version', '5');
                     CREATE TABLE workspaces (
                         id TEXT PRIMARY KEY, path TEXT NOT NULL UNIQUE,
                         next_sequence INTEGER NOT NULL DEFAULT 0
                     );
                     INSERT INTO workspaces VALUES ('workspace', '/workspace', 0);
                     CREATE TABLE sessions (
                         id TEXT PRIMARY KEY,
                         workspace_id TEXT NOT NULL REFERENCES workspaces(id),
                         parent_id TEXT REFERENCES sessions(id),
                         title TEXT NOT NULL, status TEXT NOT NULL, active_run_id TEXT,
                         queued_prompts INTEGER NOT NULL DEFAULT 0, model TEXT,
                         max_output_tokens INTEGER, organization TEXT,
                         approval_mode TEXT NOT NULL DEFAULT 'ask',
                         estimated_cost_usd_nanos INTEGER NOT NULL DEFAULT 0,
                         cost_known INTEGER NOT NULL DEFAULT 1,
                         created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
                     );
                     INSERT INTO sessions VALUES (
                         'session', 'workspace', NULL, 'Old', 'idle', NULL, 0,
                         'openai/gpt-test', 100, NULL, 'ask', 0, 1, 1, 1
                     );
                     CREATE TABLE runs (
                         id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
                         command_id TEXT NOT NULL UNIQUE, user_message_id TEXT NOT NULL,
                         assistant_message_id TEXT NOT NULL, status TEXT NOT NULL,
                         cancel_requested INTEGER NOT NULL DEFAULT 0, outcome_json TEXT,
                         usage_json TEXT, estimated_cost_usd_nanos INTEGER,
                         created_at_ms INTEGER NOT NULL, started_at_ms INTEGER,
                         finished_at_ms INTEGER
                     );
                     INSERT INTO runs VALUES (
                         'run', 'session', 'command', 'user-message', 'assistant-message',
                         'completed', 0, NULL, NULL, NULL, 1, 1, 2
                     );
                     CREATE TABLE messages (
                         id TEXT PRIMARY KEY,
                         session_id TEXT NOT NULL REFERENCES sessions(id),
                         run_id TEXT NOT NULL REFERENCES runs(id),
                         ordinal INTEGER NOT NULL, role TEXT NOT NULL, state TEXT NOT NULL,
                         output TEXT NOT NULL DEFAULT '', refusal TEXT NOT NULL DEFAULT '',
                         created_at_ms INTEGER NOT NULL,
                         UNIQUE(session_id, ordinal)
                     );
                     INSERT INTO messages VALUES (
                         'user-message', 'session', 'run', 1, 'user', 'complete', 'hi', '', 1
                     );
                     INSERT INTO messages VALUES (
                         'assistant-message', 'session', 'run', 2, 'assistant', 'complete',
                         'hello', '', 1
                     );
                     CREATE TABLE tool_calls (
                         id TEXT PRIMARY KEY,
                         run_id TEXT NOT NULL REFERENCES runs(id),
                         turn_ordinal INTEGER NOT NULL,
                         call_ordinal INTEGER NOT NULL,
                         provider_call_id TEXT NOT NULL,
                         name TEXT NOT NULL,
                         arguments_json TEXT NOT NULL,
                         state TEXT NOT NULL,
                         result TEXT,
                         is_error INTEGER NOT NULL DEFAULT 0,
                         approval_resolution TEXT,
                         requested_at_ms INTEGER NOT NULL,
                         started_at_ms INTEGER,
                         resolved_at_ms INTEGER,
                         finished_at_ms INTEGER
                     );",
                )
                .unwrap();
        }

        let (connection, _) = open_database(&path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT value FROM metadata WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "14"
        );
        assert!(has_column(&connection, "tool_calls", "display_json").unwrap());
        let (turn_ordinal, output, state) = connection
            .query_row(
                "SELECT turn_ordinal, output, state FROM messages WHERE id = 'assistant-message'",
                [],
                |row| {
                    Ok((
                        row.get::<_, u16>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(turn_ordinal, 0);
        assert_eq!(output, "hello");
        assert_eq!(state, "complete");
    }

    #[test]
    fn version_six_migration_adds_the_display_column_and_keeps_existing_calls_bare() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        {
            // A version-6 store whose tool_calls table predates display_json,
            // holding one completed edit call.
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                     INSERT INTO metadata VALUES ('schema_version', '6');
                     CREATE TABLE tool_calls (
                         id TEXT PRIMARY KEY,
                         run_id TEXT NOT NULL,
                         turn_ordinal INTEGER NOT NULL,
                         call_ordinal INTEGER NOT NULL,
                         provider_call_id TEXT NOT NULL,
                         name TEXT NOT NULL,
                         arguments_json TEXT NOT NULL,
                         state TEXT NOT NULL,
                         result TEXT,
                         is_error INTEGER NOT NULL DEFAULT 0,
                         approval_resolution TEXT,
                         requested_at_ms INTEGER NOT NULL,
                         started_at_ms INTEGER,
                         resolved_at_ms INTEGER,
                         finished_at_ms INTEGER
                     );
                     INSERT INTO tool_calls VALUES (
                         'call', 'run', 1, 1, 'call_0', 'edit_file', '{}',
                         'completed', 'Edited note.txt: replaced 1 occurrence(s).',
                         0, NULL, 1, 1, NULL, 2
                     );",
                )
                .unwrap();
        }

        let (connection, _) = open_database(&path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT value FROM metadata WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "14"
        );
        let (display_json, result) = connection
            .query_row(
                "SELECT display_json, result FROM tool_calls WHERE id = 'call'",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(display_json, None);
        assert_eq!(
            result.as_deref(),
            Some("Edited note.txt: replaced 1 occurrence(s).")
        );
    }

    #[test]
    fn version_seven_migration_adds_compaction_storage_and_run_kinds() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        {
            // A version-7 store whose runs table predates internal run kinds
            // and that has no compaction storage.
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                     INSERT INTO metadata VALUES ('schema_version', '7');
                     CREATE TABLE runs (
                         id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                         command_id TEXT NOT NULL UNIQUE, user_message_id TEXT NOT NULL,
                         assistant_message_id TEXT NOT NULL, status TEXT NOT NULL,
                         cancel_requested INTEGER NOT NULL DEFAULT 0, outcome_json TEXT,
                         usage_json TEXT, estimated_cost_usd_nanos INTEGER,
                         created_at_ms INTEGER NOT NULL, started_at_ms INTEGER,
                         finished_at_ms INTEGER
                     );
                     INSERT INTO runs VALUES (
                         'run', 'session', 'command', 'user', 'assistant',
                         'completed', 0, NULL, NULL, NULL, 1, 1, 2
                     );",
                )
                .unwrap();
        }

        let (connection, _) = open_database(&path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT value FROM metadata WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "14"
        );
        assert!(has_column(&connection, "runs", "kind").unwrap());
        assert_eq!(
            connection
                .query_row("SELECT kind FROM runs WHERE id = 'run'", [], |row| row
                    .get::<_, String>(
                    0
                ))
                .unwrap(),
            "prompt"
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM session_compactions", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn version_ten_migration_adds_context_and_child_ownership_without_guessing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                     INSERT INTO metadata VALUES ('schema_version', '10');
                     CREATE TABLE workspaces (
                         id TEXT PRIMARY KEY, path TEXT NOT NULL UNIQUE,
                         next_sequence INTEGER NOT NULL DEFAULT 0
                     );
                     INSERT INTO workspaces VALUES ('workspace', '/workspace', 0);
                     CREATE TABLE sessions (
                         id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspaces(id),
                         parent_id TEXT REFERENCES sessions(id), title TEXT NOT NULL,
                         status TEXT NOT NULL, active_run_id TEXT,
                         queued_prompts INTEGER NOT NULL DEFAULT 0, model TEXT,
                         max_output_tokens INTEGER, organization TEXT,
                         approval_mode TEXT NOT NULL DEFAULT 'ask',
                         estimated_cost_usd_nanos INTEGER NOT NULL DEFAULT 0,
                         cost_known INTEGER NOT NULL DEFAULT 1,
                         created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
                     );
                     INSERT INTO sessions VALUES (
                         'session', 'workspace', NULL, 'Old', 'idle', NULL, 0,
                         'openai/gpt-test', 100, NULL, 'ask', 0, 1, 1, 1
                     );",
                )
                .unwrap();
        }

        let (connection, _) = open_database(&path).unwrap();

        assert_eq!(
            connection
                .query_row(
                    "SELECT value FROM metadata WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "14"
        );
        assert!(has_column(&connection, "sessions", "context_tokens").unwrap());
        assert!(has_column(&connection, "sessions", "owner_run_id").unwrap());
        assert_eq!(
            connection
                .query_row(
                    "SELECT context_tokens FROM sessions WHERE id = 'session'",
                    [],
                    |row| row.get::<_, Option<u64>>(0),
                )
                .unwrap(),
            None
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT owner_run_id FROM sessions WHERE id = 'session'",
                    [],
                    |row| row.get::<_, Option<String>>(0),
                )
                .unwrap(),
            None
        );
        connection
            .execute(
                "UPDATE sessions SET context_tokens = 12_500 WHERE id = 'session'",
                [],
            )
            .unwrap();
        drop(connection);

        let (reopened, _) = open_database(&path).unwrap();
        assert_eq!(
            reopened
                .query_row(
                    "SELECT context_tokens FROM sessions WHERE id = 'session'",
                    [],
                    |row| row.get::<_, Option<u64>>(0),
                )
                .unwrap(),
            Some(12_500)
        );
    }

    #[test]
    fn version_eleven_migration_adds_child_ownership_and_preserves_context() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                     INSERT INTO metadata VALUES ('schema_version', '11');
                     CREATE TABLE workspaces (
                         id TEXT PRIMARY KEY, path TEXT NOT NULL UNIQUE,
                         next_sequence INTEGER NOT NULL DEFAULT 0
                     );
                     INSERT INTO workspaces VALUES ('workspace', '/workspace', 0);
                     CREATE TABLE sessions (
                         id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspaces(id),
                         parent_id TEXT REFERENCES sessions(id), title TEXT NOT NULL,
                         status TEXT NOT NULL, active_run_id TEXT,
                         queued_prompts INTEGER NOT NULL DEFAULT 0, model TEXT,
                         max_output_tokens INTEGER, organization TEXT,
                         approval_mode TEXT NOT NULL DEFAULT 'ask', context_tokens INTEGER,
                         estimated_cost_usd_nanos INTEGER NOT NULL DEFAULT 0,
                         cost_known INTEGER NOT NULL DEFAULT 1,
                         created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
                     );
                     INSERT INTO sessions VALUES (
                         'session', 'workspace', NULL, 'Existing', 'idle', NULL, 0,
                         'openai/gpt-test', 100, NULL, 'ask', 777, 0, 1, 1, 1
                     );",
                )
                .unwrap();
        }

        let (connection, _) = open_database(&path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT value FROM metadata WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "14"
        );
        assert!(has_column(&connection, "sessions", "owner_run_id").unwrap());
        assert_eq!(
            connection
                .query_row(
                    "SELECT context_tokens, owner_run_id FROM sessions WHERE id = 'session'",
                    [],
                    |row| Ok((row.get::<_, u64>(0)?, row.get::<_, Option<String>>(1)?)),
                )
                .unwrap(),
            (777, None)
        );
    }

    #[test]
    fn version_twelve_migration_adds_prompt_identity_without_guessing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                     INSERT INTO metadata VALUES ('schema_version', '12');
                     CREATE TABLE runs (
                         id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                         command_id TEXT NOT NULL UNIQUE, user_message_id TEXT NOT NULL,
                         assistant_message_id TEXT NOT NULL, status TEXT NOT NULL,
                         kind TEXT NOT NULL DEFAULT 'prompt',
                         auto_compaction INTEGER NOT NULL DEFAULT 0,
                         cancel_requested INTEGER NOT NULL DEFAULT 0,
                         outcome_json TEXT, usage_json TEXT, context_tokens INTEGER,
                         estimated_cost_usd_nanos INTEGER, created_at_ms INTEGER NOT NULL,
                         started_at_ms INTEGER, finished_at_ms INTEGER
                     );
                     INSERT INTO runs(
                         id, session_id, command_id, user_message_id,
                         assistant_message_id, status, created_at_ms, finished_at_ms
                     ) VALUES (
                         'run', 'session', 'command', 'user', 'assistant',
                         'completed', 1, 2
                     );",
                )
                .unwrap();
        }

        let (connection, _) = open_database(&path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT value FROM metadata WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "14"
        );
        assert!(has_column(&connection, "runs", "prompt_identity_json").unwrap());
        assert_eq!(
            connection
                .query_row(
                    "SELECT prompt_identity_json FROM runs WHERE id = 'run'",
                    [],
                    |row| row.get::<_, Option<String>>(0),
                )
                .unwrap(),
            None
        );
    }

    #[test]
    fn version_thirteen_migration_adds_per_turn_audit_columns() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO metadata VALUES ('schema_version', '13');
                 CREATE TABLE model_turns (
                     run_id TEXT NOT NULL,
                     turn_ordinal INTEGER NOT NULL,
                     assistant_content_json TEXT NOT NULL,
                     PRIMARY KEY(run_id, turn_ordinal)
                 );",
            )
            .unwrap();
        drop(connection);

        let (connection, _) = open_database(&path).unwrap();

        assert_eq!(
            connection
                .query_row(
                    "SELECT value FROM metadata WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "14"
        );
        for column in [
            "model_json",
            "usage_json",
            "estimated_cost_usd_nanos",
            "completed_at_ms",
        ] {
            assert!(
                has_column(&connection, "model_turns", column).unwrap(),
                "{column}"
            );
        }
    }

    #[test]
    fn version_fourteen_store_missing_audit_columns_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO metadata VALUES ('schema_version', '14');
                 CREATE TABLE model_turns (
                     run_id TEXT NOT NULL,
                     turn_ordinal INTEGER NOT NULL,
                     assistant_content_json TEXT NOT NULL,
                     PRIMARY KEY(run_id, turn_ordinal)
                 );",
            )
            .unwrap();
        drop(connection);

        assert_eq!(
            open_database(&path).unwrap_err(),
            SessionRuntimeError::Persistence
        );
    }

    #[test]
    fn assembly_pruning_stubs_old_read_only_results_and_preserves_errors() {
        let call = |id: &str, name: &str| ContentBlock::ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: serde_json::json!({"path": "src/lib.rs"}),
        };
        let result = |id: &str, is_error: bool| ContentBlock::ToolResult {
            call_id: id.to_owned(),
            content: "y".repeat(500),
            is_error,
        };
        let mut context = vec![
            Message::user("start"),
            Message::new(Role::Assistant, vec![call("c1", "read_file")]),
            Message::tool_results(vec![result("c1", false)]),
            Message::new(Role::Assistant, vec![call("c2", "shell")]),
            Message::tool_results(vec![result("c2", false)]),
            Message::new(Role::Assistant, vec![call("c3", "read_file")]),
            Message::tool_results(vec![result("c3", true)]),
            // The recency window: the last four model turns stay verbatim.
            Message::new(Role::Assistant, vec![call("c4", "read_file")]),
            Message::tool_results(vec![result("c4", false)]),
            Message::assistant("a"),
            Message::assistant("b"),
            Message::assistant("c"),
        ];
        let before = context_bytes(&context);

        prune_stale_tool_results(&mut context);

        let results = context
            .iter()
            .flat_map(Message::content)
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    call_id,
                    content,
                    is_error,
                } => Some((call_id.as_str(), content.as_str(), *is_error)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            results[0],
            (
                "c1",
                "[pruned: read_file {\"path\":\"src/lib.rs\"} returned 500 bytes; \
                 call it again if needed]",
                false
            )
        );
        // Shell output is not re-derivable; it survives outside the window.
        assert_eq!(results[1].0, "c2");
        assert!(results[1].1.starts_with("yyy"));
        // Errors prune to error stubs: content stubbed, is_error preserved.
        assert!(results[2].1.starts_with("[pruned: read_file"));
        assert!(results[2].2, "the error flag must survive pruning");
        // Inside the window everything stays verbatim.
        assert!(results[3].1.starts_with("yyy"));
        assert!(context_bytes(&context) < before);
    }

    #[test]
    fn capacity_accounting_measures_the_pruned_assembly_not_raw_rows() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let (mut connection, store_id) = open_database(&path).unwrap();
        let workspace_id = WorkspaceId::from_bytes([1; 16]);
        let session_id = SessionId::from_bytes([2; 16]);
        let run_id = RunId::from_bytes([3; 16]);
        connection
            .execute(
                "INSERT INTO workspaces(id, path, next_sequence) VALUES (?1, '/w', 0)",
                [workspace_id.to_string()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO sessions(id, workspace_id, title, status, model,
                                      created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, 'S', 'idle', 'test/model', 1, 1)",
                params![session_id.to_string(), workspace_id.to_string()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO runs(id, session_id, command_id, user_message_id,
                                  assistant_message_id, status, kind, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'completed', 'prompt', 1)",
                params![
                    run_id.to_string(),
                    session_id.to_string(),
                    CommandId::from_bytes([4; 16]).to_string(),
                    MessageId::from_bytes([5; 16]).to_string(),
                    MessageId::from_bytes([6; 16]).to_string(),
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO messages(id, session_id, run_id, ordinal, role, state,
                                      output, created_at_ms)
                 VALUES (?1, ?2, ?3, 1, 'user', 'complete', 'hi', 1)",
                params![
                    MessageId::from_bytes([5; 16]).to_string(),
                    session_id.to_string(),
                    run_id.to_string(),
                ],
            )
            .unwrap();
        // Two early read_file turns whose results total ~6 MiB of stored
        // rows — well over the 4 MiB budget — followed by four text turns
        // that push them out of the recency window.
        for (turn, provider_id) in [(1, "c1"), (2, "c2")] {
            connection
                .execute(
                    "INSERT INTO model_turns(run_id, turn_ordinal, assistant_content_json)
                     VALUES (?1, ?2, ?3)",
                    params![
                        run_id.to_string(),
                        turn,
                        format!(
                            "[{{\"type\":\"tool_call\",\"id\":\"{provider_id}\",\
                             \"name\":\"read_file\",\"arguments\":{{\"path\":\"big.txt\"}}}}]"
                        ),
                    ],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO tool_calls(id, run_id, turn_ordinal, call_ordinal,
                                            provider_call_id, name, arguments_json, state,
                                            result, is_error, requested_at_ms)
                     VALUES (?1, ?2, ?3, 0, ?4, 'read_file', '{\"path\":\"big.txt\"}',
                             'completed', ?5, 0, 1)",
                    params![
                        format!("call-{provider_id}"),
                        run_id.to_string(),
                        turn,
                        provider_id,
                        "z".repeat(3 * 1024 * 1024),
                    ],
                )
                .unwrap();
        }
        for turn in 3..=6 {
            connection
                .execute(
                    "INSERT INTO model_turns(run_id, turn_ordinal, assistant_content_json)
                     VALUES (?1, ?2, '[{\"type\":\"text\",\"text\":\"ok\"}]')",
                    params![run_id.to_string(), turn],
                )
                .unwrap();
        }

        let transaction = connection.transaction().unwrap();
        let assembled = assembled_context_bytes(&transaction, session_id).unwrap();
        drop(transaction);
        assert!(
            assembled < 64 * 1024,
            "stale results must assemble as stubs, got {assembled} bytes"
        );

        // A prompt fits because the budget measures the pruned assembly, not
        // the ~6 MiB of stored result rows.
        let applied = execute_command(
            &mut connection,
            store_id,
            CommandId::from_bytes([9; 16]),
            SessionCommand::SubmitPrompt {
                session_id,
                prompt: "continue".to_owned(),
            },
            &WorkspaceGrantSeed::default(),
        )
        .unwrap();
        assert!(matches!(
            applied.receipt.outcome,
            CommandOutcome::PromptQueued { .. }
        ));
    }

    #[test]
    fn interrupted_compaction_commits_no_marker_and_can_be_retried() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let (mut connection, store_id) = open_database(&path).unwrap();
        let workspace_id = WorkspaceId::from_bytes([1; 16]);
        let session_id = SessionId::from_bytes([2; 16]);
        let run_id = RunId::from_bytes([3; 16]);
        connection
            .execute(
                "INSERT INTO workspaces(id, path, next_sequence) VALUES (?1, '/w', 0)",
                [workspace_id.to_string()],
            )
            .unwrap();
        // A compaction run crashed mid-summarization: still marked running,
        // and — because summary and marker commit atomically with the run's
        // completion — no session_compactions row exists.
        connection
            .execute(
                "INSERT INTO sessions(id, workspace_id, title, status, active_run_id,
                                      model, created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, 'S', 'running', ?3, 'test/model', 1, 1)",
                params![
                    session_id.to_string(),
                    workspace_id.to_string(),
                    run_id.to_string(),
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO runs(id, session_id, command_id, user_message_id,
                                  assistant_message_id, status, kind, created_at_ms,
                                  started_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'running', 'compaction', 1, 1)",
                params![
                    run_id.to_string(),
                    session_id.to_string(),
                    CommandId::from_bytes([4; 16]).to_string(),
                    MessageId::from_bytes([5; 16]).to_string(),
                    MessageId::from_bytes([6; 16]).to_string(),
                ],
            )
            .unwrap();

        recover_interrupted_runs(&mut connection, store_id).unwrap();

        assert_eq!(
            connection
                .query_row(
                    "SELECT status FROM runs WHERE id = ?1",
                    [run_id.to_string()],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "interrupted"
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM session_compactions", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            0,
            "a crashed compaction must leave no marker"
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT status FROM sessions WHERE id = ?1",
                    [session_id.to_string()],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "idle"
        );

        // The command can simply be retried.
        let applied = execute_command(
            &mut connection,
            store_id,
            CommandId::from_bytes([9; 16]),
            SessionCommand::CompactSession { session_id },
            &WorkspaceGrantSeed::default(),
        )
        .unwrap();
        assert!(matches!(
            applied.receipt.outcome,
            CommandOutcome::CompactionQueued { session_id: queued, .. }
                if queued == session_id
        ));
    }

    #[test]
    fn cost_uses_the_context_tier_and_cache_rates() {
        let pricing = ModelPricing {
            input_usd_nanos_per_token: 1,
            output_usd_nanos_per_token: 2,
            cache_read_usd_nanos_per_token: Some(1),
            cache_write_usd_nanos_per_token: Some(2),
            context_tier: Some(qq_protocol::ModelPricingTier {
                above_input_tokens: 10,
                input_usd_nanos_per_token: 10,
                output_usd_nanos_per_token: 20,
                cache_read_usd_nanos_per_token: Some(3),
                cache_write_usd_nanos_per_token: Some(4),
            }),
            provenance: "test".to_owned(),
        };
        assert_eq!(
            run_cost(
                TokenUsage {
                    input_tokens: 8,
                    cache_read_input_tokens: 2,
                    cache_write_input_tokens: 1,
                    output_tokens: 3,
                },
                &pricing,
            ),
            Some(8 * 10 + 2 * 3 + 4 + 3 * 20)
        );
    }

    #[test]
    fn prompt_titles_are_compact_and_bounded() {
        assert_eq!(
            prompt_title("  Fix the login\n\tredirect  "),
            "Fix the login redirect"
        );
        assert_eq!(
            prompt_title(&"x".repeat(49)),
            format!("{}...", "x".repeat(48))
        );
        assert_eq!(prompt_title("\0\u{1b}\u{202e}\u{2066}"), "New session");
    }

    async fn resolve_workspace(
        runtime: &SessionRuntime,
        path: &std::path::Path,
    ) -> (WorkspaceId, EventCursor) {
        let receipt = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::ResolveWorkspace {
                    path: path.to_str().unwrap().to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::WorkspaceResolved { workspace_id } = receipt.outcome else {
            panic!("unexpected receipt")
        };
        (workspace_id, receipt.committed_through)
    }

    async fn create_session(
        runtime: &SessionRuntime,
        workspace_id: WorkspaceId,
        parent_id: Option<SessionId>,
    ) -> CommandReceipt {
        create_session_with_mode(runtime, workspace_id, parent_id, ApprovalMode::default()).await
    }

    async fn create_session_with_mode(
        runtime: &SessionRuntime,
        workspace_id: WorkspaceId,
        parent_id: Option<SessionId>,
        approval_mode: ApprovalMode,
    ) -> CommandReceipt {
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id,
                    model: ModelSelection {
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode,
                },
            )
            .await
            .unwrap()
    }

    async fn collect_through_finished(
        events: &mut SessionEventStream,
    ) -> Vec<SessionEventEnvelope> {
        // Generous upper bound: the auto-compaction tests stream multi-MiB
        // outputs concurrently, which can starve lighter tests of CPU.
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut observed = Vec::new();
            while let Some(event) = events.next().await {
                let event = event.unwrap();
                let finished = matches!(event.event, SessionEvent::RunFinished { .. });
                observed.push(event);
                if finished {
                    break;
                }
            }
            observed
        })
        .await
        .unwrap()
    }

    /// Collects events until the compaction commits (`SessionCompacted`),
    /// which is published after the internal run's `RunFinished`.
    async fn collect_through_compacted(
        events: &mut SessionEventStream,
    ) -> Vec<SessionEventEnvelope> {
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut observed = Vec::new();
            while let Some(event) = events.next().await {
                let event = event.unwrap();
                let compacted = matches!(event.event, SessionEvent::SessionCompacted { .. });
                observed.push(event);
                if compacted {
                    break;
                }
            }
            observed
        })
        .await
        .unwrap()
    }

    async fn compact_session(runtime: &SessionRuntime, session_id: SessionId) -> RunId {
        let receipt = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CompactSession { session_id },
            )
            .await
            .unwrap();
        let CommandOutcome::CompactionQueued { run_id, .. } = receipt.outcome else {
            panic!("unexpected receipt")
        };
        run_id
    }

    /// The concatenated text of each message in a captured provider request.
    fn request_texts(request: &ModelRequest) -> Vec<String> {
        request
            .messages()
            .iter()
            .map(|message| {
                message
                    .content()
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>()
            })
            .collect()
    }

    fn assert_tool_results_are_exact(messages: &[Message]) {
        let mut calls = HashMap::<String, Vec<usize>>::new();
        let mut results = HashMap::<String, Vec<usize>>::new();
        for (message_index, message) in messages.iter().enumerate() {
            for block in message.content() {
                match block {
                    ContentBlock::ToolCall { id, .. } => {
                        calls.entry(id.clone()).or_default().push(message_index);
                    }
                    ContentBlock::ToolResult { call_id, .. } => {
                        results
                            .entry(call_id.clone())
                            .or_default()
                            .push(message_index);
                    }
                    ContentBlock::Text { .. } => {}
                }
            }
        }
        for (id, call_positions) in &calls {
            assert_eq!(call_positions.len(), 1, "duplicate ToolCall for {id}");
            let result_positions = results.get(id).unwrap_or_else(|| {
                panic!("missing ToolResult for {id}");
            });
            assert_eq!(result_positions.len(), 1, "duplicate ToolResult for {id}");
            assert!(
                result_positions[0] > call_positions[0],
                "ToolResult for {id} must follow its ToolCall"
            );
        }
        for id in results.keys() {
            assert!(calls.contains_key(id), "orphaned ToolResult for {id}");
        }
    }

    #[tokio::test]
    async fn compact_session_is_refused_while_a_run_is_active_and_runs_toolless_after() {
        let mut harness = session_management_harness().await;
        let queued = harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id: harness.session_id,
                    prompt: "do work".to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
            panic!("unexpected receipt")
        };
        let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;

        // Idle-only: refused with the same error DeleteSession uses while a
        // run is active.
        assert_eq!(
            harness
                .runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::CompactSession {
                        session_id: harness.session_id,
                    },
                )
                .await
                .unwrap_err(),
            SessionRuntimeError::SessionActive
        );

        respond_approval(
            &harness.runtime,
            run_id,
            tool_call.id,
            ApprovalDecision::Deny,
        )
        .await
        .unwrap();
        collect_through_finished(&mut harness.events).await;

        // Idle now: the compaction queues and executes through the ordinary
        // machinery. The provider requests a tool on its first turn, but
        // internal runs deny every call without persisting or prompting.
        let compaction_run = compact_session(&harness.runtime, harness.session_id).await;
        let observed = collect_through_compacted(&mut harness.events).await;
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
                if *run_id == compaction_run
        )));
        assert!(
            !observed.iter().any(|event| matches!(
                &event.event,
                SessionEvent::PromptQueued { .. }
                    | SessionEvent::AssistantMessageStarted { .. }
                    | SessionEvent::ToolCallRequested { .. }
                    | SessionEvent::ToolApprovalRequested { .. }
            )),
            "an internal run must publish no transcript or tool events"
        );
        // The summarizer loaded through the ordinary loader path.
        assert_eq!(harness.models.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn compaction_runs_account_usage_and_cost_but_join_no_transcript() {
        let (directory, runtime) = test_runtime().await;
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "say hello".to_owned(),
                },
            )
            .await
            .unwrap();
        collect_through_finished(&mut events).await;
        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 8,
                message_limit: 32,
            })
            .await
            .unwrap();
        let focused = snapshot.focused.unwrap();
        assert_eq!(focused.messages.len(), 2);
        assert_eq!(focused.summary.context_tokens, Some(13));
        let cost_before = focused.summary.estimated_cost_usd_nanos.unwrap();

        let compaction_run = compact_session(&runtime, session_id).await;
        let observed = collect_through_compacted(&mut events).await;

        // Usage and cost account like any run.
        let usage = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::RunFinished { run_id, usage, .. } if *run_id == compaction_run => {
                    Some(*usage)
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(
            usage,
            Some(TokenUsage {
                input_tokens: 10,
                cache_read_input_tokens: 2,
                cache_write_input_tokens: 1,
                output_tokens: 5,
            })
        );
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ModelTurnCompleted {
                run_id,
                turn_ordinal: 1,
                model: ModelSelection { model: Some(model), .. },
                usage: Some(TokenUsage { input_tokens: 10, output_tokens: 5, .. }),
                estimated_cost_usd_nanos: Some(20_500),
            } if *run_id == compaction_run && model == "test/model"
        )));
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished {
                session: SessionSummary {
                    context_tokens: None,
                    ..
                },
                run_id,
                context_tokens: Some(13),
                ..
            } if *run_id == compaction_run
        )));
        let (before_bytes, after_bytes, summary_excerpt, context_tokens) = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::SessionCompacted {
                    session,
                    before_bytes,
                    after_bytes,
                    summary,
                } => Some((
                    *before_bytes,
                    *after_bytes,
                    summary.clone(),
                    session.context_tokens,
                )),
                _ => None,
            })
            .unwrap();
        assert!(before_bytes > 0);
        assert!(after_bytes > 0);
        assert_eq!(summary_excerpt.as_deref(), Some("hello"));
        assert_eq!(context_tokens, None);

        // The transcript is untouched: no new message rows, one more run,
        // cost increased, session idle again.
        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 8,
                message_limit: 32,
            })
            .await
            .unwrap();
        let focused = snapshot.focused.unwrap();
        assert_eq!(focused.messages.len(), 2);
        assert_eq!(focused.runs.len(), 2);
        assert_eq!(focused.summary.status, SessionStatus::Idle);
        assert_eq!(focused.summary.context_tokens, None);
        assert_eq!(focused.runs[1].context_tokens, Some(13));
        assert!(focused.summary.estimated_cost_usd_nanos.unwrap() > cost_before);
    }

    #[tokio::test]
    async fn assembly_after_compaction_is_summary_plus_verbatim_span_and_recompaction_folds() {
        let mut harness = scripted_runs_harness(ApprovalMode::Ask, vec![]).await;
        submit_prompt(&harness, "first prompt").await;
        collect_through_finished(&mut harness.events).await;

        compact_session(&harness.runtime, harness.session_id).await;
        collect_through_compacted(&mut harness.events).await;
        {
            // The summarization request is the assembled context plus the
            // fixed instruction, with the file list seeded mechanically.
            let requests = harness.requests.lock().unwrap();
            let texts = request_texts(&requests[1]);
            assert!(texts.iter().any(|text| text == "first prompt"));
            let instruction = texts.last().unwrap();
            assert!(instruction.starts_with("Summarize this conversation"));
            assert!(instruction.contains("Files touched"));
            assert!(instruction.contains("(none recorded)"));
        }

        submit_prompt(&harness, "second prompt").await;
        collect_through_finished(&mut harness.events).await;
        {
            // Assembly is now summary + verbatim span after the marker; the
            // original prompt survives only inside the summary.
            let requests = harness.requests.lock().unwrap();
            let texts = request_texts(&requests[2]);
            assert!(texts[0].starts_with(COMPACTION_SUMMARY_PREAMBLE));
            assert_eq!(texts[1], "second prompt");
            assert!(!texts.iter().any(|text| text == "first prompt"));
        }

        // Recompaction summarizes the prior summary plus the span since.
        compact_session(&harness.runtime, harness.session_id).await;
        collect_through_compacted(&mut harness.events).await;
        {
            let requests = harness.requests.lock().unwrap();
            let texts = request_texts(&requests[3]);
            assert!(texts[0].starts_with(COMPACTION_SUMMARY_PREAMBLE));
            assert!(texts.iter().any(|text| text == "second prompt"));
            assert!(
                texts
                    .last()
                    .unwrap()
                    .starts_with("Summarize this conversation")
            );
        }

        submit_prompt(&harness, "third prompt").await;
        collect_through_finished(&mut harness.events).await;
        {
            // Only the newest summary replays; prior summaries are folded in,
            // not stacked.
            let requests = harness.requests.lock().unwrap();
            let texts = request_texts(requests.last().unwrap());
            assert_eq!(texts.len(), 2);
            assert!(texts[0].starts_with(COMPACTION_SUMMARY_PREAMBLE));
            assert_eq!(texts[1], "third prompt");
        }

        // Bounded history: both compactions are retained for future rollback.
        let connection = Connection::open(harness.workspace_path.join("sessions.sqlite3")).unwrap();
        let compactions: u32 = connection
            .query_row("SELECT COUNT(*) FROM session_compactions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(compactions, 2);
    }

    /// One scripted model load for the auto-compaction tests: what the
    /// provider streams for that run.
    #[derive(Clone)]
    enum AutoCompactScript {
        /// Streams the text and completes.
        Text(String),
        /// Fails the model stream with a transport error.
        Fail,
        /// Fails the model stream with a context-window overflow.
        ContextOverflow,
        /// Never yields: the run parks until cancelled.
        Stall,
    }

    struct AutoCompactLoader {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        scripts: Vec<AutoCompactScript>,
        loads: StdMutex<usize>,
    }

    impl RuntimeLoader for AutoCompactLoader {
        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let mut loads = self.loads.lock().unwrap();
            let script = self
                .scripts
                .get(*loads)
                .cloned()
                .unwrap_or_else(|| AutoCompactScript::Text("done".to_owned()));
            *loads += 1;
            drop(loads);
            let provider = AutoCompactProvider {
                requests: Arc::clone(&self.requests),
                script,
            };
            Box::pin(async move {
                Runtime::new(provider, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        // Failure-path tests assert on the first error; turn
                        // retry is covered in lib.rs.
                        runtime: Arc::new(
                            runtime.with_turn_retry_policy(crate::TurnRetryPolicy::disabled()),
                        ),
                        pricing: None,
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct AutoCompactProvider {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        script: AutoCompactScript,
    }

    impl Provider for AutoCompactProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            match &self.script {
                AutoCompactScript::Text(text) => Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::OutputTextDelta { text: text.clone() }),
                    Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                ])),
                AutoCompactScript::Fail => Box::pin(stream::iter([Err(
                    qq_provider::ProviderError::Transport("scripted model failure".to_owned()),
                )])),
                AutoCompactScript::ContextOverflow => Box::pin(stream::iter([Err(
                    qq_provider::ProviderError::ResponseFailed {
                        kind: qq_provider::ProviderErrorKind::ContextExceeded,
                        message: "scripted context overflow".to_owned(),
                    },
                )])),
                AutoCompactScript::Stall => Box::pin(stream::pending()),
            }
        }
    }

    struct AutoCompactHarness {
        _directory: TempDir,
        runtime: SessionRuntime,
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        workspace_path: PathBuf,
        session_id: SessionId,
        events: SessionEventStream,
    }

    async fn auto_compact_harness(scripts: Vec<AutoCompactScript>) -> AutoCompactHarness {
        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(AutoCompactLoader {
                requests: Arc::clone(&requests),
                scripts,
                loads: StdMutex::new(0),
            }),
        )
        .await
        .unwrap();
        let workspace_path = directory.path().to_owned();
        let (workspace_id, _) = resolve_workspace(&runtime, &workspace_path).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        AutoCompactHarness {
            _directory: directory,
            runtime,
            requests,
            workspace_path,
            session_id,
            events,
        }
    }

    async fn queue_prompt(
        runtime: &SessionRuntime,
        session_id: SessionId,
        prompt: String,
    ) -> RunId {
        let queued = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt { session_id, prompt },
            )
            .await
            .unwrap();
        let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
            panic!("unexpected receipt")
        };
        run_id
    }

    /// Collects events until `stop` matches (inclusive), with a generous
    /// timeout: the auto-compaction tests stream multi-MiB outputs.
    async fn collect_until(
        events: &mut SessionEventStream,
        stop: impl Fn(&SessionEvent) -> bool,
    ) -> Vec<SessionEventEnvelope> {
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut observed = Vec::new();
            loop {
                let event = events.next().await.unwrap().unwrap();
                let done = stop(&event.event);
                observed.push(event);
                if done {
                    return observed;
                }
            }
        })
        .await
        .unwrap()
    }

    fn finished_for(run_id: RunId) -> impl Fn(&SessionEvent) -> bool {
        move |event| matches!(event, SessionEvent::RunFinished { run_id: finished, .. } if *finished == run_id)
    }

    fn position_of(
        observed: &[SessionEventEnvelope],
        predicate: impl Fn(&SessionEvent) -> bool,
    ) -> usize {
        observed
            .iter()
            .position(|event| predicate(&event.event))
            .unwrap()
    }

    /// An output that pushes the assembled context past the auto-compaction
    /// threshold while staying comfortably under the hard budget.
    fn over_threshold_output() -> String {
        "x".repeat(AUTO_COMPACT_CONTEXT_BYTES + 64 * 1024)
    }

    #[tokio::test]
    async fn prompts_below_the_context_threshold_never_auto_compact() {
        let mut harness = auto_compact_harness(vec![
            AutoCompactScript::Text("first answer".to_owned()),
            AutoCompactScript::Text("second answer".to_owned()),
        ])
        .await;
        for prompt in ["one", "two"] {
            let run_id =
                queue_prompt(&harness.runtime, harness.session_id, prompt.to_owned()).await;
            let observed = collect_until(&mut harness.events, finished_for(run_id)).await;
            // Only the prompt itself runs: nothing claims ahead of it and no
            // compaction commits.
            assert!(observed.iter().all(|event| match &event.event {
                SessionEvent::RunStarted {
                    run_id: started, ..
                } => *started == run_id,
                SessionEvent::SessionCompacted { .. } => false,
                _ => true,
            }));
        }
        assert_eq!(harness.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn crossing_the_byte_threshold_compacts_before_the_queued_prompt() {
        let mut harness = auto_compact_harness(vec![
            AutoCompactScript::Text(over_threshold_output()),
            AutoCompactScript::Text("the summary".to_owned()),
            AutoCompactScript::Text("done".to_owned()),
            AutoCompactScript::Text("done again".to_owned()),
        ])
        .await;
        let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
        collect_until(&mut harness.events, finished_for(first)).await;

        let prompt = queue_prompt(&harness.runtime, harness.session_id, "over".to_owned()).await;
        let observed = collect_until(&mut harness.events, finished_for(prompt)).await;

        // The compaction claims first; the prompt stays queued and runs
        // right after it.
        let compaction = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::RunStarted { run_id, .. } if *run_id != prompt => Some(*run_id),
                _ => None,
            })
            .expect("an auto-compaction run must start before the prompt");
        let compaction_finished = position_of(&observed, |event| {
            matches!(
                event,
                SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
                    if *run_id == compaction
            )
        });
        let compacted = position_of(&observed, |event| {
            matches!(event, SessionEvent::SessionCompacted { .. })
        });
        let prompt_started = position_of(
            &observed,
            |event| matches!(event, SessionEvent::RunStarted { run_id, .. } if *run_id == prompt),
        );
        let prompt_finished = position_of(&observed, |event| {
            matches!(
                event,
                SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
                    if *run_id == prompt
            )
        });
        assert!(compaction_finished < compacted);
        assert!(compacted < prompt_started);
        assert!(prompt_started < prompt_finished);

        {
            // The summarization request ends with the fixed instruction, and
            // the prompt then runs on the compacted assembly.
            let requests = harness.requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            let summarize = request_texts(&requests[1]);
            assert!(
                summarize
                    .last()
                    .unwrap()
                    .starts_with("Summarize this conversation")
            );
            let after = request_texts(&requests[2]);
            assert!(after[0].starts_with(COMPACTION_SUMMARY_PREAMBLE));
            assert!(after[0].contains("the summary"));
            assert_eq!(after[after.len() - 1], "over");
        }

        // The run row records automatic provenance.
        let connection = Connection::open(harness.workspace_path.join("sessions.sqlite3")).unwrap();
        let auto: bool = connection
            .query_row(
                "SELECT auto_compaction FROM runs WHERE id = ?1",
                [compaction.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(auto);

        // No re-trigger: the assembly shrank below the threshold, so the
        // next prompt runs directly.
        let third = queue_prompt(&harness.runtime, harness.session_id, "after".to_owned()).await;
        let observed = collect_until(&mut harness.events, finished_for(third)).await;
        assert!(observed.iter().all(|event| match &event.event {
            SessionEvent::RunStarted { run_id, .. } => *run_id == third,
            SessionEvent::SessionCompacted { .. } => false,
            _ => true,
        }));
    }

    #[tokio::test]
    async fn a_context_overflow_failure_compacts_before_the_next_prompt() {
        // The provider rejects the first prompt as exceeding the model
        // context window while the session is still under the byte
        // threshold. The failure must be loud (a failed run outcome naming
        // the overflow) and the next prompt must compact first instead of
        // hitting the same wall.
        let mut harness = auto_compact_harness(vec![
            AutoCompactScript::ContextOverflow,
            AutoCompactScript::Text("the summary".to_owned()),
            AutoCompactScript::Text("recovered".to_owned()),
        ])
        .await;
        let first = queue_prompt(&harness.runtime, harness.session_id, "big ask".to_owned()).await;
        let observed = collect_until(&mut harness.events, finished_for(first)).await;
        assert!(
            observed.iter().any(|event| matches!(
                &event.event,
                SessionEvent::RunFinished {
                    run_id,
                    outcome: RunOutcome::Failed { failure },
                    ..
                } if *run_id == first
                    && failure.kind == RunFailureKind::ProviderContextExceeded
            )),
            "the overflow must surface as a failed run outcome"
        );

        let second = queue_prompt(&harness.runtime, harness.session_id, "retry".to_owned()).await;
        let observed = collect_until(&mut harness.events, finished_for(second)).await;
        // A compaction run claims ahead of the retried prompt.
        let compaction = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::RunStarted { run_id, .. } if *run_id != second => Some(*run_id),
                _ => None,
            })
            .expect("a compaction must run before the retried prompt");
        let compacted = position_of(&observed, |event| {
            matches!(event, SessionEvent::SessionCompacted { .. })
        });
        let prompt_started = position_of(
            &observed,
            |event| matches!(event, SessionEvent::RunStarted { run_id, .. } if *run_id == second),
        );
        assert!(compacted < prompt_started);
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
                if *run_id == compaction
        )));
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
                if *run_id == second
        )));
    }

    #[tokio::test]
    async fn exceeding_the_hard_budget_compacts_once_and_the_prompt_proceeds() {
        let mut harness = auto_compact_harness(vec![
            AutoCompactScript::Text("x".repeat(MAX_CONTEXT_BYTES - 100 * 1024)),
            AutoCompactScript::Text("the summary".to_owned()),
            AutoCompactScript::Text("done".to_owned()),
        ])
        .await;
        let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
        collect_until(&mut harness.events, finished_for(first)).await;

        // Context plus prompt exceeds the hard budget. Submission is
        // admitted (previously this was rejected outright); the claim
        // compacts once, re-checks, and the prompt proceeds.
        let second = queue_prompt(
            &harness.runtime,
            harness.session_id,
            "y".repeat(MAX_PROMPT_BYTES),
        )
        .await;
        let observed = collect_until(&mut harness.events, finished_for(second)).await;
        assert!(
            observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
        );
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
                if *run_id == second
        )));
        assert_eq!(harness.requests.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_prompt_still_over_budget_after_compacting_fails_with_the_policy_outcome() {
        let mut harness = auto_compact_harness(vec![
            AutoCompactScript::Text("x".repeat(MAX_CONTEXT_BYTES - 100 * 1024)),
            // Pathological summarizer: the summary is as large as the
            // transcript it replaces, so the retry is still past the budget.
            AutoCompactScript::Text("s".repeat(MAX_CONTEXT_BYTES - 100 * 1024)),
        ])
        .await;
        let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
        collect_until(&mut harness.events, finished_for(first)).await;

        let second = queue_prompt(
            &harness.runtime,
            harness.session_id,
            "y".repeat(MAX_PROMPT_BYTES),
        )
        .await;
        let observed = collect_until(&mut harness.events, finished_for(second)).await;
        // The one attempt happened...
        assert!(
            observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
        );
        // ...and the prompt then fails with the context policy failure
        // without reaching the model.
        let outcome = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::RunFinished {
                    run_id, outcome, ..
                } if *run_id == second => Some(outcome.clone()),
                _ => None,
            })
            .unwrap();
        assert!(matches!(
            outcome,
            RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::Policy,
                    ref message,
                }
            } if message.contains("4 MiB limit")
        ));
        assert_eq!(harness.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_failed_auto_compaction_does_not_strand_the_queued_prompt() {
        let mut harness = auto_compact_harness(vec![
            AutoCompactScript::Text(over_threshold_output()),
            AutoCompactScript::Fail,
            AutoCompactScript::Text("done".to_owned()),
        ])
        .await;
        let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
        collect_until(&mut harness.events, finished_for(first)).await;

        let second = queue_prompt(&harness.runtime, harness.session_id, "over".to_owned()).await;
        let observed = collect_until(&mut harness.events, finished_for(second)).await;
        // The summarizer failed and committed nothing...
        let compaction = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::RunStarted { run_id, .. } if *run_id != second => Some(*run_id),
                _ => None,
            })
            .unwrap();
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished { run_id, outcome: RunOutcome::Failed { .. }, .. }
                if *run_id == compaction
        )));
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
        );
        // ...and the prompt still ran to completion, with no second attempt.
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
                if *run_id == second
        )));
        assert_eq!(harness.requests.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_compaction_that_does_not_shrink_the_assembly_never_loops() {
        let mut harness = auto_compact_harness(vec![
            AutoCompactScript::Text(over_threshold_output()),
            // The summary itself stays past the threshold: the guard must
            // let the prompt proceed after the single attempt.
            AutoCompactScript::Text(over_threshold_output()),
            AutoCompactScript::Text("done".to_owned()),
        ])
        .await;
        let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
        collect_until(&mut harness.events, finished_for(first)).await;

        let second = queue_prompt(&harness.runtime, harness.session_id, "over".to_owned()).await;
        let observed = collect_until(&mut harness.events, finished_for(second)).await;
        let compactions = observed
            .iter()
            .filter(|event| {
                matches!(
                    &event.event,
                    SessionEvent::RunStarted { run_id, .. } if *run_id != second
                )
            })
            .count();
        assert_eq!(compactions, 1, "exactly one automatic attempt per prompt");
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
                if *run_id == second
        )));
        assert_eq!(harness.requests.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn cancelling_the_queued_prompt_cancels_the_pending_auto_compaction() {
        let mut harness = auto_compact_harness(vec![
            AutoCompactScript::Text(over_threshold_output()),
            AutoCompactScript::Stall,
        ])
        .await;
        let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
        collect_until(&mut harness.events, finished_for(first)).await;

        let prompt = queue_prompt(&harness.runtime, harness.session_id, "over".to_owned()).await;
        let observed = collect_until(
            &mut harness.events,
            |event| matches!(event, SessionEvent::RunStarted { run_id, .. } if *run_id != prompt),
        )
        .await;
        let SessionEvent::RunStarted {
            run_id: compaction, ..
        } = observed.last().unwrap().event
        else {
            panic!("expected the auto-compaction to start")
        };

        // Cancelling the only queued prompt cascades to the compaction that
        // was running on its behalf.
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CancelRun { run_id: prompt },
            )
            .await
            .unwrap();
        let observed = collect_until(&mut harness.events, finished_for(compaction)).await;
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished { run_id, outcome: RunOutcome::Cancelled, .. }
                if *run_id == prompt
        )));
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::CancellationRequested { run_id, .. } if *run_id == compaction
        )));
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
        );
        // The compaction settled cancelled and the session ended idle.
        let session = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::RunFinished {
                    run_id,
                    outcome: RunOutcome::Cancelled,
                    session,
                    ..
                } if *run_id == compaction => Some(session.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(session.status, SessionStatus::Idle);
        assert_eq!(session.queued_prompts, 0);
        assert_eq!(session.active_run_id, None);
    }

    #[tokio::test]
    async fn cancelling_a_queued_prompt_never_cancels_a_manual_compaction() {
        let mut harness = auto_compact_harness(vec![
            AutoCompactScript::Text("hello".to_owned()),
            AutoCompactScript::Stall,
        ])
        .await;
        let first = queue_prompt(&harness.runtime, harness.session_id, "hi".to_owned()).await;
        collect_until(&mut harness.events, finished_for(first)).await;

        // A user-requested compaction claims and parks at the model.
        let compaction = compact_session(&harness.runtime, harness.session_id).await;
        collect_until(&mut harness.events, |event| {
            matches!(event, SessionEvent::RunStarted { run_id, .. } if *run_id == compaction)
        })
        .await;

        // Queue a prompt behind it, then cancel that prompt: the manual
        // compaction must keep running.
        let prompt = queue_prompt(&harness.runtime, harness.session_id, "later".to_owned()).await;
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CancelRun { run_id: prompt },
            )
            .await
            .unwrap();
        let observed = collect_until(&mut harness.events, finished_for(prompt)).await;
        assert!(!observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::CancellationRequested { run_id, .. } if *run_id == compaction
        )));

        // Clean up: cancel the parked compaction directly.
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CancelRun { run_id: compaction },
            )
            .await
            .unwrap();
        let observed = collect_until(&mut harness.events, finished_for(compaction)).await;
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished { run_id, outcome: RunOutcome::Cancelled, .. }
                if *run_id == compaction
        )));
    }

    #[tokio::test]
    async fn assembly_prunes_stale_read_only_results_but_never_mutating_ones() {
        let note = "n".repeat(600);
        let written = "w".repeat(600);
        let mut harness = scripted_runs_harness(
            ApprovalMode::Auto,
            vec![
                vec![
                    ("read_file", r#"{"path":"note.txt"}"#.to_owned()),
                    (
                        "write_file",
                        format!(r#"{{"path":"out.txt","content":"{written}"}}"#),
                    ),
                ],
                vec![],
                vec![],
                vec![("read_file", r#"{"path":"note.txt"}"#.to_owned())],
                vec![],
            ],
        )
        .await;
        std::fs::write(harness.workspace_path.join("note.txt"), &note).unwrap();

        for prompt in ["one", "two", "three", "four", "five"] {
            submit_prompt(&harness, prompt).await;
            collect_through_finished(&mut harness.events).await;
        }

        let requests = harness.requests.lock().unwrap();
        let last = requests.last().unwrap();
        let results = last
            .messages()
            .iter()
            .flat_map(Message::content)
            .filter_map(|block| match block {
                ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 3);
        // The old read is a stub naming the tool, arguments, and size.
        assert!(
            results[0].starts_with("[pruned: read_file {\"path\":\"note.txt\"} returned"),
            "stale read-only result must be stubbed, got {:?}",
            results[0]
        );
        assert!(results[0].ends_with("call it again if needed]"));
        // The equally old mutation is never pruned: not re-derivable.
        assert!(
            !results[1].starts_with("[pruned"),
            "mutating results must never be pruned, got {:?}",
            results[1]
        );
        // The recent read stays verbatim.
        assert!(
            results[2].contains("nnnn"),
            "recent results must stay verbatim, got {:?}",
            results[2]
        );
    }

    #[tokio::test]
    async fn creates_root_and_child_sessions_in_one_workspace_snapshot() {
        let (directory, runtime) = test_runtime().await;
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let root = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated {
            session_id: root_id,
        } = root.outcome
        else {
            panic!("unexpected receipt")
        };
        let child = create_session(&runtime, workspace_id, Some(root_id)).await;
        let CommandOutcome::SessionCreated {
            session_id: child_id,
        } = child.outcome
        else {
            panic!("unexpected receipt")
        };

        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(child_id),
                session_limit: 32,
                message_limit: 32,
            })
            .await
            .unwrap();

        assert_eq!(snapshot.sessions.len(), 2);
        assert_eq!(snapshot.focused.unwrap().summary.parent_id, Some(root_id));
    }

    #[tokio::test]
    async fn only_the_first_prompt_names_a_session() {
        let (directory, runtime) = test_runtime().await;
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };

        for prompt in ["New session", "Do not replace the first title"] {
            runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::SubmitPrompt {
                        session_id,
                        prompt: prompt.to_owned(),
                    },
                )
                .await
                .unwrap();
        }

        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 1,
                message_limit: 4,
            })
            .await
            .unwrap();
        assert_eq!(snapshot.focused.unwrap().summary.title, "New session");
    }

    #[tokio::test]
    async fn retries_return_the_original_durable_receipt() {
        let (directory, runtime) = test_runtime().await;
        let command_id = CommandId::generate().unwrap();
        let command = SessionCommand::ResolveWorkspace {
            path: directory.path().to_str().unwrap().to_owned(),
        };

        let first = runtime.command(command_id, command.clone()).await.unwrap();
        let retry = runtime.command(command_id, command).await.unwrap();

        assert_eq!(retry, first);
        assert_eq!(
            runtime
                .command(
                    command_id,
                    SessionCommand::ResolveWorkspace {
                        path: "/different".to_owned(),
                    },
                )
                .await
                .unwrap_err(),
            SessionRuntimeError::IdempotencyConflict
        );
    }

    #[tokio::test]
    async fn reasoning_replays_without_entering_transcript_or_model_context() {
        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(ReasoningLoader {
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let after_creation = created.committed_through;
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: after_creation,
            })
            .unwrap();

        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "first prompt".to_owned(),
                },
            )
            .await
            .unwrap();
        let observed = collect_through_finished(&mut events).await;
        let reasoning = observed
            .iter()
            .filter_map(|envelope| match &envelope.event {
                SessionEvent::ReasoningStarted { kind, .. } => Some(("started", *kind, None)),
                SessionEvent::ReasoningDelta { kind, text, .. } => {
                    Some(("delta", *kind, Some(text.as_str())))
                }
                SessionEvent::ReasoningCompleted { kind, .. } => Some(("completed", *kind, None)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            reasoning,
            vec![
                ("started", qq_provider::ReasoningKind::Summary, None),
                (
                    "delta",
                    qq_provider::ReasoningKind::Summary,
                    Some("private rationale")
                ),
                ("completed", qq_provider::ReasoningKind::Summary, None),
            ]
        );

        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 1,
                message_limit: 8,
            })
            .await
            .unwrap();
        let messages = snapshot.focused.unwrap().messages;
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, MessageRole::User);
        assert_eq!(messages[0].output, "first prompt");
        assert_eq!(messages[1].role, MessageRole::Assistant);
        assert_eq!(messages[1].output, "answer");
        assert!(
            messages
                .iter()
                .all(|message| !message.output.contains("private rationale"))
        );

        // A fresh subscription reads the same reasoning transitions from the
        // durable event log, in the same positions as the live subscriber.
        let mut replay = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: after_creation,
            })
            .unwrap();
        let replayed = tokio::time::timeout(Duration::from_secs(2), async {
            let mut replayed = Vec::new();
            for _ in 0..observed.len() {
                replayed.push(replay.next().await.unwrap().unwrap());
            }
            replayed
        })
        .await
        .unwrap();
        assert_eq!(replayed, observed);

        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "second prompt".to_owned(),
                },
            )
            .await
            .unwrap();
        collect_through_finished(&mut events).await;
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let next_context = request_texts(&requests[1]);
        assert!(next_context.iter().any(|text| text == "answer"));
        assert!(next_context.iter().any(|text| text == "second prompt"));
        assert!(
            next_context
                .iter()
                .all(|text| !text.contains("private rationale"))
        );
    }

    #[tokio::test]
    async fn streams_committed_run_events_and_snapshots_the_result() {
        let (directory, runtime) = test_runtime().await;
        let (workspace_id, initial) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();

        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "Say hello".to_owned(),
                },
            )
            .await
            .unwrap();

        let mut observed = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(event) = events.next().await {
                let event = event.unwrap();
                let finished = matches!(event.event, SessionEvent::RunFinished { .. });
                observed.push(event);
                if finished {
                    break;
                }
            }
        })
        .await
        .unwrap();

        assert!(matches!(
            observed[0].event,
            SessionEvent::PromptQueued { .. }
        ));
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(event.event, SessionEvent::TextAppended { .. }))
                .count(),
            2
        );
        assert!(
            observed
                .windows(2)
                .all(|events| { events[1].cursor.sequence == events[0].cursor.sequence + 1 })
        );
        assert!(matches!(
            &observed.last().unwrap().event,
            SessionEvent::RunFinished {
                session: SessionSummary {
                    context_tokens: Some(13),
                    ..
                },
                usage: Some(TokenUsage {
                    input_tokens: 10,
                    cache_read_input_tokens: 2,
                    cache_write_input_tokens: 1,
                    output_tokens: 5,
                }),
                context_tokens: Some(13),
                ..
            }
        ));
        assert!(observed.iter().any(|event| matches!(
            event.event,
            SessionEvent::RunContextUpdated {
                context_tokens: 13,
                ..
            }
        )));
        assert!(observed.iter().any(|event| matches!(
            event.event,
            SessionEvent::SessionContextUpdated {
                context_tokens: Some(13),
                ..
            }
        )));
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ModelTurnCompleted {
                turn_ordinal: 1,
                model: ModelSelection {
                    model: Some(model),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                usage: Some(TokenUsage {
                    input_tokens: 10,
                    cache_read_input_tokens: 2,
                    cache_write_input_tokens: 1,
                    output_tokens: 5,
                }),
                estimated_cost_usd_nanos: Some(20_500),
                ..
            } if model == "test/model"
        )));
        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 32,
                message_limit: 32,
            })
            .await
            .unwrap();
        let focused = snapshot.focused.unwrap();
        assert_eq!(focused.messages.len(), 2);
        assert_eq!(focused.messages[1].output, "hello");
        assert_eq!(focused.summary.status, SessionStatus::Idle);
        assert_eq!(focused.summary.model.as_deref(), Some("test/model"));
        assert_eq!(focused.summary.context_tokens, Some(13));
        assert_eq!(focused.summary.estimated_cost_usd_nanos, Some(20_500));
        assert_eq!(
            focused.runs[0].usage,
            Some(TokenUsage {
                input_tokens: 10,
                cache_read_input_tokens: 2,
                cache_write_input_tokens: 1,
                output_tokens: 5,
            })
        );
        assert_eq!(focused.runs[0].context_tokens, Some(13));
        assert_eq!(focused.runs[0].estimated_cost_usd_nanos, Some(20_500));
        let connection = Connection::open(directory.path().join("sessions.sqlite3")).unwrap();
        let (model, usage, cost, completed): (String, String, u64, u64) = connection
            .query_row(
                "SELECT model_json, usage_json, estimated_cost_usd_nanos, completed_at_ms
                 FROM model_turns",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<ModelSelection>(&model)
                .unwrap()
                .model
                .as_deref(),
            Some("test/model")
        );
        assert_eq!(
            serde_json::from_str::<TokenUsage>(&usage).unwrap(),
            TokenUsage {
                input_tokens: 10,
                cache_read_input_tokens: 2,
                cache_write_input_tokens: 1,
                output_tokens: 5,
            }
        );
        assert_eq!(cost, 20_500);
        assert!(completed > 0);
        assert!(snapshot.cursor.sequence > initial.sequence);
    }

    #[tokio::test]
    async fn unmeasured_new_prompt_clears_stale_session_context() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(UsageSequenceLoader {
                usages: StdMutex::new(vec![
                    Some(qq_provider::ProviderUsage {
                        input_tokens: 40_000,
                        cache_read_input_tokens: 12_000,
                        cache_write_input_tokens: 2_400,
                        output_tokens: 1,
                    }),
                    None,
                ]),
            }),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();

        let first_run = queue_prompt(&runtime, session_id, "measured".to_owned()).await;
        let first = collect_through_finished(&mut events).await;
        assert!(first.iter().any(|event| matches!(
            event.event,
            SessionEvent::SessionContextUpdated {
                run_id,
                context_tokens: Some(54_400),
            } if run_id == first_run
        )));

        let second_run = queue_prompt(&runtime, session_id, "unmeasured".to_owned()).await;
        let second = collect_through_finished(&mut events).await;
        assert!(second.iter().any(|event| matches!(
            event.event,
            SessionEvent::SessionContextUpdated {
                run_id,
                context_tokens: None,
            } if run_id == second_run
        )));
        assert!(second.iter().all(|event| !matches!(
            event.event,
            SessionEvent::RunContextUpdated { run_id, .. } if run_id == second_run
        )));
        assert!(second.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished {
                session: SessionSummary {
                    context_tokens: None,
                    ..
                },
                run_id,
                usage: None,
                context_tokens: None,
                ..
            } if *run_id == second_run
        )));

        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 1,
                message_limit: 8,
            })
            .await
            .unwrap();
        let focused = snapshot.focused.unwrap();
        assert_eq!(focused.summary.context_tokens, None);
        assert_eq!(focused.runs.len(), 2);
        assert_eq!(focused.runs[0].context_tokens, Some(54_400));
        assert_eq!(focused.runs[1].context_tokens, None);
    }

    #[tokio::test]
    async fn cancellation_before_a_model_turn_preserves_known_session_cost() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(PricedHangingLoader),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();

        let run_id = queue_prompt(&runtime, session_id, "cancel".to_owned()).await;
        collect_until(&mut events, |event| {
            matches!(event, SessionEvent::RunStarted { run_id: started, .. } if *started == run_id)
        })
        .await;
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CancelRun { run_id },
            )
            .await
            .unwrap();
        let observed = collect_until(&mut events, finished_for(run_id)).await;

        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished {
                session: SessionSummary {
                    estimated_cost_usd_nanos: Some(0),
                    ..
                },
                outcome: RunOutcome::Cancelled,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn persists_tool_transitions_and_reconstructs_follow_up_context() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("note.txt"), "tool result\n").unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(ToolLoopLoader {
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "inspect the note".to_owned(),
                },
            )
            .await
            .unwrap();

        let observed = collect_through_finished(&mut events).await;
        let context_updates = observed
            .iter()
            .filter_map(|event| match &event.event {
                SessionEvent::SessionContextUpdated {
                    context_tokens: Some(context_tokens),
                    ..
                } => Some(*context_tokens),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(context_updates, [4, 6]);
        let requested = observed
            .iter()
            .position(|event| matches!(event.event, SessionEvent::ToolCallRequested { .. }))
            .unwrap();
        let started = observed
            .iter()
            .position(|event| matches!(event.event, SessionEvent::ToolCallStarted { .. }))
            .unwrap();
        let finished = observed
            .iter()
            .position(|event| matches!(event.event, SessionEvent::ToolCallFinished { .. }))
            .unwrap();
        assert!(requested < started && started < finished);

        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 1,
                message_limit: 8,
            })
            .await
            .unwrap();
        let focused = snapshot.focused.unwrap();
        assert_eq!(focused.tool_calls.len(), 1);
        assert_eq!(focused.tool_calls[0].state, ToolCallState::Completed);
        assert_eq!(
            focused.tool_calls[0].result.as_deref(),
            Some("tool result\n")
        );
        assert_eq!(
            focused.runs[0].usage,
            Some(TokenUsage {
                input_tokens: 10,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                output_tokens: 3,
            })
        );
        assert_eq!(focused.runs[0].context_tokens, Some(6));
        assert_eq!(focused.summary.context_tokens, Some(6));

        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "what did you read?".to_owned(),
                },
            )
            .await
            .unwrap();
        let _ = collect_through_finished(&mut events).await;

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[2].messages().len(), 5);
        assert!(matches!(
            requests[2].messages()[1].content(),
            [ContentBlock::ToolCall { id, .. }] if id == "call_0"
        ));
        assert!(matches!(
            requests[2].messages()[2].content(),
            [ContentBlock::ToolResult { call_id, content, .. }]
                if call_id == "call_0" && content == "tool result\n"
        ));
    }

    #[tokio::test]
    async fn one_durable_run_continues_across_the_internal_tool_budget() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("note.txt"), "tool result\n").unwrap();
        std::fs::write(directory.path().join("slice-effects.txt"), "seed").unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(database_path.clone()),
            Arc::new(RenewableSliceLoader {
                requests: Arc::clone(&requests),
                checkpoint_wait: None,
                metered_empty_checkpoint: false,
            }),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created =
            create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::Auto).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        let run_id = queue_prompt(&runtime, session_id, "finish a long task".to_owned()).await;

        let observed = collect_until(&mut events, finished_for(run_id)).await;
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(
                    event.event,
                    SessionEvent::RunStarted { run_id: started, .. } if started == run_id
                ))
                .count(),
            1
        );
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(
                    event.event,
                    SessionEvent::RunFinished { run_id: finished, .. } if finished == run_id
                ))
                .count(),
            1
        );
        assert!(matches!(
            observed.last().map(|event| &event.event),
            Some(SessionEvent::RunFinished {
                run_id: finished,
                outcome: RunOutcome::Completed,
                ..
            }) if *finished == run_id
        ));
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(event.event, SessionEvent::ToolCallRequested { .. }))
                .count(),
            crate::MAX_TOOL_CALLS_PER_SLICE
        );
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(event.event, SessionEvent::ToolCallFinished { .. }))
                .count(),
            crate::MAX_TOOL_CALLS_PER_SLICE
        );

        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 1,
                message_limit: 16,
            })
            .await
            .unwrap();
        let focused = snapshot.focused.unwrap();
        assert_eq!(focused.runs.len(), 1);
        assert_eq!(focused.runs[0].outcome, Some(RunOutcome::Completed));
        assert_eq!(focused.tool_calls.len(), crate::MAX_TOOL_CALLS_PER_SLICE);
        assert!(
            focused
                .tool_calls
                .iter()
                .all(|call| { call.run_id == run_id && call.state == ToolCallState::Completed })
        );
        let mut provider_call_ids = focused
            .tool_calls
            .iter()
            .map(|call| call.provider_call_id.as_str())
            .collect::<Vec<_>>();
        provider_call_ids.sort_unstable();
        provider_call_ids.dedup();
        assert_eq!(provider_call_ids.len(), crate::MAX_TOOL_CALLS_PER_SLICE);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("slice-effects.txt")).unwrap(),
            "seedx",
            "the mutating call before rollover must execute exactly once"
        );
        assert!(focused.messages.iter().any(|message| {
            message.role == MessageRole::Assistant && message.output == "slice checkpoint"
        }));
        assert!(focused.messages.iter().any(|message| {
            message.role == MessageRole::Assistant && message.output == "task complete"
        }));

        {
            let recorded_requests = requests.lock().unwrap();
            let checkpoint = &recorded_requests[recorded_requests.len() - 2];
            let continuation = recorded_requests.last().unwrap();
            assert!(checkpoint.tools().is_empty());
            assert!(!continuation.tools().is_empty());
            assert!(
                continuation
                    .system()
                    .is_some_and(|system| system.contains(crate::SLICE_CONTINUATION_NOTICE))
            );
            assert!(continuation.messages().iter().any(|message| {
                message.content().iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::Text { text } if text == "slice checkpoint"
                    )
                })
            }));
        }

        let replay_after = observed.last().unwrap().cursor;
        drop(runtime);
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(database_path),
            Arc::new(RenewableSliceLoader {
                requests: Arc::clone(&requests),
                checkpoint_wait: None,
                metered_empty_checkpoint: false,
            }),
        )
        .await
        .unwrap();
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: replay_after,
            })
            .unwrap();
        let follow_up = queue_prompt(&runtime, session_id, "confirm completion".to_owned()).await;
        let replayed = collect_until(&mut events, finished_for(follow_up)).await;
        assert!(matches!(
            replayed.last().map(|event| &event.event),
            Some(SessionEvent::RunFinished {
                run_id: finished,
                outcome: RunOutcome::Completed,
                ..
            }) if *finished == follow_up
        ));
        let requests = requests.lock().unwrap();
        let replay_request = requests.last().unwrap();
        assert!(replay_request.messages().iter().any(|message| {
            message.content().iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::Text { text } if text == "slice checkpoint"
                )
            })
        }));
        assert_eq!(
            std::fs::read_to_string(directory.path().join("slice-effects.txt")).unwrap(),
            "seedx",
            "replay and follow-up context must not repeat the mutation"
        );
    }

    #[tokio::test]
    async fn cancellation_at_the_slice_checkpoint_has_one_cancelled_terminal() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("note.txt"), "tool result\n").unwrap();
        std::fs::write(directory.path().join("slice-effects.txt"), "seed").unwrap();
        let checkpoint_wait = Arc::new(tokio::sync::Notify::new());
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(RenewableSliceLoader {
                requests: Arc::clone(&requests),
                checkpoint_wait: Some(Arc::clone(&checkpoint_wait)),
                metered_empty_checkpoint: false,
            }),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created =
            create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::Auto).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        let run_id = queue_prompt(&runtime, session_id, "finish a long task".to_owned()).await;

        tokio::time::timeout(Duration::from_secs(30), checkpoint_wait.notified())
            .await
            .expect("the run must reach its tool-free checkpoint request");
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CancelRun { run_id },
            )
            .await
            .unwrap();
        let observed = collect_until(&mut events, finished_for(run_id)).await;

        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(
                    event.event,
                    SessionEvent::RunFinished { run_id: finished, .. } if finished == run_id
                ))
                .count(),
            1
        );
        assert!(matches!(
            observed.last().map(|event| &event.event),
            Some(SessionEvent::RunFinished {
                run_id: finished,
                outcome: RunOutcome::Cancelled,
                ..
            }) if *finished == run_id
        ));
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(event.event, SessionEvent::ToolCallRequested { .. }))
                .count(),
            crate::MAX_TOOL_CALLS_PER_SLICE
        );
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(event.event, SessionEvent::ToolCallFinished { .. }))
                .count(),
            crate::MAX_TOOL_CALLS_PER_SLICE
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join("slice-effects.txt")).unwrap(),
            "seedx"
        );
        let requests = requests.lock().unwrap();
        assert!(requests.last().unwrap().tools().is_empty());
        assert!(!requests.iter().any(|request| {
            request
                .system()
                .is_some_and(|system| system.contains(crate::SLICE_CONTINUATION_NOTICE))
        }));
    }

    #[tokio::test]
    async fn empty_checkpoint_failure_retains_the_billed_turn_accounting() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("note.txt"), "tool result\n").unwrap();
        std::fs::write(directory.path().join("slice-effects.txt"), "seed").unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(RenewableSliceLoader {
                requests,
                checkpoint_wait: None,
                metered_empty_checkpoint: true,
            }),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created =
            create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::Auto).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        let run_id = queue_prompt(&runtime, session_id, "finish a long task".to_owned()).await;

        let observed = collect_until(&mut events, finished_for(run_id)).await;
        assert!(matches!(
            observed.last().map(|event| &event.event),
            Some(SessionEvent::RunFinished {
                run_id: finished,
                outcome: RunOutcome::Failed {
                    failure: RunFailure {
                        kind: RunFailureKind::ProviderResponse,
                        message,
                    },
                },
                ..
            }) if *finished == run_id && message == "provider returned an empty slice checkpoint"
        ));

        let expected_usage = TokenUsage {
            input_tokens: 19,
            cache_read_input_tokens: 1,
            cache_write_input_tokens: 2,
            output_tokens: 21,
        };
        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 1,
                message_limit: 16,
            })
            .await
            .unwrap();
        let focused = snapshot.focused.unwrap();
        assert_eq!(focused.runs[0].usage, Some(expected_usage));
        assert_eq!(focused.runs[0].estimated_cost_usd_nanos, Some(61_700));
        let accounting = focused.summary.accounting.unwrap();
        assert_eq!(accounting.direct.usage, Some(expected_usage));
        assert_eq!(accounting.direct.estimated_cost_usd_nanos, Some(61_700));
        assert_eq!(focused.summary.estimated_cost_usd_nanos, Some(61_700));
        assert!(!focused.messages.iter().any(|message| {
            message.role == MessageRole::Assistant && message.output == "slice checkpoint"
        }));
    }

    #[tokio::test]
    async fn terminal_runs_replay_committed_turns_and_status_in_follow_up_context() {
        let outcomes = [
            (RunOutcome::Completed, None),
            (
                RunOutcome::Failed {
                    failure: RunFailure {
                        kind: RunFailureKind::ProviderTransport,
                        message: "provider connection dropped".to_owned(),
                    },
                },
                Some("The previous run failed: provider connection dropped"),
            ),
            (
                RunOutcome::Cancelled,
                Some("The previous run was cancelled."),
            ),
            (
                RunOutcome::Interrupted,
                Some("The previous run was interrupted before completion."),
            ),
        ];

        for (outcome, expected_status) in outcomes {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open(directory.path().join("sessions.sqlite3"))
                .await
                .unwrap();
            let resolved = store
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::ResolveWorkspace {
                        path: directory.path().to_str().unwrap().to_owned(),
                    },
                )
                .await
                .unwrap();
            let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.receipt.outcome
            else {
                panic!("unexpected receipt")
            };
            let created = store
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::CreateSession {
                        workspace_id,
                        parent_id: None,
                        model: ModelSelection {
                            model: Some("test/model".to_owned()),
                            max_output_tokens: Some(256),
                            organization: None,
                        },
                        approval_mode: ApprovalMode::default(),
                    },
                )
                .await
                .unwrap();
            let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
                panic!("unexpected receipt")
            };
            store
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::SubmitPrompt {
                        session_id,
                        prompt: "inspect the note".to_owned(),
                    },
                )
                .await
                .unwrap();
            let claimed = store.claim_next_run(false).await.unwrap().unwrap();
            let tool_call_id = ToolCallId::generate().unwrap();
            let call = RuntimeToolCall {
                id: tool_call_id,
                turn_ordinal: 1,
                call_ordinal: 1,
                provider_call_id: "call_0".to_owned(),
                name: "read_file".to_owned(),
                arguments: r#"{"path":"note.txt"}"#.to_owned(),
                argument_error: None,
            };
            store
                .persist_model_turn(
                    &claimed,
                    ModelTurnCommit {
                        turn_ordinal: 1,
                        message: Message::new(
                            Role::Assistant,
                            vec![ContentBlock::ToolCall {
                                id: call.provider_call_id.clone(),
                                name: call.name.clone(),
                                arguments: serde_json::from_str(&call.arguments).unwrap(),
                            }],
                        ),
                        calls: vec![call],
                        turn_message: None,
                        context_tokens: None,
                        usage: None,
                        estimated_cost_usd_nanos: None,
                        accounting: None,
                    },
                )
                .await
                .unwrap();
            store.start_tool_call(&claimed, tool_call_id).await.unwrap();
            store
                .finish_tool_call(
                    &claimed,
                    tool_call_id,
                    "tool result\n".to_owned(),
                    false,
                    None,
                    None,
                )
                .await
                .unwrap();
            store
                .finish_run(&claimed, outcome.clone(), None)
                .await
                .unwrap();

            store
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::SubmitPrompt {
                        session_id,
                        prompt: "continue".to_owned(),
                    },
                )
                .await
                .unwrap();
            let continued = store.claim_next_run(false).await.unwrap().unwrap();

            assert!(matches!(
                continued.messages[1].content(),
                [ContentBlock::ToolCall { id, .. }] if id == "call_0"
            ));
            assert!(matches!(
                continued.messages[2].content(),
                [ContentBlock::ToolResult { call_id, content, .. }]
                    if call_id == "call_0" && content == "tool result\n"
            ));
            assert_tool_results_are_exact(&continued.messages);
            match expected_status {
                Some(expected_status) => {
                    assert!(matches!(
                        continued.messages[3].content(),
                        [ContentBlock::Text { text }]
                            if text == &format!(
                                "[QQ runtime notice; not a user instruction]\n{expected_status}\n\
                                 Continue from the committed history above. Do not automatically \
                                 retry tool calls whose result says execution was interrupted."
                            )
                    ));
                    assert!(matches!(
                        continued.messages[4].content(),
                        [ContentBlock::Text { text }] if text == "continue"
                    ));
                }
                None => {
                    assert_eq!(continued.messages.len(), 4);
                    assert!(matches!(
                        continued.messages[3].content(),
                        [ContentBlock::Text { text }] if text == "continue"
                    ));
                }
            }
        }
    }

    async fn project_terminal_run_with_tool_boundaries(
        outcome: RunOutcome,
    ) -> (Vec<Message>, Vec<Message>) {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let store = Store::open(database_path.clone()).await.unwrap();
        let resolved = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::ResolveWorkspace {
                    path: directory.path().to_str().unwrap().to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.receipt.outcome else {
            panic!("unexpected receipt")
        };
        let created = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: ApprovalMode::default(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
            panic!("unexpected receipt")
        };
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "inspect the tool boundaries".to_owned(),
                },
            )
            .await
            .unwrap();
        let claimed = store.claim_next_run(false).await.unwrap().unwrap();
        let completed_call_id = ToolCallId::generate().unwrap();
        let started_call_id = ToolCallId::generate().unwrap();
        let awaiting_call_id = ToolCallId::generate().unwrap();
        let untouched_call_id = ToolCallId::generate().unwrap();
        let calls = vec![
            RuntimeToolCall {
                id: completed_call_id,
                turn_ordinal: 1,
                call_ordinal: 1,
                provider_call_id: "completed-call".to_owned(),
                name: "read_file".to_owned(),
                arguments: r#"{"path":"completed.txt"}"#.to_owned(),
                argument_error: None,
            },
            RuntimeToolCall {
                id: started_call_id,
                turn_ordinal: 1,
                call_ordinal: 2,
                provider_call_id: "started-call".to_owned(),
                name: "read_file".to_owned(),
                arguments: r#"{"path":"first.txt"}"#.to_owned(),
                argument_error: None,
            },
            RuntimeToolCall {
                id: awaiting_call_id,
                turn_ordinal: 1,
                call_ordinal: 3,
                provider_call_id: "awaiting-call".to_owned(),
                name: "shell".to_owned(),
                arguments: r#"{"command":"true"}"#.to_owned(),
                argument_error: None,
            },
            RuntimeToolCall {
                id: untouched_call_id,
                turn_ordinal: 1,
                call_ordinal: 4,
                provider_call_id: "untouched-call".to_owned(),
                name: "read_file".to_owned(),
                arguments: r#"{"path":"second.txt"}"#.to_owned(),
                argument_error: None,
            },
        ];
        store
            .persist_model_turn(
                &claimed,
                ModelTurnCommit {
                    turn_ordinal: 1,
                    message: Message::new(
                        Role::Assistant,
                        calls
                            .iter()
                            .map(|call| ContentBlock::ToolCall {
                                id: call.provider_call_id.clone(),
                                name: call.name.clone(),
                                arguments: serde_json::from_str(&call.arguments).unwrap(),
                            })
                            .collect(),
                    ),
                    calls,
                    turn_message: None,
                    context_tokens: None,
                    usage: None,
                    estimated_cost_usd_nanos: None,
                    accounting: None,
                },
            )
            .await
            .unwrap();
        store
            .start_tool_call(&claimed, completed_call_id)
            .await
            .unwrap();
        store
            .finish_tool_call(
                &claimed,
                completed_call_id,
                "persisted result".to_owned(),
                false,
                None,
                None,
            )
            .await
            .unwrap();
        store
            .start_tool_call(&claimed, started_call_id)
            .await
            .unwrap();
        store
            .request_tool_approval(&claimed, awaiting_call_id, None, None)
            .await
            .unwrap();
        let finished = store.finish_run(&claimed, outcome, None).await.unwrap();
        let after = finished.last().unwrap().cursor;
        drop(store);
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        drop(connection);
        let restarted_database_path = directory.path().join("restarted-sessions.sqlite3");
        std::fs::copy(&database_path, &restarted_database_path).unwrap();

        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(database_path),
            Arc::new(CapturingLoader {
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after,
            })
            .unwrap();
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "continue safely".to_owned(),
                },
            )
            .await
            .unwrap();
        let _ = collect_through_finished(&mut events).await;

        let projected_before_restart = {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            requests[0].messages().to_vec()
        };
        drop(runtime);

        let restarted_requests = Arc::new(StdMutex::new(Vec::new()));
        let restarted = SessionRuntime::open(
            SessionRuntimeOptions::new(restarted_database_path),
            Arc::new(CapturingLoader {
                requests: Arc::clone(&restarted_requests),
            }),
        )
        .await
        .unwrap();
        let mut restarted_events = restarted
            .subscribe(SubscribeRequest {
                workspace_id,
                after,
            })
            .unwrap();
        restarted
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "continue safely".to_owned(),
                },
            )
            .await
            .unwrap();
        let _ = collect_through_finished(&mut restarted_events).await;
        let restarted_requests = restarted_requests.lock().unwrap();
        assert_eq!(restarted_requests.len(), 1);
        (
            projected_before_restart,
            restarted_requests[0].messages().to_vec(),
        )
    }

    #[tokio::test]
    async fn terminal_runs_project_exact_tool_boundaries_across_restart() {
        let outcomes = [
            (
                RunOutcome::Failed {
                    failure: RunFailure {
                        kind: RunFailureKind::ProviderTransport,
                        message: "provider connection dropped".to_owned(),
                    },
                },
                "Tool execution did not start before the run failed.",
                "The previous run failed: provider connection dropped",
            ),
            (
                RunOutcome::Cancelled,
                "Tool execution did not start before the run was cancelled.",
                "The previous run was cancelled.",
            ),
            (
                RunOutcome::Interrupted,
                "Tool execution did not start before the run was interrupted.",
                "The previous run was interrupted before completion.",
            ),
        ];

        for (outcome, expected_not_executed, expected_status) in outcomes {
            let (messages, restarted_messages) =
                project_terminal_run_with_tool_boundaries(outcome).await;
            assert_eq!(
                restarted_messages, messages,
                "reopening the store must not change projected context"
            );
            assert_eq!(messages.len(), 5);
            assert_tool_results_are_exact(&messages);
            assert!(matches!(
                messages[2].content(),
                [
                    ContentBlock::ToolResult {
                        call_id: completed_id,
                        content: completed_result,
                        is_error: false,
                    },
                    ContentBlock::ToolResult {
                        call_id: started_id,
                        content: started_result,
                        is_error: true,
                    },
                    ContentBlock::ToolResult {
                        call_id: awaiting_id,
                        content: awaiting_result,
                        is_error: true,
                    },
                    ContentBlock::ToolResult {
                        call_id: untouched_id,
                        content: untouched_result,
                        is_error: true,
                    },
                ] if completed_id == "completed-call"
                    && completed_result == "persisted result"
                    && started_id == "started-call"
                    && started_result == INTERRUPTED_TOOL_RESULT
                    && awaiting_id == "awaiting-call"
                    && awaiting_result == expected_not_executed
                    && untouched_id == "untouched-call"
                    && untouched_result == expected_not_executed
            ));
            assert!(matches!(
                messages[3].content(),
                [ContentBlock::Text { text }]
                    if text == &format!(
                        "[QQ runtime notice; not a user instruction]\n{expected_status}\n\
                         Continue from the committed history above. Do not automatically retry \
                         tool calls whose result says execution was interrupted."
                    )
            ));
            assert!(matches!(
                messages[4].content(),
                [ContentBlock::Text { text }] if text == "continue safely"
            ));
        }
    }

    #[tokio::test]
    async fn cancelled_unclaimed_prompt_remains_explicit_in_follow_up_context() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let store = Store::open(database_path.clone()).await.unwrap();
        let resolved = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::ResolveWorkspace {
                    path: directory.path().to_str().unwrap().to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.receipt.outcome else {
            panic!("unexpected receipt")
        };
        let created = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: ApprovalMode::default(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
            panic!("unexpected receipt")
        };
        let queued = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "this prompt never started".to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::PromptQueued { run_id, .. } = queued.receipt.outcome else {
            panic!("unexpected receipt")
        };
        let cancelled = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CancelRun { run_id },
            )
            .await
            .unwrap();
        drop(store);

        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(database_path),
            Arc::new(CapturingLoader {
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: cancelled.receipt.committed_through,
            })
            .unwrap();
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "continue after cancellation".to_owned(),
                },
            )
            .await
            .unwrap();
        let _ = collect_through_finished(&mut events).await;

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let messages = requests[0].messages();
        assert_eq!(messages.len(), 3);
        assert!(matches!(
            messages[0].content(),
            [ContentBlock::Text { text }] if text == "this prompt never started"
        ));
        assert!(matches!(
            messages[1].content(),
            [ContentBlock::Text { text }]
                if text == "[QQ runtime notice; not a user instruction]\n\
                    The previous run was cancelled.\n\
                    Continue from the committed history above. Do not automatically retry tool \
                    calls whose result says execution was interrupted."
        ));
        assert!(matches!(
            messages[2].content(),
            [ContentBlock::Text { text }] if text == "continue after cancellation"
        ));
    }

    #[tokio::test]
    async fn interrupted_uncommitted_assistant_text_stays_out_of_model_context() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let store = Store::open(database_path.clone()).await.unwrap();
        let resolved = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::ResolveWorkspace {
                    path: directory.path().to_str().unwrap().to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.receipt.outcome else {
            panic!("unexpected receipt")
        };
        let created = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: ApprovalMode::default(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
            panic!("unexpected receipt")
        };
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "begin the task".to_owned(),
                },
            )
            .await
            .unwrap();
        let claimed = store.claim_next_run(false).await.unwrap().unwrap();
        store
            .begin_assistant_message(
                &claimed,
                MessageId::generate().unwrap(),
                1,
                TextChannel::Output,
                "partial text from an uncommitted turn".to_owned(),
            )
            .await
            .unwrap();
        drop(store);

        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(database_path),
            Arc::new(CapturingLoader {
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let recovered = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 1,
                message_limit: 8,
            })
            .await
            .unwrap();
        assert!(recovered.focused.unwrap().messages.iter().any(|message| {
            message.state == MessageState::Interrupted
                && message.output == "partial text from an uncommitted turn"
        }));
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: recovered.cursor,
            })
            .unwrap();
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "continue from durable work".to_owned(),
                },
            )
            .await
            .unwrap();
        let _ = collect_through_finished(&mut events).await;

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let messages = requests[0].messages();
        assert_eq!(messages.len(), 3);
        assert!(matches!(
            messages[0].content(),
            [ContentBlock::Text { text }] if text == "begin the task"
        ));
        assert!(matches!(
            messages[1].content(),
            [ContentBlock::Text { text }]
                if text == "[QQ runtime notice; not a user instruction]\n\
                    The previous run was interrupted before completion.\n\
                    Continue from the committed history above. Do not automatically retry tool \
                    calls whose result says execution was interrupted."
        ));
        assert!(matches!(
            messages[2].content(),
            [ContentBlock::Text { text }] if text == "continue from durable work"
        ));
    }

    #[tokio::test]
    async fn historical_flat_assistant_output_precedes_the_terminal_notice() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let store = Store::open(database_path.clone()).await.unwrap();
        let resolved = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::ResolveWorkspace {
                    path: directory.path().to_str().unwrap().to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.receipt.outcome else {
            panic!("unexpected receipt")
        };
        let created = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: ApprovalMode::default(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
            panic!("unexpected receipt")
        };
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "legacy prompt".to_owned(),
                },
            )
            .await
            .unwrap();
        let claimed = store.claim_next_run(false).await.unwrap().unwrap();
        store
            .begin_assistant_message(
                &claimed,
                MessageId::generate().unwrap(),
                1,
                TextChannel::Output,
                "legacy committed answer".to_owned(),
            )
            .await
            .unwrap();
        let finished = store
            .finish_run(
                &claimed,
                RunOutcome::Failed {
                    failure: RunFailure {
                        kind: RunFailureKind::ProviderTransport,
                        message: "legacy provider failed".to_owned(),
                    },
                },
                None,
            )
            .await
            .unwrap();
        let after = finished.last().unwrap().cursor;
        drop(store);

        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute(
                "UPDATE messages SET state = 'complete'\
                 WHERE session_id = ?1 AND role = 'assistant'",
                [session_id.to_string()],
            )
            .unwrap();
        drop(connection);

        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(database_path),
            Arc::new(CapturingLoader {
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after,
            })
            .unwrap();
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "continue from the legacy store".to_owned(),
                },
            )
            .await
            .unwrap();
        let _ = collect_through_finished(&mut events).await;

        let requests = requests.lock().unwrap();
        let messages = requests[0].messages();
        assert_eq!(messages.len(), 4);
        assert!(matches!(
            messages[0].content(),
            [ContentBlock::Text { text }] if text == "legacy prompt"
        ));
        assert!(matches!(
            messages[1].content(),
            [ContentBlock::Text { text }] if text == "legacy committed answer"
        ));
        assert!(matches!(
            messages[2].content(),
            [ContentBlock::Text { text }]
                if text.contains("The previous run failed: legacy provider failed")
        ));
        assert!(matches!(
            messages[3].content(),
            [ContentBlock::Text { text }] if text == "continue from the legacy store"
        ));
    }

    #[tokio::test]
    async fn manual_and_auto_compaction_share_the_terminal_run_projection() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let store = Store::open(database_path.clone()).await.unwrap();
        let resolved = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::ResolveWorkspace {
                    path: directory.path().to_str().unwrap().to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.receipt.outcome else {
            panic!("unexpected receipt")
        };
        let created = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: ApprovalMode::default(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
            panic!("unexpected receipt")
        };
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "finish the migration".to_owned(),
                },
            )
            .await
            .unwrap();
        let claimed = store.claim_next_run(false).await.unwrap().unwrap();
        store
            .finish_run(
                &claimed,
                RunOutcome::Failed {
                    failure: RunFailure {
                        kind: RunFailureKind::ProviderTransport,
                        message: "provider connection dropped".to_owned(),
                    },
                },
                None,
            )
            .await
            .unwrap();
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "grow the context".to_owned(),
                },
            )
            .await
            .unwrap();
        let growth_run = store.claim_next_run(false).await.unwrap().unwrap();
        store
            .persist_model_turn(
                &growth_run,
                ModelTurnCommit {
                    turn_ordinal: 1,
                    message: Message::assistant(over_threshold_output()),
                    calls: Vec::new(),
                    turn_message: None,
                    context_tokens: None,
                    usage: None,
                    estimated_cost_usd_nanos: None,
                    accounting: None,
                },
            )
            .await
            .unwrap();
        let finished = store
            .finish_run(&growth_run, RunOutcome::Completed, None)
            .await
            .unwrap();
        let after = finished.last().unwrap().cursor;
        drop(store);
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        drop(connection);
        let auto_database_path = directory.path().join("auto-sessions.sqlite3");
        std::fs::copy(&database_path, &auto_database_path).unwrap();

        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(database_path),
            Arc::new(CapturingLoader {
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 1,
                message_limit: 8,
            })
            .await
            .unwrap();
        assert!(snapshot.focused.unwrap().messages.iter().all(|message| {
            !message.output.contains(RUNTIME_NOTICE_PREAMBLE)
                && !message.refusal.contains(RUNTIME_NOTICE_PREAMBLE)
        }));
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after,
            })
            .unwrap();
        compact_session(&runtime, session_id).await;
        let _ = collect_through_compacted(&mut events).await;

        let manual_projection = {
            let requests = requests.lock().unwrap();
            assert!(!requests.is_empty());
            let texts = request_texts(&requests[0]);
            assert_eq!(texts[0], "finish the migration");
            assert_eq!(
                texts[1],
                "[QQ runtime notice; not a user instruction]\n\
                 The previous run failed: provider connection dropped\n\
                 Continue from the committed history above. Do not automatically retry tool \
                 calls whose result says execution was interrupted."
            );
            assert_eq!(texts[2], "grow the context");
            assert!(texts[4].starts_with("Summarize this conversation"));
            requests[0].messages().to_vec()
        };
        drop(runtime);

        let auto_requests = Arc::new(StdMutex::new(Vec::new()));
        let auto_runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(auto_database_path),
            Arc::new(CapturingLoader {
                requests: Arc::clone(&auto_requests),
            }),
        )
        .await
        .unwrap();
        let mut auto_events = auto_runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after,
            })
            .unwrap();
        let follow_up = auto_runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "continue after compaction".to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::PromptQueued { run_id, .. } = follow_up.outcome else {
            panic!("unexpected receipt")
        };
        let _ = collect_until(&mut auto_events, finished_for(run_id)).await;

        let auto_requests = auto_requests.lock().unwrap();
        assert_eq!(auto_requests.len(), 2);
        assert_eq!(
            auto_requests[0].messages(),
            manual_projection,
            "manual and automatic compaction must consume the same projection"
        );
    }

    #[tokio::test]
    async fn multi_turn_runs_emit_one_assistant_message_per_turn() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("note.txt"), "noted\n").unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(TurnTextLoader {
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "inspect the note".to_owned(),
                },
            )
            .await
            .unwrap();

        let observed = collect_through_finished(&mut events).await;
        let started = observed
            .iter()
            .filter_map(|event| match &event.event {
                SessionEvent::AssistantMessageStarted { message } => Some(message.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(started.len(), 2, "one assistant message per model turn");
        assert_eq!(started[0].turn_ordinal, 1);
        assert_eq!(started[1].turn_ordinal, 2);
        assert_ne!(started[0].id, started[1].id);
        // The second turn's message starts only after the first turn's tool
        // call finished: text and calls replay in true execution order.
        let first_started = observed
            .iter()
            .position(|event| {
                matches!(&event.event, SessionEvent::AssistantMessageStarted { message }
                    if message.id == started[0].id)
            })
            .unwrap();
        let call_requested = observed
            .iter()
            .position(|event| matches!(event.event, SessionEvent::ToolCallRequested { .. }))
            .unwrap();
        let call_finished = observed
            .iter()
            .position(|event| matches!(event.event, SessionEvent::ToolCallFinished { .. }))
            .unwrap();
        let second_started = observed
            .iter()
            .position(|event| {
                matches!(&event.event, SessionEvent::AssistantMessageStarted { message }
                    if message.id == started[1].id)
            })
            .unwrap();
        assert!(first_started < call_requested);
        assert!(call_requested < call_finished);
        assert!(call_finished < second_started);

        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 1,
                message_limit: 8,
            })
            .await
            .unwrap();
        let focused = snapshot.focused.unwrap();
        assert_eq!(focused.messages.len(), 3);
        assert_eq!(focused.messages[0].role, MessageRole::User);
        assert_eq!(focused.messages[0].turn_ordinal, 0);
        assert_eq!(focused.messages[1].role, MessageRole::Assistant);
        assert_eq!(focused.messages[1].turn_ordinal, 1);
        assert_eq!(focused.messages[1].output, "Let me look. ");
        assert_eq!(focused.messages[1].state, MessageState::Complete);
        assert_eq!(focused.messages[2].turn_ordinal, 2);
        assert_eq!(focused.messages[2].output, "done");
        assert_eq!(focused.messages[2].state, MessageState::Complete);
        assert_eq!(focused.tool_calls.len(), 1);
        assert_eq!(focused.tool_calls[0].turn_ordinal, 1);
    }

    #[tokio::test]
    async fn call_only_turns_persist_no_message_row() {
        let harness = scripted_runs_harness(
            ApprovalMode::Auto,
            vec![vec![("read_file", r#"{"path":"note.txt"}"#.to_owned())]],
        )
        .await;
        std::fs::write(harness.workspace_path.join("note.txt"), "noted\n").unwrap();
        let mut harness = harness;
        submit_prompt(&harness, "read the note").await;
        let observed = collect_through_finished(&mut harness.events).await;
        let started = observed
            .iter()
            .filter_map(|event| match &event.event {
                SessionEvent::AssistantMessageStarted { message } => Some(message.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            started.len(),
            1,
            "a call-only turn must not start an assistant message"
        );
        assert_eq!(started[0].turn_ordinal, 2);

        let (workspace_id, _) = resolve_workspace(&harness.runtime, &harness.workspace_path).await;
        let snapshot = harness
            .runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(harness.session_id),
                session_limit: 1,
                message_limit: 8,
            })
            .await
            .unwrap();
        let focused = snapshot.focused.unwrap();
        assert_eq!(
            focused.messages.len(),
            2,
            "turn one requested a call without text, so it persists no message row"
        );
        assert_eq!(focused.messages[0].role, MessageRole::User);
        assert_eq!(focused.messages[1].role, MessageRole::Assistant);
        assert_eq!(focused.messages[1].turn_ordinal, 2);
        assert_eq!(focused.messages[1].output, "done");
        assert_eq!(focused.tool_calls[0].turn_ordinal, 1);
    }

    #[tokio::test]
    async fn recovery_interrupts_only_the_current_turns_message() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let store = Store::open(database_path.clone()).await.unwrap();
        let resolved = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::ResolveWorkspace {
                    path: directory.path().to_str().unwrap().to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.receipt.outcome else {
            panic!("unexpected receipt")
        };
        let created = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: ApprovalMode::default(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
            panic!("unexpected receipt")
        };
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "read".to_owned(),
                },
            )
            .await
            .unwrap();
        let claimed = store.claim_next_run(false).await.unwrap().unwrap();
        // Turn one: streamed text, then a completed tool call; the turn
        // committed, finalizing its message.
        let first_message = MessageId::generate().unwrap();
        store
            .begin_assistant_message(
                &claimed,
                first_message,
                1,
                TextChannel::Output,
                "Checking. ".to_owned(),
            )
            .await
            .unwrap();
        let tool_call_id = ToolCallId::generate().unwrap();
        let call = RuntimeToolCall {
            id: tool_call_id,
            turn_ordinal: 1,
            call_ordinal: 1,
            provider_call_id: "call_0".to_owned(),
            name: "read_file".to_owned(),
            arguments: r#"{"path":"note.txt"}"#.to_owned(),
            argument_error: None,
        };
        store
            .persist_model_turn(
                &claimed,
                ModelTurnCommit {
                    turn_ordinal: 1,
                    message: Message::new(
                        Role::Assistant,
                        vec![
                            ContentBlock::Text {
                                text: "Checking. ".to_owned(),
                            },
                            ContentBlock::ToolCall {
                                id: call.provider_call_id.clone(),
                                name: call.name.clone(),
                                arguments: serde_json::from_str(&call.arguments).unwrap(),
                            },
                        ],
                    ),
                    calls: vec![call],
                    turn_message: Some(first_message),
                    context_tokens: None,
                    usage: None,
                    estimated_cost_usd_nanos: None,
                    accounting: None,
                },
            )
            .await
            .unwrap();
        store.start_tool_call(&claimed, tool_call_id).await.unwrap();
        store
            .finish_tool_call(
                &claimed,
                tool_call_id,
                "noted\n".to_owned(),
                false,
                None,
                None,
            )
            .await
            .unwrap();
        // Turn two starts streaming, then the server crashes.
        let second_message = MessageId::generate().unwrap();
        store
            .begin_assistant_message(
                &claimed,
                second_message,
                2,
                TextChannel::Output,
                "So far".to_owned(),
            )
            .await
            .unwrap();
        drop(store);

        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(database_path),
            Arc::new(ScriptedLoader),
        )
        .await
        .unwrap();
        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 1,
                message_limit: 8,
            })
            .await
            .unwrap();
        let focused = snapshot.focused.unwrap();
        assert_eq!(focused.messages.len(), 3);
        let first = focused
            .messages
            .iter()
            .find(|message| message.id == first_message)
            .unwrap();
        let second = focused
            .messages
            .iter()
            .find(|message| message.id == second_message)
            .unwrap();
        assert_eq!(
            first.state,
            MessageState::Complete,
            "the committed turn's message must survive recovery untouched"
        );
        assert_eq!(second.state, MessageState::Interrupted);
        assert_eq!(focused.runs[0].outcome, Some(RunOutcome::Interrupted));
    }

    #[tokio::test]
    async fn recovery_interrupts_running_tools_without_reexecuting_them() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let store = Store::open(database_path.clone()).await.unwrap();
        let resolved = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::ResolveWorkspace {
                    path: directory.path().to_str().unwrap().to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.receipt.outcome else {
            panic!("unexpected receipt")
        };
        let created = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: ApprovalMode::default(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
            panic!("unexpected receipt")
        };
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "read".to_owned(),
                },
            )
            .await
            .unwrap();
        let claimed = store.claim_next_run(false).await.unwrap().unwrap();
        let tool_call_id = ToolCallId::generate().unwrap();
        // A mutating call crashed mid-execution: replay must surface an
        // explicit interrupted result, never re-run the side effect.
        let call = RuntimeToolCall {
            id: tool_call_id,
            turn_ordinal: 1,
            call_ordinal: 1,
            provider_call_id: "provider-call".to_owned(),
            name: "edit_file".to_owned(),
            arguments: r#"{"path":"note.txt","old_string":"a","new_string":"b"}"#.to_owned(),
            argument_error: None,
        };
        store
            .persist_model_turn(
                &claimed,
                ModelTurnCommit {
                    turn_ordinal: 1,
                    message: Message::new(
                        Role::Assistant,
                        vec![ContentBlock::ToolCall {
                            id: call.provider_call_id.clone(),
                            name: call.name.clone(),
                            arguments: serde_json::from_str(&call.arguments).unwrap(),
                        }],
                    ),
                    calls: vec![call],
                    turn_message: None,
                    context_tokens: None,
                    usage: None,
                    estimated_cost_usd_nanos: None,
                    accounting: None,
                },
            )
            .await
            .unwrap();
        let started = store.start_tool_call(&claimed, tool_call_id).await.unwrap();
        drop(store);

        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(database_path),
            Arc::new(CapturingLoader {
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: started.cursor,
            })
            .unwrap();
        let recovered = collect_through_finished(&mut events).await;
        assert!(matches!(
            &recovered[0].event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.id == tool_call_id
                    && tool_call.state == ToolCallState::Interrupted
                    && tool_call.is_error
        ));
        assert!(matches!(
            &recovered[1].event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Interrupted,
                ..
            }
        ));
        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 1,
                message_limit: 4,
            })
            .await
            .unwrap();
        assert_eq!(
            snapshot.focused.unwrap().tool_calls[0].state,
            ToolCallState::Interrupted
        );

        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "continue".to_owned(),
                },
            )
            .await
            .unwrap();
        let _ = collect_through_finished(&mut events).await;

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(matches!(
            requests[0].messages()[2].content(),
            [ContentBlock::ToolResult {
                call_id,
                content,
                is_error: true,
            }] if call_id == "provider-call" && content == INTERRUPTED_TOOL_RESULT
        ));
    }

    #[tokio::test]
    async fn orphaned_tool_call_blocks_replay_with_synthesized_interrupted_results() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let store = Store::open(database_path.clone()).await.unwrap();
        let resolved = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::ResolveWorkspace {
                    path: directory.path().to_str().unwrap().to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.receipt.outcome else {
            panic!("unexpected receipt")
        };
        let created = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: ApprovalMode::default(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
            panic!("unexpected receipt")
        };
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "read".to_owned(),
                },
            )
            .await
            .unwrap();
        let claimed = store.claim_next_run(false).await.unwrap().unwrap();
        // Simulate the pre-fix crash window: the model turn committed with a
        // ToolCall block, but no tool_calls rows were ever written.
        store
            .persist_model_turn(
                &claimed,
                ModelTurnCommit {
                    turn_ordinal: 1,
                    message: Message::new(
                        Role::Assistant,
                        vec![ContentBlock::ToolCall {
                            id: "orphan-call".to_owned(),
                            name: "read_file".to_owned(),
                            arguments: serde_json::json!({"path": "note.txt"}),
                        }],
                    ),
                    calls: Vec::new(),
                    turn_message: None,
                    context_tokens: None,
                    usage: None,
                    estimated_cost_usd_nanos: None,
                    accounting: None,
                },
            )
            .await
            .unwrap();
        drop(store);

        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(database_path),
            Arc::new(CapturingLoader {
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.receipt.committed_through,
            })
            .unwrap();
        let _ = collect_through_finished(&mut events).await;
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "continue".to_owned(),
                },
            )
            .await
            .unwrap();
        let _ = collect_through_finished(&mut events).await;

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let messages = requests[0].messages();
        assert!(matches!(
            messages[1].content(),
            [ContentBlock::ToolCall { id, .. }] if id == "orphan-call"
        ));
        assert!(matches!(
            messages[2].content(),
            [ContentBlock::ToolResult {
                call_id,
                content,
                is_error: true,
            }] if call_id == "orphan-call" && content == INTERRUPTED_TOOL_RESULT
        ));
        assert_tool_results_are_exact(messages);
    }

    struct ContextBudgetLoader;

    impl RuntimeLoader for ContextBudgetLoader {
        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            Box::pin(async {
                Runtime::new(ContextBudgetProvider, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(runtime),
                        pricing: None,
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct ContextBudgetProvider;

    impl Provider for ContextBudgetProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "x".repeat(MAX_CONTEXT_BYTES + 1),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
            ]))
        }
    }

    #[tokio::test]
    async fn exceeding_the_context_budget_fails_the_run_with_a_policy_outcome() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(ContextBudgetLoader),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "fill the context".to_owned(),
                },
            )
            .await
            .unwrap();

        let observed = collect_through_finished(&mut events).await;
        let finished = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::RunFinished { outcome, .. } => Some(outcome.clone()),
                _ => None,
            })
            .unwrap();
        assert!(matches!(
            finished,
            RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::Policy,
                    ref message,
                }
            } if message.contains("4 MiB limit")
        ));
    }

    #[tokio::test]
    async fn a_panicking_run_task_fails_durably_and_the_session_keeps_scheduling() {
        struct PanicOnceLoader {
            calls: Arc<AtomicUsize>,
        }

        impl RuntimeLoader for PanicOnceLoader {
            fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
                let calls = Arc::clone(&self.calls);
                Box::pin(async move {
                    struct PanicOnceProvider {
                        calls: Arc<AtomicUsize>,
                    }

                    impl Provider for PanicOnceProvider {
                        fn stream(&self, _request: ModelRequest) -> ProviderStream {
                            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                                panic!("injected run-task panic");
                            }
                            Box::pin(stream::iter([
                                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                                    text: "recovered".to_owned(),
                                }),
                                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                            ]))
                        }
                    }

                    Runtime::new(PanicOnceProvider { calls }, "test-model", 256)
                        .map(|runtime| LoadedRuntime {
                            runtime: Arc::new(runtime),
                            pricing: None,
                        })
                        .map_err(|error| RuntimeLoadError {
                            kind: RunFailureKind::Configuration,
                            message: error.to_string(),
                        })
                })
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(PanicOnceLoader {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();

        let failed_run = queue_prompt(&runtime, session_id, "panic".to_owned()).await;
        let failed = tokio::time::timeout(
            Duration::from_secs(1),
            collect_through_finished(&mut events),
        )
        .await
        .expect("a panicking run must settle durably within one second");
        assert!(failed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished {
                run_id,
                outcome: RunOutcome::Failed {
                    failure: RunFailure {
                        kind: RunFailureKind::Server,
                        ..
                    }
                },
                ..
            } if *run_id == failed_run
        )));

        let continued_run = queue_prompt(&runtime, session_id, "continue".to_owned()).await;
        let continued = collect_through_finished(&mut events).await;
        assert!(continued.iter().any(|event| matches!(
            event.event,
            SessionEvent::RunFinished {
                run_id,
                outcome: RunOutcome::Completed,
                ..
            } if run_id == continued_run
        )));
    }

    #[tokio::test]
    async fn shutdown_cancels_running_and_queued_prompts_before_returning() {
        let directory = tempfile::tempdir().unwrap();
        let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
        options.max_active_runs = 1;
        let runtime = SessionRuntime::open(options, Arc::new(PricedHangingLoader))
            .await
            .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();

        let running = queue_prompt(&runtime, session_id, "run".to_owned()).await;
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let event = events.next().await.unwrap().unwrap();
                if matches!(
                    event.event,
                    SessionEvent::RunStarted { run_id, .. } if run_id == running
                ) {
                    break;
                }
            }
        })
        .await
        .expect("the first prompt must start before shutdown");
        let queued = queue_prompt(&runtime, session_id, "queued".to_owned()).await;

        tokio::time::timeout(Duration::from_secs(1), runtime.shutdown())
            .await
            .expect("shutdown must settle bounded provider work")
            .unwrap();

        let mut finished = HashMap::new();
        let mut terminal_count = 0;
        tokio::time::timeout(Duration::from_secs(1), async {
            while finished.len() < 2 {
                let event = events.next().await.unwrap().unwrap();
                if let SessionEvent::RunFinished {
                    run_id, outcome, ..
                } = event.event
                {
                    terminal_count += 1;
                    finished.insert(run_id, outcome);
                }
            }
        })
        .await
        .expect("both accepted prompts must publish terminal events");
        assert_eq!(terminal_count, 2);
        assert_eq!(finished.get(&running), Some(&RunOutcome::Cancelled));
        assert_eq!(finished.get(&queued), Some(&RunOutcome::Cancelled));

        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 1,
                message_limit: 4,
            })
            .await
            .unwrap();
        let focused = snapshot.focused.unwrap();
        assert_eq!(focused.summary.status, SessionStatus::Idle);
        assert_eq!(focused.summary.active_run_id, None);
        assert!(
            focused
                .runs
                .iter()
                .all(|run| run.status == RunStatus::Cancelled)
        );

        let error = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "too late".to_owned(),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error, SessionRuntimeError::Unavailable);
    }

    #[tokio::test]
    async fn subscribers_converge_and_replay_from_an_intermediate_cursor() {
        let (directory, runtime) = test_runtime().await;
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let request = SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        };
        let mut first = runtime.subscribe(request).unwrap();
        let mut second = runtime.subscribe(request).unwrap();

        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "converge".to_owned(),
                },
            )
            .await
            .unwrap();

        let (first, second) = tokio::join!(
            collect_through_finished(&mut first),
            collect_through_finished(&mut second),
        );
        assert_eq!(first, second);
        assert!(first.len() > 2);

        let split = first.len() / 2;
        let mut replay = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: first[split - 1].cursor,
            })
            .unwrap();
        let replayed = tokio::time::timeout(Duration::from_secs(2), async {
            let mut replayed = Vec::new();
            for _ in split..first.len() {
                replayed.push(replay.next().await.unwrap().unwrap());
            }
            replayed
        })
        .await
        .unwrap();

        assert_eq!(replayed, first[split..]);
    }

    #[tokio::test]
    async fn scheduler_store_failure_disables_runtime_and_existing_subscribers() {
        let (directory, runtime) = test_runtime().await;
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        runtime
            .inner
            .store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "persist me".to_owned(),
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            events.next().await.unwrap().unwrap().event,
            SessionEvent::PromptQueued { .. }
        ));

        let worker = runtime.inner.store.stop_worker_for_test().unwrap();
        tokio::task::spawn_blocking(move || worker.join().unwrap())
            .await
            .unwrap();

        let mut failed = runtime.inner.failed.subscribe();
        runtime.request_schedule();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !*failed.borrow() {
                failed.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        assert_eq!(
            events.next().await,
            Some(Err(SessionRuntimeError::Unavailable))
        );
        assert_eq!(
            runtime
                .snapshot(SnapshotRequest {
                    workspace_id,
                    focused_session_id: Some(session_id),
                    session_limit: 1,
                    message_limit: 1,
                })
                .await
                .unwrap_err(),
            SessionRuntimeError::Unavailable
        );
        assert_eq!(
            runtime
                .subscribe(SubscribeRequest {
                    workspace_id,
                    after: created.committed_through,
                })
                .err(),
            Some(SessionRuntimeError::Unavailable)
        );
    }

    #[tokio::test]
    async fn queues_follow_ups_without_reordering_conversation_context() {
        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions {
                database_path: directory.path().join("sessions.sqlite3"),
                max_active_runs: 1,
                approval_timeout: DEFAULT_APPROVAL_TIMEOUT,
                grant_authority: None,
                approval_reviewer: None,
            },
            Arc::new(CapturingLoader {
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();

        for prompt in ["first", "second"] {
            runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::SubmitPrompt {
                        session_id,
                        prompt: prompt.to_owned(),
                    },
                )
                .await
                .unwrap();
        }
        let mut finished = 0;
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(event) = events.next().await {
                if matches!(event.unwrap().event, SessionEvent::RunFinished { .. }) {
                    finished += 1;
                    if finished == 2 {
                        break;
                    }
                }
            }
        })
        .await
        .unwrap();

        let captured = requests.lock().unwrap();
        assert_eq!(captured.len(), 2);
        assert_eq!(
            captured[1].messages(),
            [
                Message::user("first"),
                Message::assistant("answer"),
                Message::user("second"),
            ]
        );
    }

    #[tokio::test]
    async fn preserves_cancellation_requested_before_runtime_registration() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let command_id = CommandId::generate().unwrap();
        let resolved = store
            .command(
                command_id,
                SessionCommand::ResolveWorkspace {
                    path: directory.path().to_str().unwrap().to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.receipt.outcome else {
            panic!("unexpected receipt")
        };
        let created = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: ApprovalMode::default(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
            panic!("unexpected receipt")
        };
        let queued = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "wait".to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::PromptQueued { run_id, .. } = queued.receipt.outcome else {
            panic!("unexpected receipt")
        };

        let claimed = store.claim_next_run(false).await.unwrap().unwrap();
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CancelRun { run_id },
            )
            .await
            .unwrap();

        assert!(store.cancellation_requested(run_id).await.unwrap());
        store
            .finish_run(&claimed, RunOutcome::Completed, None)
            .await
            .unwrap();
        let run = store
            .call(Priority::Control, move |connection| {
                load_run(connection, run_id)
            })
            .await
            .unwrap();
        assert_eq!(run.outcome, Some(RunOutcome::Cancelled));
    }

    #[tokio::test]
    async fn chunks_large_deltas_and_ignores_empty_deltas() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(ChunkingLoader),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "large".to_owned(),
                },
            )
            .await
            .unwrap();

        let mut chunks = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(event) = events.next().await {
                match event.unwrap().event {
                    SessionEvent::TextAppended { text, .. } => chunks.push(text),
                    SessionEvent::RunFinished { .. } => break,
                    _ => {}
                }
            }
        })
        .await
        .unwrap();

        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|chunk| !chunk.is_empty()));
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.len() <= MAX_TEXT_CHUNK_BYTES)
        );
        assert_eq!(chunks.concat(), "é".repeat(MAX_TEXT_CHUNK_BYTES / 2 + 8));
    }

    #[tokio::test]
    async fn rejects_cross_workspace_focus_and_oversized_pages() {
        let (directory, runtime) = test_runtime().await;
        let second = tempfile::tempdir().unwrap();
        let (first_workspace, _) = resolve_workspace(&runtime, directory.path()).await;
        let (second_workspace, _) = resolve_workspace(&runtime, second.path()).await;
        let created = create_session(&runtime, second_workspace, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };

        assert_eq!(
            runtime
                .snapshot(SnapshotRequest {
                    workspace_id: first_workspace,
                    focused_session_id: Some(session_id),
                    session_limit: 32,
                    message_limit: 32,
                })
                .await
                .unwrap_err(),
            SessionRuntimeError::SessionNotFound
        );
        assert_eq!(
            runtime
                .snapshot(SnapshotRequest {
                    workspace_id: first_workspace,
                    focused_session_id: None,
                    session_limit: MAX_SNAPSHOT_SESSIONS + 1,
                    message_limit: 1,
                })
                .await
                .unwrap_err(),
            SessionRuntimeError::InvalidPageLimit
        );
    }

    #[tokio::test]
    async fn schedules_ready_sessions_fairly() {
        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions {
                database_path: directory.path().join("sessions.sqlite3"),
                max_active_runs: 1,
                approval_timeout: DEFAULT_APPROVAL_TIMEOUT,
                grant_authority: None,
                approval_reviewer: None,
            },
            Arc::new(CapturingLoader {
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let first = create_session(&runtime, workspace_id, None).await;
        let second = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated {
            session_id: first_session,
        } = first.outcome
        else {
            panic!("unexpected receipt")
        };
        let CommandOutcome::SessionCreated {
            session_id: second_session,
        } = second.outcome
        else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: second.committed_through,
            })
            .unwrap();
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id: first_session,
                    prompt: "first-a".to_owned(),
                },
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(event) = events.next().await {
                if matches!(event.unwrap().event, SessionEvent::RunStarted { .. }) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        for (session_id, prompt) in [(first_session, "first-b"), (second_session, "second-a")] {
            runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::SubmitPrompt {
                        session_id,
                        prompt: prompt.to_owned(),
                    },
                )
                .await
                .unwrap();
        }
        let mut finished = 0;
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(event) = events.next().await {
                if matches!(event.unwrap().event, SessionEvent::RunFinished { .. }) {
                    finished += 1;
                    if finished == 3 {
                        break;
                    }
                }
            }
        })
        .await
        .unwrap();

        let captured = requests.lock().unwrap();
        assert_eq!(captured.len(), 3);
        assert_eq!(
            captured[0].messages().last(),
            Some(&Message::user("first-a"))
        );
        assert_eq!(
            captured[1].messages().last(),
            Some(&Message::user("second-a"))
        );
        assert_eq!(
            captured[2].messages().last(),
            Some(&Message::user("first-b"))
        );
    }

    #[tokio::test]
    async fn immediate_approval_response_cannot_race_past_registered_waiter() {
        let mut harness = approval_harness(
            ApprovalMode::Ask,
            "__test_mutate",
            "{}",
            1,
            DEFAULT_APPROVAL_TIMEOUT,
        )
        .await;
        let (observed, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        assert!(
            observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolCallRequested { .. }))
        );
        assert_eq!(tool_call.state, ToolCallState::AwaitingApproval);

        let receipt = respond_approval(
            &harness.runtime,
            harness.run_id,
            tool_call.id,
            ApprovalDecision::ApproveOnce,
        )
        .await
        .unwrap();
        assert_eq!(
            receipt.outcome,
            CommandOutcome::ToolApprovalResolved {
                tool_call_id: tool_call.id,
                resolution: ApprovalResolution::ApprovedOnce,
            }
        );

        let observed = collect_through_finished(&mut harness.events).await;
        assert!(matches!(
            &observed[0].event,
            SessionEvent::ToolApprovalResolved {
                resolution: ApprovalResolution::ApprovedOnce,
                ..
            }
        ));
        assert!(
            observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolCallStarted { .. }))
        );
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.state == ToolCallState::Completed
                    && tool_call.result.as_deref() == Some("mutated")
        )));
        assert!(matches!(
            &observed.last().unwrap().event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            }
        ));
        let requests = harness.requests.lock().unwrap();
        assert!(matches!(
            requests[1].messages()[2].content(),
            [ContentBlock::ToolResult {
                content,
                is_error: false,
                ..
            }] if content == "mutated"
        ));
    }

    #[tokio::test]
    async fn denial_returns_a_tool_error_and_the_run_still_completes() {
        let mut harness = approval_harness(
            ApprovalMode::Ask,
            "__test_mutate",
            "{}",
            1,
            DEFAULT_APPROVAL_TIMEOUT,
        )
        .await;
        let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        respond_approval(
            &harness.runtime,
            harness.run_id,
            tool_call.id,
            ApprovalDecision::Deny,
        )
        .await
        .unwrap();

        let observed = collect_through_finished(&mut harness.events).await;
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolApprovalResolved {
                tool_call,
                resolution: ApprovalResolution::Denied,
            } if tool_call.state == ToolCallState::Denied && tool_call.is_error
        )));
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolCallStarted { .. }))
        );
        assert!(matches!(
            &observed.last().unwrap().event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            }
        ));
        let denied_result = {
            let requests = harness.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            match requests[1].messages()[2].content() {
                [
                    ContentBlock::ToolResult {
                        content,
                        is_error: true,
                        ..
                    },
                ] => content.clone(),
                other => panic!("unexpected tool result content {other:?}"),
            }
        };
        assert_eq!(denied_result, approval::USER_DENIED_RESULT);
        let snapshot = harness
            .runtime
            .snapshot(SnapshotRequest {
                workspace_id: harness.workspace_id,
                focused_session_id: Some(harness.session_id),
                session_limit: 1,
                message_limit: 8,
            })
            .await
            .unwrap();
        assert_eq!(
            snapshot.focused.unwrap().tool_calls[0].state,
            ToolCallState::Denied
        );
    }

    #[tokio::test]
    async fn responding_twice_returns_the_recorded_outcome_without_side_effects() {
        let mut harness = approval_harness(
            ApprovalMode::Ask,
            "__test_mutate",
            "{}",
            1,
            DEFAULT_APPROVAL_TIMEOUT,
        )
        .await;
        let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        respond_approval(
            &harness.runtime,
            harness.run_id,
            tool_call.id,
            ApprovalDecision::Deny,
        )
        .await
        .unwrap();
        let _ = collect_through_finished(&mut harness.events).await;

        // A retry with a different decision returns the recorded denial.
        let retry = respond_approval(
            &harness.runtime,
            harness.run_id,
            tool_call.id,
            ApprovalDecision::ApproveOnce,
        )
        .await
        .unwrap();
        assert_eq!(
            retry.outcome,
            CommandOutcome::ToolApprovalResolved {
                tool_call_id: tool_call.id,
                resolution: ApprovalResolution::Denied,
            }
        );
        let snapshot = harness
            .runtime
            .snapshot(SnapshotRequest {
                workspace_id: harness.workspace_id,
                focused_session_id: Some(harness.session_id),
                session_limit: 1,
                message_limit: 8,
            })
            .await
            .unwrap();
        assert_eq!(
            snapshot.focused.unwrap().tool_calls[0].state,
            ToolCallState::Denied
        );

        assert_eq!(
            respond_approval(
                &harness.runtime,
                harness.run_id,
                ToolCallId::generate().unwrap(),
                ApprovalDecision::ApproveOnce,
            )
            .await
            .unwrap_err(),
            SessionRuntimeError::ToolCallNotFound
        );
    }

    #[tokio::test]
    async fn unresolved_approvals_are_denied_by_timeout_with_a_distinct_error() {
        let mut harness = approval_harness(
            ApprovalMode::Ask,
            "__test_mutate",
            "{}",
            1,
            Duration::from_millis(50),
        )
        .await;
        let (_, _) = collect_until_approval_requested(&mut harness.events).await;
        let observed = collect_through_finished(&mut harness.events).await;
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolApprovalResolved {
                tool_call,
                resolution: ApprovalResolution::DeniedTimeout,
            } if tool_call.state == ToolCallState::Denied
        )));
        assert!(matches!(
            &observed.last().unwrap().event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            }
        ));
        let requests = harness.requests.lock().unwrap();
        assert!(matches!(
            requests[1].messages()[2].content(),
            [ContentBlock::ToolResult {
                content,
                is_error: true,
                ..
            }] if content == approval::TIMEOUT_DENIED_RESULT
        ));
    }

    #[tokio::test]
    async fn approve_for_session_grants_cover_later_calls_without_prompting() {
        let mut harness = approval_harness(
            ApprovalMode::Ask,
            "__test_mutate",
            "{}",
            2,
            DEFAULT_APPROVAL_TIMEOUT,
        )
        .await;
        let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        respond_approval(
            &harness.runtime,
            harness.run_id,
            tool_call.id,
            ApprovalDecision::ApproveForSession {
                grant: ApprovalGrant::Tool {
                    name: "__test_mutate".to_owned(),
                },
            },
        )
        .await
        .unwrap();

        let observed = collect_through_finished(&mut harness.events).await;
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. })),
            "the session grant must cover the second call"
        );
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(
                    &event.event,
                    SessionEvent::ToolCallFinished { tool_call }
                        if tool_call.state == ToolCallState::Completed
                ))
                .count(),
            2
        );
        assert!(matches!(
            &observed.last().unwrap().event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            }
        ));
    }

    /// Scripted grant authority: hands every session a fixed seed and
    /// answers promotions with a fixed outcome, recording every request.
    struct ScriptedGrantAuthority {
        seed: WorkspaceGrantSeed,
        outcome: WorkspaceGrantOutcome,
        seeded: StdMutex<Vec<PathBuf>>,
        promotions: StdMutex<Vec<(PathBuf, ApprovalGrant)>>,
    }

    impl ScriptedGrantAuthority {
        fn new(seed: WorkspaceGrantSeed, outcome: WorkspaceGrantOutcome) -> Arc<Self> {
            Arc::new(Self {
                seed,
                outcome,
                seeded: StdMutex::new(Vec::new()),
                promotions: StdMutex::new(Vec::new()),
            })
        }
    }

    impl WorkspaceGrantAuthority for ScriptedGrantAuthority {
        fn seed_grants(&self, workspace: &Path) -> GrantSeedFuture {
            self.seeded.lock().unwrap().push(workspace.to_owned());
            Box::pin(std::future::ready(self.seed.clone()))
        }

        fn promote_grant(&self, workspace: &Path, grant: &ApprovalGrant) -> GrantPromotionFuture {
            self.promotions
                .lock()
                .unwrap()
                .push((workspace.to_owned(), grant.clone()));
            Box::pin(std::future::ready(self.outcome.clone()))
        }
    }

    /// The `workspace_grant_promoted` event for a responded approval. It is
    /// published by a background task, so it may land before or after the
    /// run's terminal event: check what was already collected, then poll.
    async fn grant_promotion_event(
        observed: &[SessionEventEnvelope],
        events: &mut SessionEventStream,
    ) -> SessionEventEnvelope {
        if let Some(event) = observed
            .iter()
            .find(|event| matches!(event.event, SessionEvent::WorkspaceGrantPromoted { .. }))
        {
            return event.clone();
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let event = events.next().await.unwrap().unwrap();
                if matches!(event.event, SessionEvent::WorkspaceGrantPromoted { .. }) {
                    return event;
                }
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn config_grants_seed_new_sessions_and_cover_calls_without_prompting() {
        let authority = ScriptedGrantAuthority::new(
            WorkspaceGrantSeed {
                tools: vec!["mcp__notes__search".to_owned()],
                shell_prefixes: vec!["cargo test".to_owned()],
            },
            WorkspaceGrantOutcome::Failed {
                message: "unused".to_owned(),
            },
        );
        let mut harness = scripted_runs_harness_with_authority(
            ApprovalMode::Ask,
            vec![vec![
                (
                    "__test_shell",
                    r#"{"command":"cargo test -p qq-core"}"#.to_owned(),
                ),
                ("mcp__notes__search", "{}".to_owned()),
            ]],
            Some(authority.clone()),
        )
        .await;
        submit_prompt(&harness, "run both granted tools").await;

        let observed = collect_through_finished(&mut harness.events).await;
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. })),
            "config-seeded grants must cover both calls without prompting"
        );
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.name == "__test_shell"
                    && tool_call.state == ToolCallState::Completed
        )));
        // The exact-name MCP grant passed the gate; with no registry attached
        // the dispatch then fails, but the call was never held for approval.
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.name == "mcp__notes__search"
        )));
        assert!(matches!(
            &observed.last().unwrap().event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            }
        ));
        let seeded = authority.seeded.lock().unwrap();
        assert_eq!(seeded.len(), 1, "one session creation resolves one seed");
        assert_eq!(
            seeded[0],
            std::fs::canonicalize(&harness.workspace_path).unwrap()
        );
    }

    #[tokio::test]
    async fn approve_for_workspace_records_the_session_grant_and_promotes_it() {
        let authority = ScriptedGrantAuthority::new(
            WorkspaceGrantSeed::default(),
            WorkspaceGrantOutcome::Written {
                path: "/w/.qq/config.ron".to_owned(),
            },
        );
        let mut harness = approval_harness_with_authority(
            ApprovalMode::Ask,
            "__test_mutate",
            "{}",
            2,
            DEFAULT_APPROVAL_TIMEOUT,
            Some(authority.clone()),
        )
        .await;
        let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        let command_id = CommandId::generate().unwrap();
        let command = SessionCommand::RespondToolApproval {
            run_id: harness.run_id,
            tool_call_id: tool_call.id,
            decision: ApprovalDecision::ApproveForWorkspace {
                grant: ApprovalGrant::Tool {
                    name: "__test_mutate".to_owned(),
                },
            },
        };
        let receipt = harness
            .runtime
            .command(command_id, command.clone())
            .await
            .unwrap();
        assert!(matches!(
            receipt.outcome,
            CommandOutcome::ToolApprovalResolved {
                resolution: ApprovalResolution::ApprovedForWorkspace,
                ..
            }
        ));

        let observed = collect_through_finished(&mut harness.events).await;
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. })),
            "the recorded session grant must cover the second call"
        );
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(
                    &event.event,
                    SessionEvent::ToolCallFinished { tool_call }
                        if tool_call.state == ToolCallState::Completed
                ))
                .count(),
            2
        );
        assert!(matches!(
            &observed.last().unwrap().event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            }
        ));

        let promoted = grant_promotion_event(&observed, &mut harness.events).await;
        assert_eq!(promoted.caused_by, Some(command_id));
        assert_eq!(promoted.run_id, Some(harness.run_id));
        assert!(matches!(
            &promoted.event,
            SessionEvent::WorkspaceGrantPromoted {
                grant: ApprovalGrant::Tool { name },
                outcome: WorkspaceGrantOutcome::Written { path },
            } if name == "__test_mutate" && path == "/w/.qq/config.ron"
        ));
        {
            let promotions = authority.promotions.lock().unwrap();
            assert_eq!(promotions.len(), 1);
            assert_eq!(
                promotions[0].0,
                std::fs::canonicalize(harness._directory.path()).unwrap()
            );
        }

        // Retrying the same command replays the durable receipt without
        // re-running the promotion.
        let retried = harness.runtime.command(command_id, command).await.unwrap();
        assert_eq!(retried, receipt);
        assert_eq!(authority.promotions.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn failed_workspace_grant_promotion_leaves_the_approval_standing() {
        let authority = ScriptedGrantAuthority::new(
            WorkspaceGrantSeed::default(),
            WorkspaceGrantOutcome::Failed {
                message: "denied by managed policy".to_owned(),
            },
        );
        let mut harness = approval_harness_with_authority(
            ApprovalMode::Ask,
            "__test_mutate",
            "{}",
            1,
            DEFAULT_APPROVAL_TIMEOUT,
            Some(authority),
        )
        .await;
        let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        respond_approval(
            &harness.runtime,
            harness.run_id,
            tool_call.id,
            ApprovalDecision::ApproveForWorkspace {
                grant: ApprovalGrant::Tool {
                    name: "__test_mutate".to_owned(),
                },
            },
        )
        .await
        .unwrap();

        let observed = collect_through_finished(&mut harness.events).await;
        assert!(
            observed.iter().any(|event| matches!(
                &event.event,
                SessionEvent::ToolCallFinished { tool_call }
                    if tool_call.state == ToolCallState::Completed
            )),
            "the approved call must execute despite the failed promotion"
        );
        assert!(matches!(
            &observed.last().unwrap().event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            }
        ));
        let promoted = grant_promotion_event(&observed, &mut harness.events).await;
        assert!(matches!(
            &promoted.event,
            SessionEvent::WorkspaceGrantPromoted {
                outcome: WorkspaceGrantOutcome::Failed { message },
                ..
            } if message == "denied by managed policy"
        ));
    }

    #[tokio::test]
    async fn approve_for_workspace_without_an_authority_reports_a_failed_promotion() {
        let mut harness = approval_harness(
            ApprovalMode::Ask,
            "__test_mutate",
            "{}",
            1,
            DEFAULT_APPROVAL_TIMEOUT,
        )
        .await;
        let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        respond_approval(
            &harness.runtime,
            harness.run_id,
            tool_call.id,
            ApprovalDecision::ApproveForWorkspace {
                grant: ApprovalGrant::Tool {
                    name: "__test_mutate".to_owned(),
                },
            },
        )
        .await
        .unwrap();
        let observed = collect_through_finished(&mut harness.events).await;
        assert!(matches!(
            &observed.last().unwrap().event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            }
        ));
        let promoted = grant_promotion_event(&observed, &mut harness.events).await;
        assert!(matches!(
            &promoted.event,
            SessionEvent::WorkspaceGrantPromoted {
                outcome: WorkspaceGrantOutcome::Failed { message },
                ..
            } if message.contains("no workspace grant store")
        ));
    }

    #[tokio::test]
    async fn read_only_sessions_deny_mutating_tools_without_prompting() {
        let mut harness = approval_harness(
            ApprovalMode::ReadOnly,
            "__test_mutate",
            "{}",
            1,
            DEFAULT_APPROVAL_TIMEOUT,
        )
        .await;
        let observed = collect_through_finished(&mut harness.events).await;
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. }))
        );
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.state == ToolCallState::Denied
                    && tool_call.result.as_deref() == Some(approval::POLICY_DENIED_RESULT)
        )));
        assert!(matches!(
            &observed.last().unwrap().event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn shell_approval_requests_carry_the_command_and_auto_mode_asks_for_dangerous_shell() {
        let mut harness = approval_harness(
            ApprovalMode::Auto,
            "__test_shell",
            r#"{"command":"git push --force origin main","cwd":"crates"}"#,
            1,
            DEFAULT_APPROVAL_TIMEOUT,
        )
        .await;
        let (observed, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        let shell = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::ToolApprovalRequested { shell, .. } => shell.clone(),
                _ => None,
            })
            .expect("shell approval requests carry the command");
        assert_eq!(shell.command, "git push --force origin main");
        assert_eq!(shell.cwd.as_deref(), Some("crates"));

        respond_approval(
            &harness.runtime,
            harness.run_id,
            tool_call.id,
            ApprovalDecision::ApproveForSession {
                grant: ApprovalGrant::ShellPrefix {
                    prefix: "git push".to_owned(),
                },
            },
        )
        .await
        .unwrap();
        let observed = collect_through_finished(&mut harness.events).await;
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.state == ToolCallState::Completed
        )));
    }

    /// A reviewer whose verdict is released by the test: `hold` starts
    /// occupied, and dropping or sending on the release channel lets the
    /// verdict return. Consultations are counted for never-consulted cases.
    struct StubReviewer {
        verdict: ReviewVerdict,
        release: StdMutex<Option<oneshot::Receiver<()>>>,
        consulted: Arc<StdMutex<Vec<ReviewRequest>>>,
    }

    impl StubReviewer {
        fn immediate(verdict: ReviewVerdict) -> (Arc<Self>, Arc<StdMutex<Vec<ReviewRequest>>>) {
            let consulted = Arc::new(StdMutex::new(Vec::new()));
            (
                Arc::new(Self {
                    verdict,
                    release: StdMutex::new(None),
                    consulted: Arc::clone(&consulted),
                }),
                consulted,
            )
        }

        fn held(verdict: ReviewVerdict) -> (Arc<Self>, oneshot::Sender<()>) {
            let (sender, receiver) = oneshot::channel();
            (
                Arc::new(Self {
                    verdict,
                    release: StdMutex::new(Some(receiver)),
                    consulted: Arc::new(StdMutex::new(Vec::new())),
                }),
                sender,
            )
        }
    }

    impl ApprovalReviewer for StubReviewer {
        fn review(&self, request: ReviewRequest) -> ReviewFuture {
            self.consulted.lock().unwrap().push(request);
            let release = self.release.lock().unwrap().take();
            let verdict = self.verdict.clone();
            Box::pin(async move {
                if let Some(release) = release {
                    let _ = release.await;
                }
                verdict
            })
        }
    }

    #[tokio::test]
    async fn reviewer_approval_executes_a_held_call_without_a_client() {
        let (reviewer, consulted) = StubReviewer::immediate(ReviewVerdict::Approve);
        let mut harness = approval_harness_with_reviewer(
            ApprovalMode::Auto,
            "__test_shell",
            r#"{"command":"git push --force origin main"}"#,
            1,
            DEFAULT_APPROVAL_TIMEOUT,
            None,
            Some(reviewer),
        )
        .await;
        let observed = collect_through_finished(&mut harness.events).await;
        assert!(
            observed
                .iter()
                .any(|event| matches!(&event.event, SessionEvent::ToolApprovalRequested { .. }))
        );
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolApprovalResolved {
                resolution: ApprovalResolution::ApprovedByReviewer,
                ..
            }
        )));
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.state == ToolCallState::Completed
        )));
        assert!(matches!(
            &observed.last().unwrap().event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            }
        ));
        let requests = consulted.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].tool_name, "__test_shell");
        assert_eq!(
            requests[0]
                .shell
                .as_ref()
                .map(|shell| shell.command.as_str()),
            Some("git push --force origin main")
        );
    }

    #[tokio::test]
    async fn reviewer_escalation_leaves_the_call_waiting_for_a_client() {
        let (reviewer, _) = StubReviewer::immediate(ReviewVerdict::Escalate {
            reason: "unsure".to_owned(),
        });
        let mut harness = approval_harness_with_reviewer(
            ApprovalMode::Auto,
            "__test_shell",
            r#"{"command":"git push --force origin main"}"#,
            1,
            DEFAULT_APPROVAL_TIMEOUT,
            None,
            Some(reviewer),
        )
        .await;
        let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        assert_eq!(tool_call.state, ToolCallState::AwaitingApproval);
        respond_approval(
            &harness.runtime,
            harness.run_id,
            tool_call.id,
            ApprovalDecision::ApproveOnce,
        )
        .await
        .unwrap();
        let observed = collect_through_finished(&mut harness.events).await;
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolApprovalResolved {
                resolution: ApprovalResolution::ApprovedOnce,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn reviewer_denial_still_lets_the_client_decide() {
        let (reviewer, _) = StubReviewer::immediate(ReviewVerdict::Deny {
            reason: "dangerous".to_owned(),
        });
        let mut harness = approval_harness_with_reviewer(
            ApprovalMode::Auto,
            "__test_shell",
            r#"{"command":"git push --force origin main"}"#,
            1,
            DEFAULT_APPROVAL_TIMEOUT,
            None,
            Some(reviewer),
        )
        .await;
        let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        respond_approval(
            &harness.runtime,
            harness.run_id,
            tool_call.id,
            ApprovalDecision::Deny,
        )
        .await
        .unwrap();
        let observed = collect_through_finished(&mut harness.events).await;
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolApprovalResolved {
                resolution: ApprovalResolution::Denied,
                ..
            }
        )));
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolCallStarted { .. }))
        );
    }

    #[tokio::test]
    async fn client_resolution_wins_over_a_late_reviewer_approval() {
        let (reviewer, release) = StubReviewer::held(ReviewVerdict::Approve);
        let mut harness = approval_harness_with_reviewer(
            ApprovalMode::Auto,
            "__test_shell",
            r#"{"command":"git push --force origin main"}"#,
            1,
            DEFAULT_APPROVAL_TIMEOUT,
            None,
            Some(reviewer),
        )
        .await;
        let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        respond_approval(
            &harness.runtime,
            harness.run_id,
            tool_call.id,
            ApprovalDecision::ApproveOnce,
        )
        .await
        .unwrap();
        // The reviewer answers only after the client's resolution committed.
        let _ = release.send(());
        let observed = collect_through_finished(&mut harness.events).await;
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolApprovalResolved {
                resolution: ApprovalResolution::ApprovedOnce,
                ..
            }
        )));
        assert!(!observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolApprovalResolved {
                resolution: ApprovalResolution::ApprovedByReviewer,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn ask_mode_never_consults_the_reviewer() {
        let (reviewer, consulted) = StubReviewer::immediate(ReviewVerdict::Approve);
        let mut harness = approval_harness_with_reviewer(
            ApprovalMode::Ask,
            "__test_mutate",
            "{}",
            1,
            DEFAULT_APPROVAL_TIMEOUT,
            None,
            Some(reviewer),
        )
        .await;
        let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        assert!(consulted.lock().unwrap().is_empty());
        respond_approval(
            &harness.runtime,
            harness.run_id,
            tool_call.id,
            ApprovalDecision::ApproveOnce,
        )
        .await
        .unwrap();
        collect_through_finished(&mut harness.events).await;
        assert!(consulted.lock().unwrap().is_empty());
    }

    /// Like `collect_through_finished`, with a deadline generous enough for
    /// tests that spawn real child processes.
    #[cfg(unix)]
    async fn collect_through_finished_generously(
        events: &mut SessionEventStream,
    ) -> Vec<SessionEventEnvelope> {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut observed = Vec::new();
            while let Some(event) = events.next().await {
                let event = event.unwrap();
                let finished = matches!(event.event, SessionEvent::RunFinished { .. });
                observed.push(event);
                if finished {
                    break;
                }
            }
            observed
        })
        .await
        .unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn approved_shell_calls_execute_stream_output_and_share_prefix_grants() {
        let mut harness = approval_harness(
            ApprovalMode::Ask,
            "shell",
            r#"{"command":"echo approved-output"}"#,
            2,
            DEFAULT_APPROVAL_TIMEOUT,
        )
        .await;
        let (observed, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        assert_eq!(tool_call.state, ToolCallState::AwaitingApproval);
        let shell = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::ToolApprovalRequested { shell, .. } => shell.clone(),
                _ => None,
            })
            .expect("shell approval requests carry the command preview");
        assert_eq!(shell.command, "echo approved-output");
        assert_eq!(shell.cwd, None);

        respond_approval(
            &harness.runtime,
            harness.run_id,
            tool_call.id,
            ApprovalDecision::ApproveForSession {
                grant: ApprovalGrant::ShellPrefix {
                    prefix: "echo".to_owned(),
                },
            },
        )
        .await
        .unwrap();

        let observed = collect_through_finished_generously(&mut harness.events).await;
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. })),
            "the echo prefix grant must cover the second call"
        );
        let completed = observed
            .iter()
            .filter_map(|event| match &event.event {
                SessionEvent::ToolCallFinished { tool_call }
                    if tool_call.state == ToolCallState::Completed =>
                {
                    Some(tool_call.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(completed.len(), 2);
        for tool_call in &completed {
            let result = tool_call.result.as_deref().unwrap();
            assert!(result.contains("approved-output"), "{result}");
            assert!(result.ends_with("exit code: 0"), "{result}");
        }
        // Live output was published, and before the call's terminal event.
        let first_delta = observed
            .iter()
            .position(|event| {
                matches!(
                    &event.event,
                    SessionEvent::ToolCallOutputDelta { tool_call_id, chunk }
                        if *tool_call_id == completed[0].id && chunk.contains("approved-output")
                )
            })
            .expect("shell output must stream as ToolCallOutputDelta events");
        let finished = observed
            .iter()
            .position(|event| {
                matches!(
                    &event.event,
                    SessionEvent::ToolCallFinished { tool_call } if tool_call.id == completed[0].id
                )
            })
            .unwrap();
        assert!(first_delta < finished);
        assert!(matches!(
            &observed.last().unwrap().event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn read_only_sessions_deny_shell_without_prompting() {
        let mut harness = approval_harness(
            ApprovalMode::ReadOnly,
            "shell",
            r#"{"command":"echo blocked"}"#,
            1,
            DEFAULT_APPROVAL_TIMEOUT,
        )
        .await;
        let observed = collect_through_finished(&mut harness.events).await;
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. }))
        );
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolCallOutputDelta { .. })),
            "a denied command must never produce output"
        );
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.state == ToolCallState::Denied
                    && tool_call.result.as_deref() == Some(approval::POLICY_DENIED_RESULT)
        )));
        assert!(matches!(
            &observed.last().unwrap().event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            }
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_a_run_interrupts_the_running_shell_call() {
        let mut harness = approval_harness(
            ApprovalMode::Ask,
            "shell",
            r#"{"command":"echo running; sleep 30"}"#,
            1,
            DEFAULT_APPROVAL_TIMEOUT,
        )
        .await;
        let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        respond_approval(
            &harness.runtime,
            harness.run_id,
            tool_call.id,
            ApprovalDecision::ApproveOnce,
        )
        .await
        .unwrap();

        // The first live chunk proves the command is running before the run
        // is cancelled; no sleeps are used to sequence the race.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = harness.events.next().await.unwrap().unwrap();
                if matches!(
                    &event.event,
                    SessionEvent::ToolCallOutputDelta { chunk, .. } if chunk.contains("running")
                ) {
                    break;
                }
            }
        })
        .await
        .expect("the approved shell call must stream its first chunk");

        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CancelRun {
                    run_id: harness.run_id,
                },
            )
            .await
            .unwrap();
        let observed = collect_through_finished_generously(&mut harness.events).await;
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call: finished }
                if finished.id == tool_call.id
                    && finished.state == ToolCallState::Interrupted
                    && finished.result.as_deref() == Some(INTERRUPTED_TOOL_RESULT)
        )));
        assert!(matches!(
            &observed.last().unwrap().event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Cancelled,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn edit_approvals_carry_the_diff_preview_and_apply_after_approval() {
        let mut harness = scripted_runs_harness(
            ApprovalMode::Ask,
            vec![vec![
                ("read_file", r#"{"path":"note.txt"}"#.to_owned()),
                (
                    "edit_file",
                    r#"{"path":"note.txt","old_string":"hello world","new_string":"goodbye world"}"#
                        .to_owned(),
                ),
            ]],
        )
        .await;
        let note = harness.workspace_path.join("note.txt");
        std::fs::write(&note, "hello world\n").unwrap();
        let run_id = submit_prompt(&harness, "edit the note").await;

        let (observed, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        let edit = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::ToolApprovalRequested { edit, .. } => edit.clone(),
                _ => None,
            })
            .expect("edit approval requests carry the diff preview");
        assert_eq!(edit.path, "note.txt");
        assert_eq!(edit.diff, "- hello world\n+ goodbye world\n");
        assert_eq!(
            std::fs::read_to_string(&note).unwrap(),
            "hello world\n",
            "nothing may be applied before approval"
        );

        respond_approval(
            &harness.runtime,
            run_id,
            tool_call.id,
            ApprovalDecision::ApproveOnce,
        )
        .await
        .unwrap();
        let observed = collect_through_finished(&mut harness.events).await;
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.name == "edit_file" && tool_call.state == ToolCallState::Completed
        )));
        assert_eq!(std::fs::read_to_string(&note).unwrap(), "goodbye world\n");
    }

    #[tokio::test]
    async fn file_state_survives_restart_and_auto_mode_applies_the_edit_without_prompting() {
        let mut harness = scripted_runs_harness(
            ApprovalMode::Auto,
            vec![vec![("read_file", r#"{"path":"note.txt"}"#.to_owned())]],
        )
        .await;
        let note = harness.workspace_path.join("note.txt");
        std::fs::write(&note, "hello\n").unwrap();

        submit_prompt(&harness, "read the note").await;
        let first = collect_through_finished(&mut harness.events).await;
        assert!(first.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.name == "read_file" && tool_call.state == ToolCallState::Completed
        )));
        let after = first.last().unwrap().cursor;
        let ScriptedRunsHarness {
            _directory,
            runtime,
            workspace_path,
            workspace_id,
            session_id,
            ..
        } = harness;
        runtime.shutdown().await.unwrap();
        drop(runtime);

        let reopened = SessionRuntime::open(
            SessionRuntimeOptions::new(workspace_path.join("sessions.sqlite3")),
            Arc::new(ScriptedRunsLoader {
                requests: Arc::new(StdMutex::new(Vec::new())),
                runs: vec![vec![(
                    "edit_file",
                    r#"{"path":"note.txt","old_string":"hello","new_string":"goodbye"}"#.to_owned(),
                )]],
                loads: StdMutex::new(0),
            }),
        )
        .await
        .unwrap();
        let mut events = reopened
            .subscribe(SubscribeRequest {
                workspace_id,
                after,
            })
            .unwrap();

        // The restarted runtime edits without re-reading: the read-before-write
        // rule is satisfied by the durable file-state map recorded by run one.
        let queued = reopened
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "now edit it".to_owned(),
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            queued.outcome,
            CommandOutcome::PromptQueued { .. }
        ));
        let second = collect_through_finished(&mut events).await;
        assert!(
            !second
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. })),
            "auto mode must not prompt for workspace edits"
        );
        assert!(second.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.name == "edit_file" && tool_call.state == ToolCallState::Completed
        )));
        assert_eq!(std::fs::read_to_string(&note).unwrap(), "goodbye\n");
        reopened.shutdown().await.unwrap();
        drop(_directory);
    }

    #[tokio::test]
    async fn completed_edits_persist_a_display_diff_the_model_context_never_carries() {
        let mut harness = scripted_runs_harness(
            ApprovalMode::Auto,
            vec![
                vec![
                    ("read_file", r#"{"path":"note.txt"}"#.to_owned()),
                    (
                        "edit_file",
                        r#"{"path":"note.txt","old_string":"hello","new_string":"goodbye"}"#
                            .to_owned(),
                    ),
                ],
                Vec::new(),
            ],
        )
        .await;
        let note = harness.workspace_path.join("note.txt");
        std::fs::write(&note, "hello\n").unwrap();

        submit_prompt(&harness, "edit the note").await;
        let observed = collect_through_finished(&mut harness.events).await;
        let finished_call = |name: &str| {
            observed
                .iter()
                .find_map(|event| match &event.event {
                    SessionEvent::ToolCallFinished { tool_call } if tool_call.name == name => {
                        Some(tool_call.clone())
                    }
                    _ => None,
                })
                .unwrap()
        };
        let edited = finished_call("edit_file");
        assert_eq!(edited.state, ToolCallState::Completed);
        // The model-facing result stays the compact summary; the diff rides
        // in the display payload only.
        assert_eq!(
            edited.result.as_deref(),
            Some("Edited note.txt: replaced 1 occurrence(s).")
        );
        assert_eq!(
            edited.display,
            Some(ToolCallDisplay::Diff {
                path: "note.txt".to_owned(),
                diff: "- hello\n+ goodbye\n".to_owned(),
            })
        );
        assert_eq!(finished_call("read_file").display, None);

        // The payload persists with the call and replays in snapshots.
        let (workspace_id, _) = resolve_workspace(&harness.runtime, &harness.workspace_path).await;
        let snapshot = harness
            .runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(harness.session_id),
                session_limit: 1,
                message_limit: 8,
            })
            .await
            .unwrap();
        let focused = snapshot.focused.unwrap();
        let persisted = focused
            .tool_calls
            .iter()
            .find(|call| call.id == edited.id)
            .unwrap();
        assert_eq!(persisted.display, edited.display);

        // A follow-up run reassembles model context from the store: the
        // summary result replays, the display diff never does.
        submit_prompt(&harness, "what changed?").await;
        let _ = collect_through_finished(&mut harness.events).await;
        let requests = harness.requests.lock().unwrap();
        let follow_up = requests.last().unwrap();
        let tool_results = follow_up
            .messages()
            .iter()
            .flat_map(|message| message.content())
            .filter_map(|block| match block {
                ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(tool_results.contains(&"Edited note.txt: replaced 1 occurrence(s)."));
        assert!(
            !tool_results
                .iter()
                .any(|content| content.contains("+ goodbye")),
            "the display diff must never enter model context"
        );
    }

    #[tokio::test]
    async fn auto_mode_executes_mutating_tools_after_a_mode_change() {
        let mut harness = approval_harness(
            ApprovalMode::ReadOnly,
            "__test_mutate",
            "{}",
            1,
            DEFAULT_APPROVAL_TIMEOUT,
        )
        .await;
        // The first run is denied by read-only policy.
        let _ = collect_through_finished(&mut harness.events).await;

        let receipt = harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SetApprovalMode {
                    session_id: harness.session_id,
                    mode: ApprovalMode::Auto,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            receipt.outcome,
            CommandOutcome::ApprovalModeSet {
                session_id: harness.session_id,
                mode: ApprovalMode::Auto,
            }
        );

        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id: harness.session_id,
                    prompt: "mutate again".to_owned(),
                },
            )
            .await
            .unwrap();
        let observed = collect_through_finished(&mut harness.events).await;
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. }))
        );
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.state == ToolCallState::Completed
        )));
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_run_waiting_for_approval() {
        let mut harness = approval_harness(
            ApprovalMode::Ask,
            "__test_mutate",
            "{}",
            1,
            DEFAULT_APPROVAL_TIMEOUT,
        )
        .await;
        let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CancelRun {
                    run_id: harness.run_id,
                },
            )
            .await
            .unwrap();
        let observed = collect_through_finished(&mut harness.events).await;
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call: finished }
                if finished.id == tool_call.id
                    && finished.state == ToolCallState::Interrupted
        )));
        assert!(matches!(
            &observed.last().unwrap().event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Cancelled,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn recovery_marks_awaiting_approval_calls_interrupted() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let store = Store::open(database_path.clone()).await.unwrap();
        let resolved = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::ResolveWorkspace {
                    path: directory.path().to_str().unwrap().to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.receipt.outcome else {
            panic!("unexpected receipt")
        };
        let created = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: ApprovalMode::Ask,
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
            panic!("unexpected receipt")
        };
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "mutate".to_owned(),
                },
            )
            .await
            .unwrap();
        let claimed = store.claim_next_run(false).await.unwrap().unwrap();
        let tool_call_id = ToolCallId::generate().unwrap();
        let call = RuntimeToolCall {
            id: tool_call_id,
            turn_ordinal: 1,
            call_ordinal: 1,
            provider_call_id: "call_0".to_owned(),
            name: "__test_mutate".to_owned(),
            arguments: "{}".to_owned(),
            argument_error: None,
        };
        store
            .persist_model_turn(
                &claimed,
                ModelTurnCommit {
                    turn_ordinal: 1,
                    message: Message::new(
                        Role::Assistant,
                        vec![ContentBlock::ToolCall {
                            id: call.provider_call_id.clone(),
                            name: call.name.clone(),
                            arguments: serde_json::from_str(&call.arguments).unwrap(),
                        }],
                    ),
                    calls: vec![call],
                    turn_message: None,
                    context_tokens: None,
                    usage: None,
                    estimated_cost_usd_nanos: None,
                    accounting: None,
                },
            )
            .await
            .unwrap();
        let awaiting = store
            .request_tool_approval(&claimed, tool_call_id, None, None)
            .await
            .unwrap();
        drop(store);

        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(database_path),
            Arc::new(ScriptedLoader),
        )
        .await
        .unwrap();
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: awaiting.cursor,
            })
            .unwrap();
        let recovered = collect_through_finished(&mut events).await;
        assert!(matches!(
            &recovered[0].event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.id == tool_call_id
                    && tool_call.state == ToolCallState::Interrupted
                    && tool_call.is_error
        ));
        assert!(matches!(
            &recovered[1].event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Interrupted,
                ..
            }
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_a_symlinked_database_and_uses_private_permissions() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("sessions.sqlite3");
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(database.clone()),
            Arc::new(ScriptedLoader),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::metadata(&database).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(runtime);

        let victim = directory.path().join("victim");
        std::fs::write(&victim, b"untouched").unwrap();
        let link = directory.path().join("linked.sqlite3");
        symlink(&victim, &link).unwrap();
        let error =
            match SessionRuntime::open(SessionRuntimeOptions::new(link), Arc::new(ScriptedLoader))
                .await
            {
                Ok(_) => panic!("symlinked database was accepted"),
                Err(error) => error,
            };
        assert_eq!(error, SessionRuntimeError::Persistence);
        assert_eq!(std::fs::read(victim).unwrap(), b"untouched");
    }

    // ----- spawn_agent (sub-agent sessions) -----

    /// Loads providers for spawn tests: model-routed entries first (children
    /// given an explicit model route), then a per-load queue (deterministic
    /// for the single-parent tests: the parent always loads first), then a
    /// fallback that completes with "done".
    struct QueueLoader {
        routed: Vec<(&'static str, Arc<dyn Provider>)>,
        queue: StdMutex<Vec<Arc<dyn Provider>>>,
    }

    impl RuntimeLoader for QueueLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let spawn_model_routes = self
                .routed
                .iter()
                .map(|(model, _)| (*model).to_owned())
                .collect::<Vec<_>>();
            let provider = self
                .routed
                .iter()
                .find(|(model, _)| request.model.model.as_deref() == Some(*model))
                .map(|(_, provider)| Arc::clone(provider))
                .or_else(|| {
                    let mut queue = self.queue.lock().unwrap();
                    if queue.is_empty() {
                        None
                    } else {
                        Some(queue.remove(0))
                    }
                })
                .unwrap_or_else(|| Arc::new(StaticTextProvider));
            Box::pin(async move {
                Runtime::with_provider(provider, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(
                            runtime
                                .with_spawn_model_routes(spawn_model_routes)
                                // Failure-path tests assert on the first
                                // error; turn retry is covered in lib.rs.
                                .with_turn_retry_policy(crate::TurnRetryPolicy::disabled()),
                        ),
                        pricing: None,
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct ResolvingLoader {
        parent: Arc<dyn Provider>,
        child: Arc<dyn Provider>,
        worker: Option<ModelSelection>,
        resolutions: Arc<AtomicUsize>,
        loads: Arc<StdMutex<Vec<ModelSelection>>>,
    }

    impl RuntimeLoader for ResolvingLoader {
        fn resolve_worker_model(
            &self,
            _workspace: String,
            parent: ModelSelection,
        ) -> WorkerRuntimeLoadFuture {
            self.resolutions.fetch_add(1, Ordering::AcqRel);
            let selection = self.worker.clone().unwrap_or(parent);
            Box::pin(std::future::ready(Ok(selection)))
        }

        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            self.loads.lock().unwrap().push(request.model.clone());
            let mut spawn_model_routes = vec!["test/explicit".to_owned()];
            if let Some(worker) = self.worker.as_ref().and_then(|worker| worker.model.clone()) {
                spawn_model_routes.push(worker);
            }
            let provider = if request.model.model.as_deref() == Some("test/model") {
                Arc::clone(&self.parent)
            } else {
                Arc::clone(&self.child)
            };
            Box::pin(async move {
                Runtime::with_provider(provider, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(
                            runtime
                                .with_spawn_model_routes(spawn_model_routes)
                                // Failure-path tests assert on the first
                                // error; turn retry is covered in lib.rs.
                                .with_turn_retry_policy(crate::TurnRetryPolicy::disabled()),
                        ),
                        pricing: None,
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct RejectingWorkerLoader {
        parent: Arc<dyn Provider>,
    }

    impl RuntimeLoader for RejectingWorkerLoader {
        fn resolve_worker_model(
            &self,
            _workspace: String,
            _parent: ModelSelection,
        ) -> WorkerRuntimeLoadFuture {
            Box::pin(std::future::ready(Err(RuntimeLoadError {
                kind: RunFailureKind::Policy,
                message: "configured worker route is denied".to_owned(),
            })))
        }

        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let provider = Arc::clone(&self.parent);
            Box::pin(async move {
                Runtime::with_provider(provider, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(runtime),
                        pricing: None,
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    /// Records every spawn-time validation and either accepts or rejects
    /// it; loads route the parent model to `parent` and everything else to
    /// `child`.
    struct ValidatingLoader {
        parent: Arc<dyn Provider>,
        child: Arc<dyn Provider>,
        worker: Option<ModelSelection>,
        rejection: Option<RuntimeLoadError>,
        validations: Arc<StdMutex<Vec<ModelSelection>>>,
        loads: Arc<StdMutex<Vec<ModelSelection>>>,
    }

    impl RuntimeLoader for ValidatingLoader {
        fn resolve_worker_model(
            &self,
            _workspace: String,
            parent: ModelSelection,
        ) -> WorkerRuntimeLoadFuture {
            let selection = self.worker.clone().unwrap_or(parent);
            Box::pin(std::future::ready(Ok(selection)))
        }

        fn validate_spawn_model(
            &self,
            _workspace: String,
            selection: ModelSelection,
        ) -> SpawnModelValidationFuture {
            self.validations.lock().unwrap().push(selection);
            let result = match &self.rejection {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            };
            Box::pin(std::future::ready(result))
        }

        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            self.loads.lock().unwrap().push(request.model.clone());
            let provider = if request.model.model.as_deref() == Some("test/model") {
                Arc::clone(&self.parent)
            } else {
                Arc::clone(&self.child)
            };
            Box::pin(async move {
                Runtime::with_provider(provider, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(
                            runtime.with_turn_retry_policy(crate::TurnRetryPolicy::disabled()),
                        ),
                        pricing: None,
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    /// Completes immediately with the text "done".
    struct StaticTextProvider;

    impl Provider for StaticTextProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "done".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
            ]))
        }
    }

    struct AccountingTextProvider {
        usage: qq_provider::ProviderUsage,
    }

    impl Provider for AccountingTextProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "done".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed {
                    usage: Some(self.usage),
                }),
            ]))
        }
    }

    struct AccountingSpawnProvider {
        turn: StdMutex<usize>,
    }

    impl Provider for AccountingSpawnProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            let mut turn = self.turn.lock().unwrap();
            let current = *turn;
            *turn += 1;
            drop(turn);
            if current == 0 {
                Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: "first".to_owned(),
                        name: "spawn_agent".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: "first".to_owned(),
                        json: r#"{"task":"first","model":"test/child-first"}"#.to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                        id: "first".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: "second".to_owned(),
                        name: "spawn_agent".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: "second".to_owned(),
                        json: r#"{"task":"second","model":"test/child-second"}"#.to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                        id: "second".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed {
                        usage: Some(qq_provider::ProviderUsage {
                            input_tokens: 2,
                            cache_read_input_tokens: 0,
                            cache_write_input_tokens: 0,
                            output_tokens: 3,
                        }),
                    }),
                ]))
            } else {
                Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed {
                        usage: Some(qq_provider::ProviderUsage {
                            input_tokens: 5,
                            cache_read_input_tokens: 0,
                            cache_write_input_tokens: 0,
                            output_tokens: 7,
                        }),
                    }),
                ]))
            }
        }
    }

    struct AccountingSpawnLoader;

    impl RuntimeLoader for AccountingSpawnLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let provider: Arc<dyn Provider> = match request.model.model.as_deref() {
                Some("test/model") => Arc::new(AccountingSpawnProvider {
                    turn: StdMutex::new(0),
                }),
                Some("test/child-first") => Arc::new(AccountingTextProvider {
                    usage: qq_provider::ProviderUsage {
                        input_tokens: 11,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                        output_tokens: 13,
                    },
                }),
                Some("test/child-second") => Arc::new(AccountingTextProvider {
                    usage: qq_provider::ProviderUsage {
                        input_tokens: 17,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                        output_tokens: 19,
                    },
                }),
                other => panic!("unexpected accounting test model: {other:?}"),
            };
            Box::pin(async move {
                Runtime::with_provider(provider, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(runtime.with_spawn_model_routes(vec![
                            "test/child-first".to_owned(),
                            "test/child-second".to_owned(),
                        ])),
                        pricing: Some(ModelPricing {
                            input_usd_nanos_per_token: 1,
                            output_usd_nanos_per_token: 1,
                            cache_read_usd_nanos_per_token: Some(1),
                            cache_write_usd_nanos_per_token: Some(1),
                            context_tier: None,
                            provenance: "accounting-test".to_owned(),
                        }),
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    /// Fails every stream with a transport error.
    struct FailingProvider;

    impl Provider for FailingProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            Box::pin(stream::once(async {
                Err(qq_provider::ProviderError::Transport("offline".to_owned()))
            }))
        }
    }

    /// Never yields: the run only ends by cancellation.
    struct HangingProvider;

    impl Provider for HangingProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            Box::pin(stream::pending())
        }
    }

    struct SpawnThenHangProvider {
        turn: AtomicUsize,
        second_turn_started: Arc<tokio::sync::Notify>,
    }

    impl Provider for SpawnThenHangProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            if self.turn.fetch_add(1, Ordering::AcqRel) == 0 {
                Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: "spawn".to_owned(),
                        name: "spawn_agent".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: "spawn".to_owned(),
                        json: r#"{"task":"finish first","model":"test/child"}"#.to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                        id: "spawn".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                ]))
            } else {
                self.second_turn_started.notify_one();
                Box::pin(stream::pending())
            }
        }
    }

    /// Tracks how many of its streams are concurrently active before
    /// completing with text, to observe the child concurrency cap.
    struct GaugedTextProvider {
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }

    impl Provider for GaugedTextProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            let active = Arc::clone(&self.active);
            let peak = Arc::clone(&self.peak);
            Box::pin(async_stream! {
                let current = active.fetch_add(1, Ordering::AcqRel) + 1;
                peak.fetch_max(current, Ordering::AcqRel);
                tokio::time::sleep(Duration::from_millis(50)).await;
                active.fetch_sub(1, Ordering::AcqRel);
                yield Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "child done".to_owned(),
                });
                yield Ok(qq_provider::ProviderEvent::Completed { usage: None });
            })
        }
    }

    /// Requests `spawns` spawn_agent calls in one turn, then completes with
    /// "done" on the next.
    struct MultiSpawnProvider {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        spawns: usize,
        arguments: fn(usize) -> String,
        turn: StdMutex<usize>,
    }

    impl Provider for MultiSpawnProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            let mut turn = self.turn.lock().unwrap();
            let current = *turn;
            *turn += 1;
            drop(turn);
            if current == 0 {
                let mut events = Vec::with_capacity(self.spawns * 3 + 1);
                for index in 0..self.spawns {
                    let id = format!("call_{index}");
                    events.push(Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: id.clone(),
                        name: "spawn_agent".to_owned(),
                    }));
                    events.push(Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: id.clone(),
                        json: (self.arguments)(index),
                    }));
                    events.push(Ok(qq_provider::ProviderEvent::ToolCallCompleted { id }));
                }
                events.push(Ok(qq_provider::ProviderEvent::Completed { usage: None }));
                Box::pin(stream::iter(events))
            } else {
                Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
    }

    async fn create_claimed_parent(
        store: &Store,
        workspace_path: &Path,
    ) -> (WorkspaceId, SessionId, ClaimedRun) {
        let resolved = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::ResolveWorkspace {
                    path: workspace_path.to_str().unwrap().to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.receipt.outcome else {
            panic!("unexpected receipt")
        };
        let root = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: ApprovalMode::default(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = root.receipt.outcome else {
            panic!("unexpected receipt")
        };
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "delegate work".to_owned(),
                },
            )
            .await
            .unwrap();
        let claimed = store.claim_next_run(false).await.unwrap().unwrap();
        (workspace_id, session_id, claimed)
    }

    struct SpawnHarness {
        _directory: TempDir,
        runtime: SessionRuntime,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        events: SessionEventStream,
    }

    async fn spawn_harness_with_loader(
        loader: Arc<dyn RuntimeLoader>,
        max_active_runs: usize,
    ) -> SpawnHarness {
        let directory = tempfile::tempdir().unwrap();
        let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
        options.max_active_runs = max_active_runs;
        let runtime = SessionRuntime::open(options, loader).await.unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        SpawnHarness {
            _directory: directory,
            runtime,
            workspace_id,
            session_id,
            events,
        }
    }

    async fn spawn_harness(
        routed: Vec<(&'static str, Arc<dyn Provider>)>,
        queue: Vec<Arc<dyn Provider>>,
        max_active_runs: usize,
    ) -> SpawnHarness {
        spawn_harness_with_loader(
            Arc::new(QueueLoader {
                routed,
                queue: StdMutex::new(queue),
            }),
            max_active_runs,
        )
        .await
    }

    async fn submit_prompt_to(
        runtime: &SessionRuntime,
        session_id: SessionId,
        prompt: &str,
    ) -> RunId {
        let queued = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: prompt.to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
            panic!("unexpected receipt")
        };
        run_id
    }

    async fn completed_instruction_hash(
        runtime: &SessionRuntime,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        events: &mut SessionEventStream,
        prompt: &str,
    ) -> String {
        let run_id = submit_prompt_to(runtime, session_id, prompt).await;
        collect_through_finished(events).await;
        runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 8,
                message_limit: 32,
            })
            .await
            .unwrap()
            .focused
            .unwrap()
            .runs
            .into_iter()
            .find(|run| run.id == run_id)
            .unwrap()
            .prompt_identity
            .expect("a sent prompt must retain its prompt identity")
            .instruction_hash
            .to_string()
    }

    /// Collects events until `run_id` finishes, with a timeout generous
    /// enough for a parent run that awaits child runs.
    async fn collect_until_run_finished(
        events: &mut SessionEventStream,
        run_id: RunId,
    ) -> Vec<SessionEventEnvelope> {
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut observed = Vec::new();
            while let Some(event) = events.next().await {
                let event = event.unwrap();
                let finished = matches!(
                    &event.event,
                    SessionEvent::RunFinished { run_id: done, .. } if *done == run_id
                );
                observed.push(event);
                if finished {
                    break;
                }
            }
            observed
        })
        .await
        .expect("timed out waiting for the run to finish")
    }

    fn finished_outcome(events: &[SessionEventEnvelope], run_id: RunId) -> Option<RunOutcome> {
        events.iter().find_map(|event| match &event.event {
            SessionEvent::RunFinished {
                run_id: done,
                outcome,
                ..
            } if *done == run_id => Some(outcome.clone()),
            _ => None,
        })
    }

    #[tokio::test]
    async fn spawn_agent_runs_a_read_only_child_and_returns_its_final_text() {
        let parent_requests = Arc::new(StdMutex::new(Vec::new()));
        let child_requests = Arc::new(StdMutex::new(Vec::new()));
        let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
            requests: Arc::clone(&parent_requests),
            script: vec![(
                "spawn_agent",
                r#"{"task":"/review Survey the widget inventory","model":"test/child"}"#.to_owned(),
            )],
            turn: StdMutex::new(0),
        });
        // The child attempts a mutation first; read-only mode must deny it
        // without prompting, and the child then completes with "done".
        let child: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
            requests: Arc::clone(&child_requests),
            script: vec![(
                "edit_file",
                r#"{"path":"a.txt","old_string":"x","new_string":"y"}"#.to_owned(),
            )],
            turn: StdMutex::new(0),
        });
        let mut harness = spawn_harness(vec![("test/child", child)], vec![parent], 8).await;
        let skill = harness._directory.path().join(".qq/skills/review");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "User-selected guidance only.\n").unwrap();
        let run_id =
            submit_prompt_to(&harness.runtime, harness.session_id, "delegate the survey").await;
        let observed = collect_until_run_finished(&mut harness.events, run_id).await;

        let child_session = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::SessionCreated { session }
                    if session.parent_id == Some(harness.session_id) =>
                {
                    Some(session.clone())
                }
                _ => None,
            })
            .expect("the spawn call must create a child session");
        assert_eq!(child_session.model.as_deref(), Some("test/child"));
        assert_eq!(child_session.status, SessionStatus::Queued);
        assert_eq!(child_session.queued_prompts, 1);
        assert_eq!(child_session.title, "/review Survey the widget inventory");
        let created_event = observed
            .iter()
            .find(|event| {
                matches!(
                    &event.event,
                    SessionEvent::SessionCreated { session } if session.id == child_session.id
                )
            })
            .unwrap();
        let queued_event = observed
            .iter()
            .find(|event| {
                matches!(
                    &event.event,
                    SessionEvent::PromptQueued { session, .. } if session.id == child_session.id
                )
            })
            .unwrap();
        assert_eq!(
            created_event.cursor.sequence + 1,
            queued_event.cursor.sequence
        );
        assert_eq!(created_event.caused_by, queued_event.caused_by);
        assert!(created_event.caused_by.is_some());
        assert_eq!(created_event.run_id, queued_event.run_id);
        // The task's first line names the child at prompt submission.
        assert!(observed.iter().any(|event| matches!(
            &event.event,
                SessionEvent::PromptQueued { session, .. }
                if session.id == child_session.id
                    && session.title == "/review Survey the widget inventory"
        )));
        // spawn_agent is read-only: no approval round trip in Ask mode.
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. }))
        );
        let follow_up = submit_prompt_to(
            &harness.runtime,
            child_session.id,
            "/review user-authored follow-up",
        )
        .await;
        let follow_up_events = collect_until_run_finished(&mut harness.events, follow_up).await;
        assert!(matches!(
            finished_outcome(&follow_up_events, follow_up),
            Some(RunOutcome::Completed)
        ));
        let child_reqs = child_requests.lock().unwrap();
        assert!(
            !child_reqs[0]
                .tools()
                .iter()
                .any(|spec| spec.name() == "spawn_agent"),
            "child sessions must not have spawn_agent declared"
        );
        assert_eq!(
            child_reqs[0].messages(),
            [Message::user("/review Survey the widget inventory")]
        );
        assert!(
            !child_reqs[0]
                .system()
                .unwrap()
                .contains("User-selected guidance only."),
            "a model-created child task must not select runtime guidance"
        );
        assert!(matches!(
            child_reqs[1].messages()[2].content(),
            [ContentBlock::ToolResult { content, is_error: true, .. }]
                if content == approval::POLICY_DENIED_RESULT
        ));
        assert!(
            child_reqs[2]
                .system()
                .unwrap()
                .contains("User-selected guidance only."),
            "an explicit user prompt in a child session may select runtime guidance"
        );
        assert!(
            !child_reqs[2]
                .tools()
                .iter()
                .any(|spec| spec.name() == "spawn_agent"),
            "child sessions remain depth-capped after a user follow-up"
        );
        drop(child_reqs);
        let parent_reqs = parent_requests.lock().unwrap();
        assert!(
            parent_reqs[0]
                .tools()
                .iter()
                .any(|spec| spec.name() == "spawn_agent")
        );
        assert!(matches!(
            parent_reqs[1].messages()[2].content(),
            [ContentBlock::ToolResult { content, is_error: false, .. }] if content == "done"
        ));
        drop(parent_reqs);
        assert!(matches!(
            finished_outcome(&observed, run_id),
            Some(RunOutcome::Completed)
        ));
    }

    #[tokio::test]
    async fn multi_turn_child_returns_only_its_final_answer() {
        let parent_requests = Arc::new(StdMutex::new(Vec::new()));
        let child_requests = Arc::new(StdMutex::new(Vec::new()));
        let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
            requests: Arc::clone(&parent_requests),
            script: vec![(
                "spawn_agent",
                r#"{"task":"Inspect the note","model":"test/child"}"#.to_owned(),
            )],
            turn: StdMutex::new(0),
        });
        let child: Arc<dyn Provider> = Arc::new(TurnTextProvider {
            requests: Arc::clone(&child_requests),
        });
        let mut harness = spawn_harness(vec![("test/child", child)], vec![parent], 8).await;
        std::fs::write(harness._directory.path().join("note.txt"), "evidence\n").unwrap();
        let parent_run = submit_prompt_to(
            &harness.runtime,
            harness.session_id,
            "delegate the inspection",
        )
        .await;
        let observed = collect_until_run_finished(&mut harness.events, parent_run).await;

        let child_session_id = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::SessionCreated { session }
                    if session.parent_id == Some(harness.session_id) =>
                {
                    Some(session.id)
                }
                _ => None,
            })
            .unwrap();
        {
            let parent_requests = parent_requests.lock().unwrap();
            assert!(matches!(
                parent_requests[1].messages()[2].content(),
                [ContentBlock::ToolResult { content, is_error: false, .. }]
                    if content == "done"
            ));
        }

        let snapshot = harness
            .runtime
            .snapshot(SnapshotRequest {
                workspace_id: harness.workspace_id,
                focused_session_id: Some(child_session_id),
                session_limit: 8,
                message_limit: 8,
            })
            .await
            .unwrap();
        let assistant = snapshot
            .focused
            .unwrap()
            .messages
            .into_iter()
            .filter(|message| message.role == MessageRole::Assistant)
            .map(|message| message.output)
            .collect::<Vec<_>>();
        assert_eq!(assistant, ["Let me look. ", "done"]);
    }

    #[tokio::test]
    async fn child_final_refusal_is_returned_to_the_parent() {
        let parent_requests = Arc::new(StdMutex::new(Vec::new()));
        let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
            requests: Arc::clone(&parent_requests),
            script: vec![(
                "spawn_agent",
                r#"{"task":"Attempt the task","model":"test/child"}"#.to_owned(),
            )],
            turn: StdMutex::new(0),
        });
        let child: Arc<dyn Provider> = Arc::new(RefusalProvider);
        let mut harness = spawn_harness(vec![("test/child", child)], vec![parent], 8).await;
        let parent_run =
            submit_prompt_to(&harness.runtime, harness.session_id, "delegate the task").await;
        collect_until_run_finished(&mut harness.events, parent_run).await;

        let parent_requests = parent_requests.lock().unwrap();
        assert!(matches!(
            parent_requests[1].messages()[2].content(),
            [ContentBlock::ToolResult { content, is_error: false, .. }]
                if content == "cannot complete that task"
        ));
    }

    #[tokio::test]
    async fn failed_child_run_insert_leaves_no_idle_orphan() {
        let parent_requests = Arc::new(StdMutex::new(Vec::new()));
        let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
            requests: Arc::clone(&parent_requests),
            script: vec![(
                "spawn_agent",
                r#"{"task":"transactional research","model":"test/child"}"#.to_owned(),
            )],
            turn: StdMutex::new(0),
        });
        let mut harness = spawn_harness(
            vec![("test/child", Arc::new(StaticTextProvider))],
            vec![parent],
            8,
        )
        .await;
        harness
            .runtime
            .inner
            .store
            .call(Priority::Control, |connection| {
                connection
                    .execute_batch(
                        "CREATE TRIGGER fail_spawned_child_run BEFORE INSERT ON runs
                         WHEN EXISTS (
                             SELECT 1 FROM sessions
                             WHERE id = NEW.session_id AND parent_id IS NOT NULL
                         )
                         BEGIN
                             SELECT RAISE(ABORT, 'injected child run failure');
                         END;",
                    )
                    .map_err(|_| SessionRuntimeError::Persistence)
            })
            .await
            .unwrap();
        let parent_run =
            submit_prompt_to(&harness.runtime, harness.session_id, "delegate atomically").await;
        let observed = collect_until_run_finished(&mut harness.events, parent_run).await;

        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::SessionCreated { .. })),
            "a rolled-back spawn must publish no child event"
        );
        let snapshot = harness
            .runtime
            .snapshot(SnapshotRequest {
                workspace_id: harness.workspace_id,
                focused_session_id: Some(harness.session_id),
                session_limit: 8,
                message_limit: 4,
            })
            .await
            .unwrap();
        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(snapshot.sessions[0].id, harness.session_id);
        let parent_requests = parent_requests.lock().unwrap();
        assert!(matches!(
            parent_requests[1].messages()[2].content(),
            [ContentBlock::ToolResult { content, is_error: true, .. }]
                if content.contains("sub-agent")
        ));
    }

    #[tokio::test]
    async fn parent_cancellation_linearizes_with_in_flight_child_creation() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let store = Store::open(database_path).await.unwrap();
        let (_, _, parent) = create_claimed_parent(&store, directory.path()).await;

        // Hold the database operation after Store::call has handed it to the
        // worker, then drop its awaiting task. The worker must still commit,
        // reproducing the handoff window where no CancelChildOnDrop guard can
        // be installed by the cancelled spawn future.
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (created_tx, created_rx) = tokio::sync::oneshot::channel();
        let create_store = store.clone();
        let create_parent = parent.clone();
        let create_task = tokio::spawn(async move {
            let store_id = create_store.store_id();
            create_store
                .call(Priority::Control, move |connection| {
                    let _ = entered_tx.send(());
                    release_rx
                        .recv()
                        .map_err(|_| SessionRuntimeError::Unavailable)?;
                    let result = create_child_run(
                        connection,
                        store_id,
                        create_parent.workspace_id,
                        create_parent.session_id,
                        create_parent.run_id,
                        ModelSelection {
                            model: Some("test/child".to_owned()),
                            max_output_tokens: Some(256),
                            organization: None,
                        },
                        "queued child task".to_owned(),
                    );
                    let _ = created_tx.send(result.as_ref().ok().map(|created| {
                        (
                            created.session_id,
                            created.run_id,
                            created.committed_through,
                        )
                    }));
                    result
                })
                .await
        });
        entered_rx.await.unwrap();
        create_task.abort();
        assert!(matches!(create_task.await, Err(error) if error.is_cancelled()));
        release_tx.send(()).unwrap();
        let (_, child_run, _) = created_rx
            .await
            .unwrap()
            .expect("the detached store job should commit the child");

        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CancelRun {
                    run_id: parent.run_id,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            store.run_outcome(child_run).await.unwrap(),
            Some(RunOutcome::Cancelled)
        );
        assert!(store.claim_next_run(true).await.unwrap().is_none());
        store
            .finish_run(&parent, RunOutcome::Cancelled, None)
            .await
            .unwrap();
        assert!(store.unfinished_run_ids().await.unwrap().is_empty());

        // The reverse database ordering rejects creation once cancellation is
        // durable, so no child can appear after its parent starts settling.
        let (_, _, cancelling_parent) = create_claimed_parent(&store, directory.path()).await;
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CancelRun {
                    run_id: cancelling_parent.run_id,
                },
            )
            .await
            .unwrap();
        let rejected = store
            .create_child_run(
                &cancelling_parent,
                ModelSelection {
                    model: Some("test/child".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                "too late".to_owned(),
            )
            .await;
        assert!(matches!(rejected, Err(SessionRuntimeError::RunNotFound)));
        store
            .finish_run(&cancelling_parent, RunOutcome::Cancelled, None)
            .await
            .unwrap();
        assert!(store.unfinished_run_ids().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn replayed_parent_cancellation_rediscovers_its_running_child() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let (_, _, parent) = create_claimed_parent(&store, directory.path()).await;
        let child = store
            .create_child_run(
                &parent,
                ModelSelection {
                    model: Some("test/child".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                "running child task".to_owned(),
            )
            .await
            .unwrap();
        let claimed_child = store.claim_next_run(true).await.unwrap().unwrap();
        assert_eq!(claimed_child.run_id, child.run_id);

        let command_id = CommandId::generate().unwrap();
        let command = SessionCommand::CancelRun {
            run_id: parent.run_id,
        };
        let first = store.command(command_id, command.clone()).await.unwrap();
        let replay = store.command(command_id, command).await.unwrap();

        assert_eq!(replay.receipt, first.receipt);
        assert_eq!(first.cascade_cancels, [child.run_id]);
        assert_eq!(replay.cascade_cancels, [child.run_id]);
        store
            .finish_run(&claimed_child, RunOutcome::Cancelled, None)
            .await
            .unwrap();
        store
            .finish_run(&parent, RunOutcome::Cancelled, None)
            .await
            .unwrap();
        assert!(store.unfinished_run_ids().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn restart_cancels_a_queued_child_owned_by_an_interrupted_parent() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let store = Store::open(database_path.clone()).await.unwrap();
        let (workspace_id, root_session_id, parent) =
            create_claimed_parent(&store, directory.path()).await;
        let child = store
            .create_child_run(
                &parent,
                ModelSelection {
                    model: Some("test/child".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                "queued child task".to_owned(),
            )
            .await
            .unwrap();
        drop(store);

        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(database_path),
            Arc::new(CapturingLoader {
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: child.committed_through,
            })
            .unwrap();
        let recovered = tokio::time::timeout(Duration::from_secs(2), async {
            let mut recovered = Vec::new();
            while recovered
                .iter()
                .filter(|event: &&SessionEventEnvelope| {
                    matches!(event.event, SessionEvent::RunFinished { .. })
                })
                .count()
                < 2
            {
                recovered.push(events.next().await.unwrap().unwrap());
            }
            recovered
        })
        .await
        .unwrap();
        assert!(recovered.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished {
                run_id,
                outcome: RunOutcome::Cancelled,
                ..
            } if *run_id == child.run_id
        )));
        assert!(recovered.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished {
                run_id,
                outcome: RunOutcome::Interrupted,
                ..
            } if *run_id == parent.run_id
        )));
        assert!(requests.lock().unwrap().is_empty());
        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(child.session_id),
                session_limit: 8,
                message_limit: 4,
            })
            .await
            .unwrap();
        let child_snapshot = snapshot.focused.unwrap();
        assert_eq!(child_snapshot.summary.parent_id, Some(root_session_id));
        assert_eq!(child_snapshot.summary.status, SessionStatus::Idle);
        assert_eq!(child_snapshot.runs[0].outcome, Some(RunOutcome::Cancelled));
    }

    #[tokio::test]
    async fn parallel_spawn_accounting_is_ordered_exact_and_survives_restart() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let mut options = SessionRuntimeOptions::new(database_path.clone());
        options.max_active_runs = 4;
        let runtime = SessionRuntime::open(options, Arc::new(AccountingSpawnLoader))
            .await
            .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated {
            session_id: parent_id,
        } = created.outcome
        else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();

        let parent_run = submit_prompt_to(&runtime, parent_id, "delegate twice").await;
        let observed = collect_until_run_finished(&mut events, parent_run).await;
        let child_ids = observed
            .iter()
            .filter_map(|event| match &event.event {
                SessionEvent::SessionCreated { session }
                    if session.parent_id == Some(parent_id) =>
                {
                    Some(session.id)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(child_ids.len(), 2);

        for child_id in &child_ids {
            let child_finished = observed
                .iter()
                .position(|event| {
                    event.session_id == *child_id
                        && matches!(event.event, SessionEvent::RunFinished { .. })
                })
                .expect("child must finish");
            let parent_refreshed = &observed[child_finished + 1];
            assert_eq!(parent_refreshed.session_id, parent_id);
            assert!(matches!(
                parent_refreshed.event,
                SessionEvent::SessionUpdated { .. }
            ));
        }

        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(parent_id),
                session_limit: 8,
                message_limit: 8,
            })
            .await
            .unwrap();
        let parent = snapshot
            .sessions
            .iter()
            .find(|session| session.id == parent_id)
            .unwrap();
        let accounting = parent.accounting.unwrap();
        assert_eq!(accounting.direct.usage, Some(usage(7, 10)));
        assert_eq!(accounting.direct.estimated_cost_usd_nanos, Some(17));
        assert_eq!(accounting.inclusive.usage, Some(usage(35, 42)));
        assert_eq!(accounting.inclusive.estimated_cost_usd_nanos, Some(77));
        let child_costs = child_ids
            .iter()
            .map(|child_id| {
                snapshot
                    .sessions
                    .iter()
                    .find(|session| session.id == *child_id)
                    .unwrap()
                    .accounting
                    .unwrap()
            })
            .map(|accounting| {
                assert_eq!(accounting.direct, accounting.inclusive);
                accounting.direct.estimated_cost_usd_nanos.unwrap()
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(child_costs, std::collections::BTreeSet::from([24, 36]));

        drop(events);
        drop(runtime);
        let reopened = SessionRuntime::open(
            SessionRuntimeOptions::new(database_path),
            Arc::new(AccountingSpawnLoader),
        )
        .await
        .unwrap();
        let reloaded = reopened
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(parent_id),
                session_limit: 8,
                message_limit: 8,
            })
            .await
            .unwrap();
        let reloaded_parent = reloaded
            .sessions
            .iter()
            .find(|session| session.id == parent_id)
            .unwrap();
        assert_eq!(reloaded_parent.accounting, parent.accounting);
    }

    #[tokio::test]
    async fn configured_worker_model_wins_and_preserves_parent_selection_fields() {
        let parent_requests = Arc::new(StdMutex::new(Vec::new()));
        let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
            requests: Arc::clone(&parent_requests),
            script: vec![(
                "spawn_agent",
                r#"{"task":"configured worker research"}"#.to_owned(),
            )],
            turn: StdMutex::new(0),
        });
        let resolutions = Arc::new(AtomicUsize::new(0));
        let loads = Arc::new(StdMutex::new(Vec::new()));
        let worker = ModelSelection {
            model: Some("test/worker".to_owned()),
            max_output_tokens: Some(123),
            organization: Some("worker-org".to_owned()),
        };
        let mut harness = spawn_harness_with_loader(
            Arc::new(ResolvingLoader {
                parent,
                child: Arc::new(StaticTextProvider),
                worker: Some(worker.clone()),
                resolutions: Arc::clone(&resolutions),
                loads: Arc::clone(&loads),
            }),
            8,
        )
        .await;

        let run_id = submit_prompt_to(
            &harness.runtime,
            harness.session_id,
            "use configured worker",
        )
        .await;
        let observed = collect_until_run_finished(&mut harness.events, run_id).await;
        let child = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::SessionCreated { session }
                    if session.parent_id == Some(harness.session_id) =>
                {
                    Some(session)
                }
                _ => None,
            })
            .expect("the configured worker must create a child");

        assert_eq!(child.model.as_deref(), Some("test/worker"));
        assert_eq!(resolutions.load(Ordering::Acquire), 1);
        assert!(
            loads
                .lock()
                .unwrap()
                .iter()
                .filter(|load| **load == worker)
                .count()
                >= 2,
            "the same complete selection must be preflighted and used by the child run"
        );
        assert!(matches!(
            finished_outcome(&observed, run_id),
            Some(RunOutcome::Completed)
        ));
    }

    #[tokio::test]
    async fn explicit_spawn_model_bypasses_configured_worker_resolution() {
        let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
            requests: Arc::new(StdMutex::new(Vec::new())),
            script: vec![(
                "spawn_agent",
                r#"{"task":"specialized research","model":"test/explicit"}"#.to_owned(),
            )],
            turn: StdMutex::new(0),
        });
        let resolutions = Arc::new(AtomicUsize::new(0));
        let loads = Arc::new(StdMutex::new(Vec::new()));
        let mut harness = spawn_harness_with_loader(
            Arc::new(ResolvingLoader {
                parent,
                child: Arc::new(StaticTextProvider),
                worker: Some(ModelSelection {
                    model: Some("test/worker".to_owned()),
                    max_output_tokens: Some(111),
                    organization: Some("worker-org".to_owned()),
                }),
                resolutions: Arc::clone(&resolutions),
                loads,
            }),
            8,
        )
        .await;

        let run_id =
            submit_prompt_to(&harness.runtime, harness.session_id, "override worker").await;
        let observed = collect_until_run_finished(&mut harness.events, run_id).await;
        let child = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::SessionCreated { session }
                    if session.parent_id == Some(harness.session_id) =>
                {
                    Some(session)
                }
                _ => None,
            })
            .expect("the explicit model must create a child");

        assert_eq!(child.model.as_deref(), Some("test/explicit"));
        assert_eq!(resolutions.load(Ordering::Acquire), 0);
        assert!(matches!(
            finished_outcome(&observed, run_id),
            Some(RunOutcome::Completed)
        ));
    }

    #[tokio::test]
    async fn worker_resolution_failure_creates_no_child_state_and_parent_continues() {
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
            requests: Arc::clone(&requests),
            script: vec![(
                "spawn_agent",
                r#"{"task":"research denied route"}"#.to_owned(),
            )],
            turn: StdMutex::new(0),
        });
        let mut harness =
            spawn_harness_with_loader(Arc::new(RejectingWorkerLoader { parent }), 8).await;

        let run_id = submit_prompt_to(
            &harness.runtime,
            harness.session_id,
            "try configured worker",
        )
        .await;
        let observed = collect_until_run_finished(&mut harness.events, run_id).await;

        assert!(
            observed.iter().all(|event| !matches!(
                &event.event,
                SessionEvent::SessionCreated { session }
                    if session.parent_id == Some(harness.session_id)
            )),
            "resolution failure must not emit a child creation"
        );
        let snapshot = harness
            .runtime
            .snapshot(SnapshotRequest {
                workspace_id: harness.workspace_id,
                focused_session_id: Some(harness.session_id),
                session_limit: 32,
                message_limit: 32,
            })
            .await
            .unwrap();
        assert_eq!(snapshot.sessions.len(), 1);
        assert!(
            snapshot
                .sessions
                .iter()
                .all(|session| session.parent_id.is_none())
        );

        let requests = requests.lock().unwrap();
        assert!(matches!(
            requests[1].messages()[2].content(),
            [ContentBlock::ToolResult { content, is_error: true, .. }]
                if content.contains("configured worker route is denied")
        ));
        assert!(matches!(
            finished_outcome(&observed, run_id),
            Some(RunOutcome::Completed)
        ));
    }

    #[tokio::test]
    async fn rejected_spawn_validation_creates_no_child_state_and_names_the_check() {
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
            requests: Arc::clone(&requests),
            script: vec![(
                "spawn_agent",
                r#"{"task":"research","model":"test/ghost"}"#.to_owned(),
            )],
            turn: StdMutex::new(0),
        });
        let validations = Arc::new(StdMutex::new(Vec::new()));
        let loads = Arc::new(StdMutex::new(Vec::new()));
        let mut harness = spawn_harness_with_loader(
            Arc::new(ValidatingLoader {
                parent,
                child: Arc::new(StaticTextProvider),
                worker: None,
                rejection: Some(RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: "model \"ghost\" is not in provider \"test\"'s authenticated \
                              model list; available routes: test/child"
                        .to_owned(),
                }),
                validations: Arc::clone(&validations),
                loads: Arc::clone(&loads),
            }),
            8,
        )
        .await;

        let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "spawn a ghost").await;
        let observed = collect_until_run_finished(&mut harness.events, run_id).await;

        // The explicit argument passed through the spawn-time choke point...
        assert_eq!(
            validations
                .lock()
                .unwrap()
                .iter()
                .map(|selection| selection.model.clone())
                .collect::<Vec<_>>(),
            [Some("test/ghost".to_owned())]
        );
        // ...was rejected before any child runtime load...
        assert_eq!(
            loads
                .lock()
                .unwrap()
                .iter()
                .map(|selection| selection.model.clone())
                .collect::<Vec<_>>(),
            [Some("test/model".to_owned())],
            "a rejected route must never reach runtime loading"
        );
        // ...and created no durable child state: no session, no prompt, no
        // run.
        assert!(observed.iter().all(|event| !matches!(
            &event.event,
            SessionEvent::SessionCreated { session }
                if session.parent_id == Some(harness.session_id)
        )));
        assert!(observed.iter().all(|event| !matches!(
            &event.event,
            SessionEvent::PromptQueued { session, .. } if session.id != harness.session_id
        )));
        let snapshot = harness
            .runtime
            .snapshot(SnapshotRequest {
                workspace_id: harness.workspace_id,
                focused_session_id: Some(harness.session_id),
                session_limit: 32,
                message_limit: 32,
            })
            .await
            .unwrap();
        assert_eq!(snapshot.sessions.len(), 1);
        assert!(
            snapshot
                .sessions
                .iter()
                .all(|session| session.parent_id.is_none())
        );
        // The parent sees a bounded tool error naming the failed check and
        // continues to completion.
        let parent_reqs = requests.lock().unwrap();
        assert!(matches!(
            parent_reqs[1].messages()[2].content(),
            [ContentBlock::ToolResult { content, is_error: true, .. }]
                if content.contains("the sub-agent model was rejected")
                    && content.contains("authenticated model list")
                    && content.contains("available routes: test/child")
        ));
        drop(parent_reqs);
        assert!(matches!(
            finished_outcome(&observed, run_id),
            Some(RunOutcome::Completed)
        ));
    }

    #[tokio::test]
    async fn spawn_validation_covers_worker_and_parent_fallback_routes() {
        for worker in [
            Some(ModelSelection {
                model: Some("test/worker".to_owned()),
                max_output_tokens: Some(64),
                organization: None,
            }),
            None,
        ] {
            let expected = worker
                .as_ref()
                .and_then(|worker| worker.model.clone())
                .unwrap_or_else(|| "test/model".to_owned());
            let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
                requests: Arc::new(StdMutex::new(Vec::new())),
                script: vec![("spawn_agent", r#"{"task":"survey"}"#.to_owned())],
                turn: StdMutex::new(0),
            });
            let validations = Arc::new(StdMutex::new(Vec::new()));
            let mut harness = spawn_harness_with_loader(
                Arc::new(ValidatingLoader {
                    parent,
                    child: Arc::new(StaticTextProvider),
                    worker,
                    rejection: None,
                    validations: Arc::clone(&validations),
                    loads: Arc::new(StdMutex::new(Vec::new())),
                }),
                8,
            )
            .await;

            let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
            let observed = collect_until_run_finished(&mut harness.events, run_id).await;

            // The resolved route — configured worker or parent fallback —
            // passed through the same spawn-time choke point the explicit
            // argument uses, and the child was created on it.
            assert_eq!(
                validations
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|selection| selection.model.clone())
                    .collect::<Vec<_>>(),
                [Some(expected.clone())]
            );
            let child = observed
                .iter()
                .find_map(|event| match &event.event {
                    SessionEvent::SessionCreated { session }
                        if session.parent_id == Some(harness.session_id) =>
                    {
                        Some(session.clone())
                    }
                    _ => None,
                })
                .expect("an accepted route must create the child");
            assert_eq!(child.model.as_deref(), Some(expected.as_str()));
            assert!(matches!(
                finished_outcome(&observed, run_id),
                Some(RunOutcome::Completed)
            ));
        }
    }

    #[tokio::test]
    async fn explicit_route_outside_the_advertised_list_spawns_when_validation_accepts() {
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
            requests: Arc::clone(&requests),
            script: vec![(
                "spawn_agent",
                r#"{"task":"survey","model":"test/discovered"}"#.to_owned(),
            )],
            turn: StdMutex::new(0),
        });
        // QueueLoader advertises only "test/child" in the schema route list;
        // "test/discovered" must still spawn because enforcement lives at
        // the spawn-time choke point (the served model list), not in the
        // schema enum.
        let child: Arc<dyn Provider> = Arc::new(StaticTextProvider);
        let mut harness = spawn_harness(vec![("test/child", child)], vec![parent], 8).await;

        let run_id =
            submit_prompt_to(&harness.runtime, harness.session_id, "spawn discovered").await;
        let observed = collect_until_run_finished(&mut harness.events, run_id).await;

        let child = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::SessionCreated { session }
                    if session.parent_id == Some(harness.session_id) =>
                {
                    Some(session.clone())
                }
                _ => None,
            })
            .expect("a validated route outside the advertised list must spawn");
        assert_eq!(child.model.as_deref(), Some("test/discovered"));
        let parent_reqs = requests.lock().unwrap();
        assert!(matches!(
            parent_reqs[1].messages()[2].content(),
            [ContentBlock::ToolResult { content, is_error: false, .. }] if content == "done"
        ));
        drop(parent_reqs);
        assert!(matches!(
            finished_outcome(&observed, run_id),
            Some(RunOutcome::Completed)
        ));
    }

    #[tokio::test]
    async fn child_sessions_cannot_spawn_and_dispatch_rejects_the_attempt() {
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let provider: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
            requests: Arc::clone(&requests),
            script: vec![("spawn_agent", r#"{"task":"go deeper"}"#.to_owned())],
            turn: StdMutex::new(0),
        });
        let harness = spawn_harness(Vec::new(), vec![provider], 8).await;
        let created = create_session(
            &harness.runtime,
            harness.workspace_id,
            Some(harness.session_id),
        )
        .await;
        let CommandOutcome::SessionCreated {
            session_id: child_id,
        } = created.outcome
        else {
            panic!("unexpected receipt")
        };
        let mut events = harness
            .runtime
            .subscribe(SubscribeRequest {
                workspace_id: harness.workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        let run_id = submit_prompt_to(&harness.runtime, child_id, "try to spawn").await;
        let observed = collect_until_run_finished(&mut events, run_id).await;

        let child_reqs = requests.lock().unwrap();
        assert!(
            !child_reqs[0]
                .tools()
                .iter()
                .any(|spec| spec.name() == "spawn_agent"),
            "the child run must not declare spawn_agent"
        );
        assert!(matches!(
            child_reqs[1].messages()[2].content(),
            [ContentBlock::ToolResult { content, is_error: true, .. }]
                if content == crate::SPAWN_UNAVAILABLE_RESULT
        ));
        drop(child_reqs);
        assert!(matches!(
            finished_outcome(&observed, run_id),
            Some(RunOutcome::Completed)
        ));
        assert!(
            !observed.iter().any(|event| matches!(
                &event.event,
                SessionEvent::SessionCreated { session } if session.parent_id == Some(child_id)
            )),
            "no grandchild session may appear"
        );
    }

    #[tokio::test]
    async fn a_failed_child_returns_a_tool_error_and_the_parent_continues() {
        let parent_requests = Arc::new(StdMutex::new(Vec::new()));
        let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
            requests: Arc::clone(&parent_requests),
            script: vec![(
                "spawn_agent",
                r#"{"task":"doomed research","model":"test/child"}"#.to_owned(),
            )],
            turn: StdMutex::new(0),
        });
        let mut harness = spawn_harness(
            vec![("test/child", Arc::new(FailingProvider))],
            vec![parent],
            8,
        )
        .await;
        let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
        let observed = collect_until_run_finished(&mut harness.events, run_id).await;

        // The child run failed, visibly, on its own session.
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished { run_id: done, outcome: RunOutcome::Failed { .. }, .. }
                if *done != run_id
        )));
        // The parent saw a tool error and still completed.
        let parent_reqs = parent_requests.lock().unwrap();
        assert!(matches!(
            parent_reqs[1].messages()[2].content(),
            [ContentBlock::ToolResult { content, is_error: true, .. }]
                if content.contains("the sub-agent run failed") && content.contains("offline")
        ));
        drop(parent_reqs);
        assert!(matches!(
            finished_outcome(&observed, run_id),
            Some(RunOutcome::Completed)
        ));
    }

    #[tokio::test]
    async fn cancelling_the_parent_run_cancels_its_in_flight_child() {
        let parent_requests = Arc::new(StdMutex::new(Vec::new()));
        let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
            requests: Arc::clone(&parent_requests),
            // No model override: the child must inherit the parent's model.
            script: vec![("spawn_agent", r#"{"task":"long research"}"#.to_owned())],
            turn: StdMutex::new(0),
        });
        let hanging: Arc<dyn Provider> = Arc::new(HangingProvider);
        let mut harness = spawn_harness(Vec::new(), vec![parent, hanging], 8).await;
        let parent_run =
            submit_prompt_to(&harness.runtime, harness.session_id, "delegate forever").await;

        // Wait until the child run is actually executing, then cancel the
        // parent.
        let mut observed = Vec::new();
        let (child_session, child_run) = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let event = harness.events.next().await.unwrap().unwrap();
                let started = match &event.event {
                    SessionEvent::RunStarted { session, run_id }
                        if *run_id != parent_run
                            && session.parent_id == Some(harness.session_id) =>
                    {
                        Some((session.clone(), *run_id))
                    }
                    _ => None,
                };
                observed.push(event);
                if let Some(started) = started {
                    break started;
                }
            }
        })
        .await
        .expect("timed out waiting for the child run to start");
        assert_eq!(
            child_session.model.as_deref(),
            Some("test/model"),
            "a child without a model argument inherits the parent's model"
        );
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CancelRun { run_id: parent_run },
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            while finished_outcome(&observed, parent_run).is_none()
                || finished_outcome(&observed, child_run).is_none()
            {
                observed.push(harness.events.next().await.unwrap().unwrap());
            }
        })
        .await
        .expect("timed out waiting for the parent and child to settle");
        assert!(matches!(
            finished_outcome(&observed, parent_run),
            Some(RunOutcome::Cancelled)
        ));
        assert!(matches!(
            finished_outcome(&observed, child_run),
            Some(RunOutcome::Cancelled)
        ));
    }

    #[tokio::test]
    async fn cancelling_the_parent_interrupts_an_in_flight_child_tool() {
        let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
            requests: Arc::new(StdMutex::new(Vec::new())),
            script: vec![(
                "spawn_agent",
                r#"{"task":"slow read-only work","model":"test/child"}"#.to_owned(),
            )],
            turn: StdMutex::new(0),
        });
        let child: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
            requests: Arc::new(StdMutex::new(Vec::new())),
            script: vec![(
                "__test_delay",
                r#"{"delay_ms":5000,"result":"too late"}"#.to_owned(),
            )],
            turn: StdMutex::new(0),
        });
        let mut harness = spawn_harness(vec![("test/child", child)], vec![parent], 8).await;
        let parent_run =
            submit_prompt_to(&harness.runtime, harness.session_id, "delegate slow work").await;

        let mut observed = Vec::new();
        let (child_run, child_call) = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let event = harness.events.next().await.unwrap().unwrap();
                let started = match &event.event {
                    SessionEvent::ToolCallStarted { tool_call }
                        if tool_call.name == "__test_delay" =>
                    {
                        Some((tool_call.run_id, tool_call.id))
                    }
                    _ => None,
                };
                observed.push(event);
                if let Some(started) = started {
                    break started;
                }
            }
        })
        .await
        .expect("the child tool never started");
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CancelRun { run_id: parent_run },
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while finished_outcome(&observed, parent_run).is_none()
                || finished_outcome(&observed, child_run).is_none()
            {
                observed.push(harness.events.next().await.unwrap().unwrap());
            }
        })
        .await
        .expect("parent cancellation did not stop the child tool");
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.id == child_call && tool_call.state == ToolCallState::Interrupted
        )));
        assert!(matches!(
            finished_outcome(&observed, child_run),
            Some(RunOutcome::Cancelled)
        ));
        assert!(matches!(
            finished_outcome(&observed, parent_run),
            Some(RunOutcome::Cancelled)
        ));
    }

    #[tokio::test]
    async fn cancelling_the_parent_after_child_completion_preserves_the_child() {
        let second_turn_started = Arc::new(tokio::sync::Notify::new());
        let parent: Arc<dyn Provider> = Arc::new(SpawnThenHangProvider {
            turn: AtomicUsize::new(0),
            second_turn_started: Arc::clone(&second_turn_started),
        });
        let child: Arc<dyn Provider> = Arc::new(StaticTextProvider);
        let mut harness = spawn_harness(vec![("test/child", child)], vec![parent], 8).await;
        let parent_run =
            submit_prompt_to(&harness.runtime, harness.session_id, "delegate then wait").await;

        let mut observed = Vec::new();
        let (child_session_id, child_run) = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let event = harness.events.next().await.unwrap().unwrap();
                let completed = match &event.event {
                    SessionEvent::RunFinished {
                        session,
                        run_id,
                        outcome: RunOutcome::Completed,
                        ..
                    } if session.parent_id == Some(harness.session_id) => {
                        Some((session.id, *run_id))
                    }
                    _ => None,
                };
                observed.push(event);
                if let Some(completed) = completed {
                    break completed;
                }
            }
        })
        .await
        .expect("the child did not complete");
        tokio::time::timeout(Duration::from_secs(2), second_turn_started.notified())
            .await
            .expect("the parent did not consume the child answer");
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CancelRun { run_id: parent_run },
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while finished_outcome(&observed, parent_run).is_none() {
                observed.push(harness.events.next().await.unwrap().unwrap());
            }
        })
        .await
        .expect("the parent did not settle");

        assert!(matches!(
            finished_outcome(&observed, child_run),
            Some(RunOutcome::Completed)
        ));
        assert!(matches!(
            finished_outcome(&observed, parent_run),
            Some(RunOutcome::Cancelled)
        ));
        assert!(!observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::CancellationRequested { run_id, .. } if *run_id == child_run
        )));
        let snapshot = harness
            .runtime
            .snapshot(SnapshotRequest {
                workspace_id: harness.workspace_id,
                focused_session_id: Some(child_session_id),
                session_limit: 8,
                message_limit: 4,
            })
            .await
            .unwrap();
        assert_eq!(
            snapshot.focused.unwrap().runs[0].outcome,
            Some(RunOutcome::Completed)
        );
    }

    #[tokio::test]
    async fn shutdown_settles_a_running_parent_and_its_in_flight_child() {
        let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
            requests: Arc::new(StdMutex::new(Vec::new())),
            script: vec![("spawn_agent", r#"{"task":"long research"}"#.to_owned())],
            turn: StdMutex::new(0),
        });
        let hanging: Arc<dyn Provider> = Arc::new(HangingProvider);
        let mut harness = spawn_harness(Vec::new(), vec![parent, hanging], 8).await;
        let parent_run =
            submit_prompt_to(&harness.runtime, harness.session_id, "delegate forever").await;

        let mut observed = Vec::new();
        let (child_session_id, child_run) = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let event = harness.events.next().await.unwrap().unwrap();
                let child = match &event.event {
                    SessionEvent::RunStarted { session, run_id }
                        if *run_id != parent_run
                            && session.parent_id == Some(harness.session_id) =>
                    {
                        Some((session.id, *run_id))
                    }
                    _ => None,
                };
                observed.push(event);
                if let Some(child) = child {
                    break child;
                }
            }
        })
        .await
        .expect("the child must start before shutdown");

        tokio::time::timeout(Duration::from_secs(1), harness.runtime.shutdown())
            .await
            .expect("shutdown must settle the parent and child")
            .unwrap();

        let mut terminal_count = 0;
        tokio::time::timeout(Duration::from_secs(1), async {
            while finished_outcome(&observed, parent_run).is_none()
                || finished_outcome(&observed, child_run).is_none()
            {
                let event = harness.events.next().await.unwrap().unwrap();
                if matches!(event.event, SessionEvent::RunFinished { .. }) {
                    terminal_count += 1;
                }
                observed.push(event);
            }
        })
        .await
        .expect("both accepted runs must publish terminal events");
        assert_eq!(terminal_count, 2);
        assert!(matches!(
            finished_outcome(&observed, parent_run),
            Some(RunOutcome::Cancelled)
        ));
        assert!(matches!(
            finished_outcome(&observed, child_run),
            Some(RunOutcome::Cancelled)
        ));

        let snapshot = harness
            .runtime
            .snapshot(SnapshotRequest {
                workspace_id: harness.workspace_id,
                focused_session_id: Some(child_session_id),
                session_limit: 8,
                message_limit: 4,
            })
            .await
            .unwrap();
        assert!(
            snapshot
                .sessions
                .iter()
                .all(|session| session.active_run_id.is_none())
        );
        assert_eq!(
            snapshot.focused.unwrap().runs[0].outcome,
            Some(RunOutcome::Cancelled)
        );
    }

    #[tokio::test]
    async fn shutdown_closes_child_admission_before_scanning_unfinished_runs() {
        struct PausedChildLoader {
            entered: Arc<tokio::sync::Notify>,
            release: Arc<tokio::sync::Notify>,
        }

        impl RuntimeLoader for PausedChildLoader {
            fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
                self.entered.notify_one();
                let release = Arc::clone(&self.release);
                Box::pin(async move {
                    release.notified().await;
                    Runtime::new(StaticTextProvider, "test-model", 256)
                        .map(|runtime| LoadedRuntime {
                            runtime: Arc::new(runtime),
                            pricing: None,
                        })
                        .map_err(|error| RuntimeLoadError {
                            kind: RunFailureKind::Configuration,
                            message: error.to_string(),
                        })
                })
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(PausedChildLoader {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            }),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let summary = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 1,
                message_limit: 1,
            })
            .await
            .unwrap()
            .focused
            .unwrap()
            .summary;
        let parent_run = RunId::generate().unwrap();
        let parent = ClaimedRun {
            workspace_id,
            workspace: std::fs::canonicalize(directory.path())
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned(),
            session_id,
            run_id: parent_run,
            command_id: CommandId::generate().unwrap(),
            kind: RunKind::Prompt,
            child: false,
            user_initiated: true,
            literal_slash: false,
            model: ModelSelection {
                model: Some("test/model".to_owned()),
                max_output_tokens: Some(256),
                organization: None,
            },
            messages: Vec::new(),
            over_budget: false,
            started: SessionEventEnvelope {
                cursor: created.committed_through,
                session_id,
                run_id: Some(parent_run),
                caused_by: None,
                occurred_at_ms: 0,
                event: SessionEvent::RunStarted {
                    session: summary,
                    run_id: parent_run,
                },
            },
        };
        let child = tokio::spawn(spawn_child_run(
            Arc::clone(&runtime.inner),
            parent,
            Arc::new(Semaphore::new(1)),
            Arc::new(AtomicUsize::new(0)),
            "research".to_owned(),
            None,
        ));
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .expect("child admission must reach its pre-commit load");

        tokio::time::timeout(Duration::from_secs(1), runtime.shutdown())
            .await
            .expect("shutdown must complete while pre-commit child work is paused")
            .unwrap();
        release.notify_one();
        let outcome = tokio::time::timeout(Duration::from_secs(1), child)
            .await
            .expect("released child admission must observe shutdown")
            .unwrap();
        assert!(outcome.is_error);
        assert!(outcome.content.contains("shutting down"));

        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 8,
                message_limit: 1,
            })
            .await
            .unwrap();
        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(snapshot.sessions[0].id, session_id);
        assert_eq!(snapshot.sessions[0].active_run_id, None);
    }

    #[tokio::test]
    async fn concurrent_children_per_run_queue_behind_the_cap() {
        let parent_requests = Arc::new(StdMutex::new(Vec::new()));
        let parent: Arc<dyn Provider> = Arc::new(MultiSpawnProvider {
            requests: Arc::clone(&parent_requests),
            spawns: MAX_CONCURRENT_CHILDREN_PER_RUN + 1,
            arguments: |index| format!(r#"{{"task":"task {index}","model":"test/child"}}"#),
            turn: StdMutex::new(0),
        });
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let child: Arc<dyn Provider> = Arc::new(GaugedTextProvider {
            active: Arc::clone(&active),
            peak: Arc::clone(&peak),
        });
        let mut harness = spawn_harness(vec![("test/child", child)], vec![parent], 8).await;
        let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "fan out").await;
        let observed = collect_until_run_finished(&mut harness.events, run_id).await;

        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(event.event, SessionEvent::SessionCreated { .. }))
                .count(),
            MAX_CONCURRENT_CHILDREN_PER_RUN + 1
        );
        assert!(peak.load(Ordering::Acquire) <= MAX_CONCURRENT_CHILDREN_PER_RUN);
        assert!(peak.load(Ordering::Acquire) >= 1);
        let parent_reqs = parent_requests.lock().unwrap();
        let results = parent_reqs[1].messages()[2].content();
        assert_eq!(results.len(), MAX_CONCURRENT_CHILDREN_PER_RUN + 1);
        for block in results {
            assert!(matches!(
                block,
                ContentBlock::ToolResult { content, is_error: false, .. }
                    if content == "child done"
            ));
        }
        drop(parent_reqs);
        assert!(matches!(
            finished_outcome(&observed, run_id),
            Some(RunOutcome::Completed)
        ));
    }

    #[tokio::test]
    async fn spawns_beyond_the_per_run_budget_return_a_tool_error() {
        let parent_requests = Arc::new(StdMutex::new(Vec::new()));
        let parent: Arc<dyn Provider> = Arc::new(MultiSpawnProvider {
            requests: Arc::clone(&parent_requests),
            spawns: MAX_SPAWNED_CHILDREN_PER_RUN + 1,
            arguments: |index| format!(r#"{{"task":"task {index}","model":"test/child"}}"#),
            turn: StdMutex::new(0),
        });
        let mut harness = spawn_harness(
            vec![("test/child", Arc::new(StaticTextProvider))],
            vec![parent],
            8,
        )
        .await;
        let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "fan out wide").await;
        let observed = collect_until_run_finished(&mut harness.events, run_id).await;

        // Exactly the budget's worth of children were created.
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(event.event, SessionEvent::SessionCreated { .. }))
                .count(),
            MAX_SPAWNED_CHILDREN_PER_RUN
        );
        let parent_reqs = parent_requests.lock().unwrap();
        let results = parent_reqs[1].messages()[2].content();
        assert_eq!(results.len(), MAX_SPAWNED_CHILDREN_PER_RUN + 1);
        let errors = results
            .iter()
            .filter(|block| {
                matches!(
                    block,
                    ContentBlock::ToolResult { content, is_error: true, .. }
                        if content.contains("already spawned")
                )
            })
            .count();
        let successes = results
            .iter()
            .filter(|block| {
                matches!(
                    block,
                    ContentBlock::ToolResult { content, is_error: false, .. }
                        if content == "done"
                )
            })
            .count();
        assert_eq!(errors, 1);
        assert_eq!(successes, MAX_SPAWNED_CHILDREN_PER_RUN);
        drop(parent_reqs);
        assert!(matches!(
            finished_outcome(&observed, run_id),
            Some(RunOutcome::Completed)
        ));
    }

    #[tokio::test]
    async fn saturated_parents_awaiting_children_never_deadlock() {
        // Two parents fill the entire root permit pool and then both await a
        // child. If children drew from the same pool nothing could ever run
        // them; the separate child pool must let every run complete.
        let spawn_script = || -> Arc<dyn Provider> {
            Arc::new(ScriptedRunProvider {
                requests: Arc::new(StdMutex::new(Vec::new())),
                script: vec![(
                    "spawn_agent",
                    r#"{"task":"shared research","model":"test/child"}"#.to_owned(),
                )],
                turn: StdMutex::new(0),
            })
        };
        let mut harness = spawn_harness(
            vec![("test/child", Arc::new(StaticTextProvider))],
            vec![spawn_script(), spawn_script()],
            2,
        )
        .await;
        let created = create_session(&harness.runtime, harness.workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id: second } = created.outcome else {
            panic!("unexpected receipt")
        };
        let first_run =
            submit_prompt_to(&harness.runtime, harness.session_id, "delegate one").await;
        let second_run = submit_prompt_to(&harness.runtime, second, "delegate two").await;

        let mut observed = Vec::new();
        tokio::time::timeout(Duration::from_secs(30), async {
            while finished_outcome(&observed, first_run).is_none()
                || finished_outcome(&observed, second_run).is_none()
            {
                observed.push(harness.events.next().await.unwrap().unwrap());
            }
        })
        .await
        .expect("saturated parents deadlocked instead of completing");
        assert!(matches!(
            finished_outcome(&observed, first_run),
            Some(RunOutcome::Completed)
        ));
        assert!(matches!(
            finished_outcome(&observed, second_run),
            Some(RunOutcome::Completed)
        ));
    }
}
