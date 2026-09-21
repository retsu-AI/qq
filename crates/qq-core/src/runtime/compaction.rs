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

use std::{future::Future, pin::Pin};

use qq_provider::Message;

/// What the loop hands the compactor: the run's transcript from its prompt
/// through the last turn to replace, and the ordinal of that turn. The
/// compactor sees no retained turns, so its budget is the window less the
/// output reserve, and it cannot summarize what stays verbatim.
pub(crate) struct InRunCompactionRequest {
    /// The prompt and everything the run appended through `turn_cutoff`:
    /// assistant turns, tool results, applied steering, runtime notices. The
    /// preceding session context (already covered by between-run compaction)
    /// is not included; the summarizer is told it is continuing a task.
    pub(crate) transcript: Vec<Message>,
    /// The last model turn ordinal the summary replaces. Turns after it stay
    /// verbatim in the live transcript and in replay.
    pub(crate) turn_cutoff: u32,
}

/// A committed in-run summary the loop splices in.
pub(crate) struct InRunCompaction {
    pub(crate) summary: String,
}

pub(crate) type InRunCompactionFuture =
    Pin<Box<dyn Future<Output = Result<InRunCompaction, InRunCompactionError>> + Send + 'static>>;

/// Why an in-run compaction did not commit. The loop fails the run with the
/// context diagnosis either way; the variant only shapes the message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum InRunCompactionError {
    /// The summarizer ran and settled failed (rejected, malformed, did not
    /// shrink). Its outcome is durable on its own run.
    #[error("the summarizer failed: {0}")]
    SummarizerFailed(String),
    /// The session layer could not start or record the compaction.
    #[error("compaction could not run: {0}")]
    Unavailable(String),
}

pub(crate) trait InRunCompactor: Send + Sync {
    fn compact(&self, request: InRunCompactionRequest) -> InRunCompactionFuture;
}
