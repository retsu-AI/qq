use std::{future::Future, pin::Pin};

use qq_protocol::{AuditOutcome, DelegationRole, SessionId, TokenUsage};

/// When a root run's candidate final answer is audited before completion.
/// Application-neutral mirror of the configured `audit` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditPolicy {
    pub mode: AuditMode,
    /// Most revision cycles the audit may send the run through.
    pub max_revisions: u16,
    /// The roster role the auditor runs as, when the roster declares one.
    pub role: DelegationRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditMode {
    Off,
    Heuristic,
    Always,
}

/// The runtime default is `Off`: audits are enabled by the configured `audit`
/// section (whose own default is `heuristic`), never by an embedded runtime
/// that has no roster to audit with.
impl Default for AuditPolicy {
    fn default() -> Self {
        Self {
            mode: AuditMode::Off,
            max_revisions: 1,
            role: DelegationRole::Strong,
        }
    }
}

/// Tool calls at or above which the heuristic considers a run substantial.
pub const AUDIT_TOOL_CALL_THRESHOLD: u32 = 12;
/// Bytes of the action summary (tool names, paths, diff summaries) quoted to
/// the auditor.
pub const MAX_AUDIT_ACTION_BYTES: usize = 32 * 1024;
/// Bytes of the candidate answer quoted to the auditor.
pub const MAX_AUDIT_ANSWER_BYTES: usize = 32 * 1024;
/// Most findings kept from one audit, and the bound on each.
pub const MAX_AUDIT_FINDINGS: usize = 8;
pub const MAX_AUDIT_FINDING_BYTES: usize = 512;

/// What one run did, as the heuristic trigger sees it. Maintained by the run
/// loop from the tool results it observed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct AuditTriggers {
    pub(crate) mutated_files: bool,
    pub(crate) non_read_shell: bool,
    pub(crate) tool_calls: u32,
    pub(crate) spawned_children: bool,
}

impl AuditTriggers {
    pub(crate) const fn fires(self, mode: AuditMode) -> bool {
        match mode {
            AuditMode::Off => false,
            AuditMode::Always => true,
            AuditMode::Heuristic => {
                self.mutated_files
                    || self.non_read_shell
                    || self.tool_calls >= AUDIT_TOOL_CALL_THRESHOLD
                    || self.spawned_children
            }
        }
    }
}

/// One finished tool call, for the action summary the auditor reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditedAction {
    pub tool: String,
    /// The `path` or `command` argument when the tool has one.
    pub target: Option<String>,
    pub is_error: bool,
}

/// Everything the auditor sees. The transcript is deliberately absent: the
/// auditor verifies claims against the workspace with its own read-only tools
/// rather than trusting the run's account of itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRequest {
    /// The user prompt that started the run.
    pub prompt: String,
    /// The candidate final answer, truncated to `MAX_AUDIT_ANSWER_BYTES`.
    pub answer: String,
    /// Finished tool calls in order, bounded to `MAX_AUDIT_ACTION_BYTES` when
    /// rendered.
    pub actions: Vec<AuditedAction>,
    pub role: DelegationRole,
    /// Which revision this is auditing (0 = the first answer).
    pub revision: u16,
}

/// The auditor's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditVerdict {
    pub outcome: AuditOutcome,
    pub findings: Vec<String>,
    pub usage: Option<TokenUsage>,
    pub cost_usd_nanos: Option<u64>,
    /// The audit child's session, when one ran.
    pub audit_session: Option<SessionId>,
}

impl AuditVerdict {
    /// An audit that could not run: fail-open, recorded as unavailable.
    #[must_use]
    pub fn unavailable() -> Self {
        Self {
            outcome: AuditOutcome::Unavailable,
            findings: Vec::new(),
            usage: Some(TokenUsage::default()),
            cost_usd_nanos: Some(0),
            audit_session: None,
        }
    }
}

pub type AuditFuture = Pin<Box<dyn Future<Output = AuditVerdict> + Send + 'static>>;

/// Audits a root run's candidate answer. The session runtime implements this
/// by spawning a read-only child (and publishes `RunAuditStarted` itself when
/// it does); direct runs have none. Must never hang: failures resolve as
/// `Unavailable`. The loop treats the verdict as advisory: revise at most
/// `max_revisions` times, then complete.
pub(crate) trait AuditHook: Send + Sync {
    fn audit(&self, request: AuditRequest, budget: super::ChildBudget) -> AuditFuture;
    fn acknowledge(&self);
    fn drain(&self) -> super::ChildDrainFuture;
}

/// Sent to the model when the auditor asked for a revision.
pub(crate) const AUDIT_REVISION_NOTICE: &str = "[QQ runtime notice; not a user instruction]\nAn \
independent read-only audit checked your answer against the workspace and reported the \
findings below. Address each finding: verify the claim, correct the answer or the work, and \
reply with the revised final answer. Do not restate the findings.";
