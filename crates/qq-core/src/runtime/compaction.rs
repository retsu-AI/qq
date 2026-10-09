//! In-run compaction: summarizing a run's own earlier turns at a safe turn
//! boundary so the run continues in one window instead of failing.
//!
//! Between runs the session layer compacts before a prompt starts. Within a
//! run, once stubbing stale read-only results no longer fits the window, the
//! loop asks its installed compactor to summarize every turn but the most
//! recent few. The compactor runs the summarizer as an internal run and
//! commits a marker scoped to this run (`session_compactions.scope_run_id`,
//! `turn_cutoff`) so replay renders the summary where the replaced turns
//! stood. The loop then splices the summary into its live transcript and
//! continues. Direct runs have no compactor and fail as before.

use std::{future::Future, pin::Pin, sync::Arc};

use qq_provider::{Message, ToolSpec};

/// What the loop hands the compactor: the run's live request cut after the
/// last turn to replace, and the ordinal of that turn. The compactor sees no
/// retained turns, so it cannot summarize what stays verbatim. The request
/// keeps the run's system prompt, tools, and message prefix so it reads the
/// provider cache the run's own turns wrote (ADR-0056 § 5).
pub(crate) struct InRunCompactionRequest {
    /// The session context before the prompt, the prompt, and everything the
    /// run appended through `turn_cutoff`: assistant turns, tool results,
    /// applied steering, runtime notices.
    pub(crate) transcript: Vec<Message>,
    /// The last model turn ordinal the summary replaces. Turns after it stay
    /// verbatim in the live transcript and in replay.
    pub(crate) turn_cutoff: u32,
    /// The run's system prompt (without a budget-final notice).
    pub(crate) system: Arc<str>,
    /// The run's declared tools. Calls are rejected, never run.
    pub(crate) tools: Arc<[ToolSpec]>,
}

/// A committed in-run summary the loop splices in.
pub(crate) struct InRunCompaction {
    pub(crate) summary: String,
}

pub(crate) type InRunCompactionFuture =
    Pin<Box<dyn Future<Output = Result<InRunCompaction, InRunCompactionError>> + Send + 'static>>;

/// Why an in-run compaction did not commit. The loop fails the run with the
/// context diagnosis for a summarizer or availability failure, and as a
/// server failure when the store could not record the compaction.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum InRunCompactionError {
    /// The summarizer ran and settled failed (rejected, malformed, did not
    /// shrink). Its outcome is durable on its own run.
    #[error("the summarizer failed: {0}")]
    SummarizerFailed(String),
    /// The session layer could not start the compaction (the prompt run
    /// ended, it was cancelled, or the runtime has failed).
    #[error("compaction could not run: {0}")]
    Unavailable(String),
    /// The store failed to start, record, or commit the compaction. Not a
    /// context problem: `/compact` would not help.
    #[error("compaction could not be persisted: {0}")]
    Persistence(String),
}

pub(crate) trait InRunCompactor: Send + Sync {
    fn compact(&self, request: InRunCompactionRequest) -> InRunCompactionFuture;
}
