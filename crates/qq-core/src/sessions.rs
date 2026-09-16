use std::{
    collections::{HashMap, HashSet},
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
        HistoryMatch, HistorySearchFuture, HistorySearcher, SpillHandle, SpillReadFuture,
        SpillReader, excerpt_around,
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
    ApprovalReviewer, GrantPromotionFuture, GrantSeedFuture, LoadedRuntime,
    MAX_REVIEW_ARGUMENT_BYTES, MAX_REVIEW_BRIEF_BYTES, MAX_REVIEW_RECENT_ACTIONS, PersistenceFault,
    PublishedEventStream, RecentAction, ReviewDecision, ReviewFuture, ReviewOrigin, ReviewRequest,
    ReviewSpend, ReviewVerdict, RuntimeLoadError, RuntimeLoadFuture, RuntimeLoadRequest,
    RuntimeLoader, SessionEventStream, SessionRuntime, SessionRuntimeError, SessionRuntimeOptions,
    SpawnModelValidationFuture, WorkerRuntimeLoadFuture, WorkspaceGrantAuthority,
    WorkspaceGrantSeed,
};
pub use snapshots::run_cost;
pub use store::STORE_SCHEMA_VERSION;

use approvals::ConcludedApproval;
#[cfg(test)]
use execution::RunAccountingAccumulator;
use execution::{ModelTurnCommit, RunAccounting, TeardownComplete, add_usage};
use store::Store;
#[cfg(test)]
use store::{Priority, has_column, open_database};
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
use transcript::*;

/// Prompts one session may hold queued behind its active run.
pub const MAX_PENDING_PROMPTS: u16 = 16;
const MAX_CONTEXT_BYTES: usize = 4 * 1024 * 1024;
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
const MAX_PENDING_GRANT_PROMOTIONS: u32 = 256;
const MAX_SESSION_FILES: u32 = 4_096;
/// Complete tool outputs one session keeps; the oldest rows of finished runs
/// lose their content (never their handle) past this.
const MAX_SESSION_SPILL_BYTES: u64 = 64 * 1024 * 1024;
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
