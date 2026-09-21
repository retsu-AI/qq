use std::{
    collections::HashMap,
    future::Future,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_stream::stream;
use futures_core::Stream;
use futures_util::{FutureExt, StreamExt};
use qq_protocol::{
    AccountingTotal, AgentProfileId, ApprovalDecision, ApprovalGrant, ApprovalMode,
    ApprovalResolution, AuditOutcome, AuditRecord, CapabilitySupport, CommandId, CommandOutcome,
    CommandReceipt, ContentHash, Correlation, EditPreview, EventCursor, FetchPreview, FinalOutput,
    InputPart, MessageId, MessageRole, MessageSnapshot, MessageState, ModelPricing, ModelSelection,
    QuestionPreview, ReasoningEvent, ResolvedModel, RunActivity, RunFailure, RunFailureKind, RunId,
    RunLimits, RunOutcome, RunPlanIdentity, RunPromptIdentity, RunSnapshot, RunStatus,
    SessionAccounting, SessionCommand, SessionEvent, SessionEventEnvelope, SessionId,
    SessionPurpose, SessionSnapshot, SessionStatus, SessionSummary, ShellCommandPreview,
    ShellVerdict, SnapshotRequest, SpawnOrigin, StoreId, SubscribeRequest, TextChannel, TokenUsage,
    ToolCallDisplay, ToolCallId, ToolCallSnapshot, ToolCallState, WorkspaceGrantOutcome,
    WorkspaceId, WorkspaceSnapshot, WorkspaceSummary, validate_input,
};
use qq_provider::{ContentBlock, Message, Role};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{RwLock, Semaphore, mpsc, oneshot, watch};

use crate::{
    GateDecision, PreparedRequestWeight, PreparedStaticPrefix, RunCancellation, RunCapabilities,
    Runtime, RuntimeEvent, RuntimeToolCall, SpawnAgentFuture, SpawnAgentOutcome, SpawnAgentSpend,
    SpawnRequest, SubagentSpawner, ToolGate, ToolGateFuture, approval,
    catalog::EffectClass,
    runtime::{
        HISTORY_SCAN_BUDGET_BYTES, HistoryMatch, HistorySearch, HistorySearchFuture,
        HistorySearcher, SpillHandle, SpillReadFuture, SpillReader, excerpt_around,
    },
    workspace::{FileState, FileStateUpdate},
};

mod approvals;
mod claim;
mod codec;
mod commands;
mod compaction;
pub(crate) mod context;
mod events;
mod execution;
mod feed;
mod in_run_compaction;
mod runtime;
mod scheduler;
mod settlement;
mod snapshots;
mod store;
mod streaming;
mod subagents;
#[cfg(test)]
mod tests;
mod tool_calls;
mod transcript;

pub use commands::MAX_DESCENDANTS_PER_ROOT;
pub use feed::PublishedEvent;
pub use runtime::{
    ApprovalReviewer, CheckpointSelection, GrantPromotionFuture, GrantSeedFuture, LoadedRuntime,
    MAX_REVIEW_ARGUMENT_BYTES, MAX_REVIEW_BRIEF_BYTES, MAX_REVIEW_RECENT_ACTIONS, PersistenceFault,
    PublishedEventStream, RecentAction, ReviewDecision, ReviewFuture, ReviewOrigin, ReviewRequest,
    ReviewSpend, ReviewVerdict, RoutingSelection, RuntimeLoadError, RuntimeLoadFuture,
    RuntimeLoadProgress, RuntimeLoadRequest, RuntimeLoadStage, RuntimeLoader, SessionEventStream,
    SessionRuntime, SessionRuntimeError, SessionRuntimeOptions, SpawnModelValidationFuture,
    TaskRouter, TaskRoutingFuture, WorkerRuntimeLoadFuture, WorkspaceGrantAuthority,
    WorkspaceGrantSeed,
};
pub use snapshots::run_cost;
pub use store::STORE_SCHEMA_VERSION;

/// Entry points for the `context_assembly` bench. Not a public API.
#[doc(hidden)]
pub mod bench_support {
    use super::*;

    /// Opens a store at `path` holding one session whose transcript has
    /// `archived_runs` completed prompt runs behind a compaction marker and
    /// `retained_runs` after it. Every run has `turns` model turns, each with
    /// one tool call whose result is `result_bytes` long. Returns the
    /// connection and the session id.
    #[must_use]
    pub fn seed_compacted_session(
        path: &Path,
        archived_runs: usize,
        retained_runs: usize,
        turns: u32,
        result_bytes: usize,
    ) -> (Connection, SessionId) {
        let (mut connection, _) = open_database(&path.to_path_buf()).expect("bench store opens");
        let workspace_id = WorkspaceId::from_bytes([1; 16]);
        let session_id = SessionId::from_bytes([2; 16]);
        let transaction = connection.transaction().expect("bench transaction");
        transaction
            .execute(
                "INSERT INTO workspaces(id, path, next_sequence) VALUES (?1, '/w', 0)",
                [workspace_id.to_string()],
            )
            .expect("workspace row");
        transaction
            .execute(
                "INSERT INTO sessions(id, workspace_id, title, status, model,
                                      created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, 'bench', 'idle', 'bench/model', 1, 2)",
                params![session_id.to_string(), workspace_id.to_string()],
            )
            .expect("session row");
        let result = "r".repeat(result_bytes);
        let total = archived_runs + retained_runs;
        let mut ordinal: u64 = 0;
        let mut cutoff_ordinal = 0;
        for index in 0..total {
            let run_id = RunId::generate().expect("run id");
            let user_id = MessageId::generate().expect("message id");
            let assistant_id = MessageId::generate().expect("message id");
            ordinal += 1;
            transaction
                .execute(
                    "INSERT INTO runs(id, session_id, command_id, user_message_id,
                                      assistant_message_id, status, outcome_json,
                                      created_at_ms, started_at_ms, finished_at_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'completed', ?6, 1, 1, 2)",
                    params![
                        run_id.to_string(),
                        session_id.to_string(),
                        CommandId::generate().expect("command id").to_string(),
                        user_id.to_string(),
                        assistant_id.to_string(),
                        serde_json::to_string(&RunOutcome::Completed).expect("outcome"),
                    ],
                )
                .expect("run row");
            transaction
                .execute(
                    "INSERT INTO messages(id, session_id, run_id, ordinal, role, state,
                                          output, created_at_ms)
                     VALUES (?1, ?2, ?3, ?4, 'user', 'complete', ?5, 1)",
                    params![
                        user_id.to_string(),
                        session_id.to_string(),
                        run_id.to_string(),
                        ordinal,
                        format!("prompt {index} needle-{index}"),
                    ],
                )
                .expect("user message row");
            ordinal += 1;
            transaction
                .execute(
                    "INSERT INTO messages(id, session_id, run_id, ordinal, role, state,
                                          output, created_at_ms)
                     VALUES (?1, ?2, ?3, ?4, 'assistant', 'complete', '', 1)",
                    params![
                        assistant_id.to_string(),
                        session_id.to_string(),
                        run_id.to_string(),
                        ordinal,
                    ],
                )
                .expect("assistant message row");
            for turn in 1..=turns {
                let call_id = format!("call-{index}-{turn}");
                let content = vec![
                    PersistedContentBlock::Text {
                        text: format!("turn {turn} of run {index}"),
                    },
                    PersistedContentBlock::ToolCall {
                        id: call_id.clone(),
                        name: "read_file".to_owned(),
                        arguments: serde_json::json!({ "path": format!("f{turn}.rs") }),
                    },
                ];
                transaction
                    .execute(
                        "INSERT INTO model_turns(run_id, turn_ordinal, assistant_content_json,
                                                 completed_at_ms, truncated)
                         VALUES (?1, ?2, ?3, 1, 0)",
                        params![
                            run_id.to_string(),
                            turn,
                            serde_json::to_string(&content).expect("turn json"),
                        ],
                    )
                    .expect("model turn row");
                transaction
                    .execute(
                        "INSERT INTO tool_calls(id, run_id, turn_ordinal, call_ordinal,
                                                provider_call_id, name, arguments_json, state,
                                                result, requested_at_ms, effect)
                         VALUES (?1, ?2, ?3, 1, ?4, 'read_file', '{}', 'completed', ?5, 1,
                                 'read_only')",
                        params![
                            ToolCallId::generate().expect("tool call id").to_string(),
                            run_id.to_string(),
                            turn,
                            call_id,
                            result,
                        ],
                    )
                    .expect("tool call row");
            }
            if index + 1 == archived_runs {
                cutoff_ordinal = ordinal;
            }
        }
        if archived_runs > 0 {
            transaction
                .execute(
                    "INSERT INTO session_compactions(session_id, run_id, summary, cutoff_ordinal,
                                                     before_bytes, after_bytes, created_at_ms)
                     VALUES (?1, ?2, 'summary of the archive', ?3, 1, 1, 1)",
                    params![
                        session_id.to_string(),
                        RunId::generate().expect("run id").to_string(),
                        cutoff_ordinal,
                    ],
                )
                .expect("compaction row");
        }
        transaction.commit().expect("bench seed commits");
        (connection, session_id)
    }

    /// Assembles the session's model context as the next run would.
    #[must_use]
    pub fn assemble(connection: &Connection, session_id: SessionId) -> usize {
        load_model_context(connection, session_id, u64::MAX)
            .expect("assembly")
            .len()
    }

    /// Searches the session's full history for `query`; returns the match
    /// count and whether the scan budget truncated the walk.
    #[must_use]
    pub fn search(connection: &Connection, session_id: SessionId, query: &str) -> (usize, bool) {
        let search = search_session_history(
            connection,
            session_id,
            RunId::from_bytes([9; 16]),
            query,
            crate::runtime::MAX_HISTORY_MATCHES,
        )
        .expect("search");
        (search.matches.len(), search.truncated)
    }
}

use approvals::ConcludedApproval;
#[cfg(test)]
use execution::RunAccountingAccumulator;
pub(crate) use execution::add_usage;
use execution::{ModelTurnCommit, RunAccounting, TeardownComplete};
use store::Store;
use store::open_database;
#[cfg(test)]
use store::{Priority, has_column};
#[cfg(test)]
use subagents::spawn_child_run;

// The persistence body was one file until H21.2. Its halves import each other
// freely through these globs; the split is by concern, not by a dependency
// order, and every item stays `pub(super)`.
use claim::*;
use codec::*;
use commands::*;
use compaction::*;
use events::*;
use settlement::*;
use snapshots::*;
use streaming::*;
use tool_calls::*;
pub(crate) use transcript::prune_stale_tool_results;
use transcript::*;

/// Prompts one session may hold queued behind its active run.
pub const MAX_PENDING_PROMPTS: u16 = 16;
const MAX_CONTEXT_BYTES: usize = 4 * 1024 * 1024;
/// The assembly recency window: the last K model turns keep their tool
/// results verbatim. Read-only results older than that are replaced by
/// one-line stubs during context assembly (the stored rows are untouched).
/// Sits with the context budget because the budget measures the assembled,
/// pruned size.
pub(crate) const CONTEXT_PRUNE_KEEP_TURNS: usize = 4;
/// Longest argument excerpt embedded in a pruned-result stub.
const CONTEXT_PRUNE_STUB_ARGUMENT_BYTES: usize = 256;
/// Compaction summaries retained per session, newest first. History is kept
/// (not deleted eagerly) so a bad compaction can be rolled back later.
const COMPACTION_HISTORY_ROWS: u32 = 3;
/// Assemblies at or below this size are compacted without a shrinkage check:
/// the structured summary's fixed framing can legitimately exceed a tiny
/// transcript that a small model window still could not fit.
const COMPACTION_SHRINKAGE_FLOOR_BYTES: usize = 16 * 1024;
/// Longest summary excerpt carried on the `SessionCompacted` event.
const MAX_EVENT_SUMMARY_BYTES: usize = 16 * 1024;
const MAX_PROMPT_BYTES: usize = 128 * 1024;
/// Events per page a subscriber is served from its cursor.
pub const MAX_REPLAY_EVENTS: u16 = 128;
const MAX_SNAPSHOT_SESSIONS: u16 = 512;
const MAX_SNAPSHOT_MESSAGES: u16 = 256;
const MAX_SNAPSHOT_TOOL_CALLS: usize = 4_096;
/// Bytes of body text one snapshot may carry across its focused and included
/// session bodies. Below `qq_protocol::MAX_SNAPSHOT_BYTES` by enough to cover
/// the fixed per-row envelope and the workspace's session summaries, so a
/// response assembled under this budget always serializes under the wire cap.
const SNAPSHOT_BODY_BUDGET_BYTES: usize = 6 * 1024 * 1024;
/// Serialized overhead charged per snapshot row beyond its text: ids, run and
/// session references, ordinals, states, and JSON punctuation.
const SNAPSHOT_ROW_OVERHEAD_BYTES: usize = 512;
const MAX_TEXT_CHUNK_BYTES: usize = 64 * 1024;
const MAX_FAILURE_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_WORKSPACES: u32 = 1024;
const MAX_SESSIONS_PER_WORKSPACE: u32 = 512;
/// Durable command receipts a store admits for commands that create work
/// (`ResolveWorkspace`, `CreateSession`, `SubmitPrompt`, `SteerRun`,
/// `SetSession*`, `CompactSession`). At the bound, new work is refused with
/// `CommandLimitReached`; commands that only stop, resolve, or remove existing
/// work keep being admitted up to `MAX_COMMANDS_WITH_CONTROL_HEADROOM`, so a
/// full store can still be cancelled, approved, and cleaned up. The receipt
/// table is the durable idempotency record and is never trimmed to make room.
const MAX_COMMANDS: u32 = 100_000;
/// Receipts reserved above `MAX_COMMANDS` for control and cleanup commands
/// alone. New work cannot consume them; only the control lane reaches them.
const MAX_COMMANDS_WITH_CONTROL_HEADROOM: u32 = MAX_COMMANDS + 10_000;
const MAX_MODEL_SELECTION_BYTES: usize = 512;
const OUTPUT_BATCH_BYTES: usize = 8 * 1024;
const OUTPUT_BATCH_DELAY: Duration = Duration::from_millis(8);
const MAX_PERSISTED_EVENT_BYTES: usize = 1024 * 1024;
const MAX_GRANT_BYTES: usize = 256;
const MAX_SESSION_GRANTS: u32 = 256;
const MAX_PENDING_GRANT_PROMOTIONS: u32 = 256;
const MAX_SESSION_FILES: u32 = 4_096;
/// Complete tool outputs one session keeps; the oldest rows of finished runs
/// lose their content (never their handle) past this.
const MAX_SESSION_SPILL_BYTES: u64 = 64 * 1024 * 1024;
/// Attached-file bytes one session keeps for context reconstruction; the
/// oldest blobs of finished runs lose their content (never their row) past
/// this, and the reconstructed prompt says so.
const MAX_SESSION_ATTACHMENT_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
/// Child runs one parent run may hold in flight at once. Spawn calls beyond
/// this cap queue behind it inside the parent's turn rather than failing.
/// `RunLimits::max_concurrent_children` may lower it per run, never raise it.
pub const MAX_CONCURRENT_CHILDREN_PER_RUN: u16 = 3;
/// Total children one parent run may spawn before further `spawn_agent`
/// calls return a tool error. `RunLimits::max_children` may lower it per run.
pub const MAX_SPAWNED_CHILDREN_PER_RUN: u16 = 8;
/// Deepest sub-agent nesting the runtime executes: the hard ceiling on
/// `delegation.max_depth`. The effective depth of one tree is the roster's
/// `max_depth` (default 1); runs at that depth receive no spawner.
pub const MAX_CHILD_DEPTH: u16 = 3;
/// Alias kept for capability documents: the ceiling clients validate against.
pub const MAX_CHILD_DEPTH_CEILING: u16 = MAX_CHILD_DEPTH;
/// Most routes a delegation roster may declare; the config layer enforces
/// the same bound, this one is the runtime's and is advertised.
pub const MAX_DELEGATION_ROSTER: u16 = 8;
const INTERRUPTED_TOOL_RESULT: &str =
    "Tool execution was interrupted before a durable result was recorded.";
const RUNTIME_NOTICE_PREAMBLE: &str = "[QQ runtime notice; not a user instruction]";
const RUNTIME_NOTICE_GUIDANCE: &str = "Continue from the committed history above. Do not \
    automatically retry tool calls whose result says execution was interrupted.";
/// Read-only built-in tools whose results context assembly may replace with
/// stubs when the call predates the stored effect class (schema 26): the
/// agent can re-derive them on demand. Calls with a stored effect prune by
/// that class instead; mutating, shell, and external results are never pruned.
const PRUNABLE_READ_ONLY_TOOLS: [&str; 6] = [
    "read_file",
    "tree",
    // Pre-v0.1.0 name of `tree`; not a tool, but rows recorded before schema
    // 26 stored no effect class and are pruned by this list.
    "list_dir",
    "search",
    "search_history",
    "read_tool_result",
];
/// Prefixes the latest compaction summary when assembly replays it as the
/// conversation's opening message.
const COMPACTION_SUMMARY_PREAMBLE: &str = "The earlier part of this conversation was compacted \
into the summary below. Treat it as authoritative context; the verbatim conversation resumes \
after it.";
/// Preamble for a summary that replaced earlier turns of the *current* run:
/// the task prompt stands verbatim above it; the model's own work so far is
/// what was summarized.
pub(crate) const IN_RUN_COMPACTION_PREAMBLE: &str = "Your earlier work on this task was compacted into \
the summary below. Treat it as authoritative: the tool results it describes were real and \
their effects stand. The verbatim conversation resumes after it; continue the task.";
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
/// The section headings `COMPACTION_INSTRUCTION` demands, in order. A summary
/// missing any of them is rejected before it can replace the transcript.
const COMPACTION_REQUIRED_SECTIONS: [&str; 6] = [
    "Intent",
    "Decisions and constraints",
    "Work state",
    "Files touched",
    "Errors",
    "User messages",
];

/// Where an in-run compaction cuts a run's live transcript. `run_messages`
/// is everything after the prompt: assistant turns, tool results, steering,
/// notices. Returns the index just past the last message to replace and the
/// ordinal (1-based, counting assistant turns) of the last replaced turn,
/// keeping the final `keep_turns` assistant turns and everything after them
/// verbatim. `None` when fewer than `keep_turns + 1` turns exist: a run that
/// short has nothing worth summarizing and the caller fails as before.
pub(crate) fn in_run_compaction_boundary(
    run_messages: &[Message],
    keep_turns: usize,
) -> Option<(usize, u32)> {
    let assistant_positions: Vec<usize> = run_messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role() == Role::Assistant)
        .map(|(index, _)| index)
        .collect();
    let replaced_turns = assistant_positions.len().checked_sub(keep_turns)?;
    if replaced_turns == 0 {
        return None;
    }
    // Replace through the message just before the first kept assistant
    // turn, so the kept turns' preceding results/steering stay with them. A
    // prior in-run summary at the head of `run_messages` is replaced too:
    // the new summary folds it, as between-run summaries fold each other.
    let first_kept = assistant_positions[replaced_turns];
    Some((first_kept, u32::try_from(replaced_turns).ok()?))
}
