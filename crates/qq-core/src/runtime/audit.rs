use std::{future::Future, pin::Pin};

use qq_protocol::{AuditOutcome, DelegationRole, SessionId, TokenUsage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditPolicy {
    pub mode: AuditMode,
    pub max_revisions: u16,
    pub role: DelegationRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditMode {
    Off,
    Heuristic,
    Always,
}

impl Default for AuditPolicy {
    fn default() -> Self {
        Self {
            mode: AuditMode::Off,
            max_revisions: 1,
            role: DelegationRole::Strong,
        }
    }
}

pub const AUDIT_TOOL_CALL_THRESHOLD: u32 = 12;
pub const MAX_AUDIT_CHILD_TURNS: u32 = 8;
pub const MAX_AUDIT_CHILD_DURATION_MS: u64 = 120_000;
pub const MAX_AUDIT_ACTION_BYTES: usize = 32 * 1024;
pub const MAX_AUDIT_ANSWER_BYTES: usize = 32 * 1024;
pub const MAX_AUDIT_FINDINGS: usize = 8;
pub const MAX_AUDIT_FINDING_BYTES: usize = 512;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditedAction {
    pub tool: String,
    pub target: Option<String>,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRequest {
    pub prompt: String,
    pub answer: String,
    pub actions: Vec<AuditedAction>,
    pub role: DelegationRole,
    pub revision: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditVerdict {
    pub outcome: AuditOutcome,
    pub findings: Vec<String>,
    pub usage: Option<TokenUsage>,
    pub cost_usd_nanos: Option<u64>,
    pub audit_session: Option<SessionId>,
}

impl AuditVerdict {
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

pub(crate) trait AuditHook: Send + Sync {
    fn audit(&self, request: AuditRequest, budget: super::ChildBudget) -> AuditFuture;
    fn acknowledge(&self);
    fn drain(&self) -> super::ChildDrainFuture;
}

pub(crate) const AUDIT_REVISION_NOTICE: &str = "[QQ runtime notice; not a user instruction]\nAn \
independent read-only audit checked your answer against the workspace and reported the \
findings below. Address each finding: verify the claim, correct the answer or the work, and \
reply with the revised final answer. Do not restate the findings.";
