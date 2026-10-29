use serde::{Deserialize, Serialize};

use crate::{
    AgentProfileId, AuditRecord, ContentHash, Correlation, FinalOutput, ModelSelection, RunId,
    RunPromptIdentity, SessionEventEnvelope, SessionId, TokenUsage, WorkspaceId,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HeadlessRecord {
    Trial(Box<HeadlessTrial>),
    Event { envelope: Box<SessionEventEnvelope> },
    Outcome(Box<HeadlessOutcome>),
    GoalRun(Box<HeadlessGoalRun>),
    Goal(Box<HeadlessGoalOutcome>),
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HeadlessRecordRef<'a> {
    Trial(&'a HeadlessTrial),
    Event { envelope: &'a SessionEventEnvelope },
    Outcome(&'a HeadlessOutcome),
    GoalRun(&'a HeadlessGoalRun),
    Goal(&'a HeadlessGoalOutcome),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadlessTrial {
    pub qq_version: String,
    pub qq_source_revision: String,
    pub protocol_version: u16,
    pub workspace_identity: ContentHash,
    pub model: ModelSelection,
    pub profile: AgentProfileId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_provenance: Option<String>,
    pub approval: HeadlessApproval,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd_nanos: Option<u64>,
    #[serde(default, skip_serializing_if = "Correlation::is_empty")]
    pub correlation: Correlation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema_sha256: Option<ContentHash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_repair_turns: Option<u8>,
    pub workspace_id: WorkspaceId,
    pub session_id: SessionId,
    pub run_id: RunId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadlessOutcome {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovered_answer: Option<String>,
    pub status: HeadlessStatus,
    pub exit_code: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd_nanos: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_identity: Option<Box<RunPromptIdentity>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit: Option<Box<AuditRecord>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_output: Option<Box<FinalOutput>>,
}

impl HeadlessOutcome {
    #[must_use]
    pub const fn is_well_formed(&self) -> bool {
        self.exit_code == self.status.code()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadlessStatus {
    Completed,
    TaskFailed,
    InvalidConfiguration,
    TimedOut,
    BudgetExhausted,
    HarnessFailure,
    NeedsInput,
    Interrupted,
}

impl HeadlessStatus {
    pub const ALL: [Self; 8] = [
        Self::Completed,
        Self::TaskFailed,
        Self::InvalidConfiguration,
        Self::TimedOut,
        Self::BudgetExhausted,
        Self::HarnessFailure,
        Self::NeedsInput,
        Self::Interrupted,
    ];

    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Completed => 0,
            Self::TaskFailed => 1,
            Self::InvalidConfiguration => 2,
            Self::TimedOut | Self::BudgetExhausted => 3,
            Self::HarnessFailure => 4,
            Self::NeedsInput => 5,
            Self::Interrupted => 130,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::TaskFailed => "task_failed",
            Self::InvalidConfiguration => "invalid_configuration",
            Self::TimedOut => "timed_out",
            Self::BudgetExhausted => "budget_exhausted",
            Self::HarnessFailure => "harness_failure",
            Self::NeedsInput => "needs_input",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HeadlessApproval {
    ReadOnly,
    Ask,
    Auto,
    Full,
}

impl HeadlessApproval {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Ask => "ask",
            Self::Auto => "auto",
            Self::Full => "full",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EventCursor, PROTOCOL_VERSION, SessionEvent, StoreId};

    fn trial() -> HeadlessTrial {
        HeadlessTrial {
            qq_version: "0.1.0".to_owned(),
            qq_source_revision: "unknown".to_owned(),
            protocol_version: PROTOCOL_VERSION,
            workspace_identity: ContentHash::from_bytes([0x44; 32]),
            model: ModelSelection {
                model_is_fallback: false,
                model: Some("openai/gpt-5.6".to_owned()),
                max_output_tokens: None,
                organization: None,
            },
            profile: AgentProfileId::new("default").unwrap(),
            context_window: None,
            pricing_provenance: None,
            approval: HeadlessApproval::ReadOnly,
            timeout_seconds: None,
            max_turns: None,
            max_cost_usd_nanos: None,
            correlation: Correlation::default(),
            arm: None,
            output_schema_sha256: None,
            output_repair_turns: None,
            workspace_id: WorkspaceId::from_bytes([2; 16]),
            session_id: SessionId::from_bytes([3; 16]),
            run_id: RunId::from_bytes([4; 16]),
        }
    }

    fn envelope() -> SessionEventEnvelope {
        SessionEventEnvelope {
            cursor: EventCursor {
                store_id: StoreId::from_bytes([1; 16]),
                workspace_id: WorkspaceId::from_bytes([2; 16]),
                sequence: 7,
            },
            session_id: SessionId::from_bytes([3; 16]),
            run_id: Some(RunId::from_bytes([4; 16])),
            caused_by: None,
            occurred_at_ms: 1_700_000_000_000,
            event: SessionEvent::SessionDeleted {
                session_id: SessionId::from_bytes([3; 16]),
            },
        }
    }

    fn outcome() -> HeadlessOutcome {
        HeadlessOutcome {
            recovered_answer: None,
            status: HeadlessStatus::Completed,
            exit_code: 0,
            message: None,
            usage: None,
            estimated_cost_usd_nanos: None,
            prompt_identity: None,
            audit: None,
            final_output: None,
        }
    }

    #[test]
    fn the_borrowing_view_encodes_exactly_as_the_owned_record() {
        let trial = trial();
        let envelope = envelope();
        let outcome = outcome();
        let pairs = [
            (
                HeadlessRecord::Trial(Box::new(trial.clone())),
                HeadlessRecordRef::Trial(&trial),
            ),
            (
                HeadlessRecord::Event {
                    envelope: Box::new(envelope.clone()),
                },
                HeadlessRecordRef::Event {
                    envelope: &envelope,
                },
            ),
            (
                HeadlessRecord::Outcome(Box::new(outcome.clone())),
                HeadlessRecordRef::Outcome(&outcome),
            ),
        ];
        for (owned, borrowed) in &pairs {
            let owned_line = serde_json::to_string(owned).unwrap();
            assert_eq!(serde_json::to_string(borrowed).unwrap(), owned_line);
            let decoded: HeadlessRecord = serde_json::from_str(&owned_line).unwrap();
            assert_eq!(&decoded, owned);
        }
    }

    #[test]
    fn records_fail_closed_on_unknown_types_and_fields() {
        let unknown_type = r#"{"type":"heartbeat","at":1}"#;
        assert!(serde_json::from_str::<HeadlessRecord>(unknown_type).is_err());

        let mut with_extra: serde_json::Value =
            serde_json::to_value(HeadlessRecord::Outcome(Box::new(outcome()))).unwrap();
        with_extra["billing"] = serde_json::json!({"cents": 3});
        assert!(serde_json::from_value::<HeadlessRecord>(with_extra).is_err());

        let bad_status = r#"{"type":"outcome","status":"succeeded","exit_code":0}"#;
        assert!(serde_json::from_str::<HeadlessRecord>(bad_status).is_err());
    }

    #[test]
    fn absent_optional_fields_are_omitted_not_null() {
        let line = serde_json::to_string(&HeadlessRecordRef::Trial(&trial())).unwrap();
        for absent in [
            "context_window",
            "pricing_provenance",
            "timeout_seconds",
            "max_turns",
            "max_cost_usd_nanos",
            "correlation",
            "arm",
            "output_schema_sha256",
            "output_repair_turns",
        ] {
            assert!(!line.contains(absent), "{absent} must be omitted: {line}");
        }
        let line = serde_json::to_string(&HeadlessRecordRef::Outcome(&outcome())).unwrap();
        assert_eq!(
            line,
            r#"{"type":"outcome","status":"completed","exit_code":0}"#
        );
    }

    #[test]
    fn the_exit_table_is_fixed() {
        let codes: Vec<(&str, u8)> = HeadlessStatus::ALL
            .iter()
            .map(|status| (status.as_str(), status.code()))
            .collect();
        assert_eq!(
            codes,
            [
                ("completed", 0),
                ("task_failed", 1),
                ("invalid_configuration", 2),
                ("timed_out", 3),
                ("budget_exhausted", 3),
                ("harness_failure", 4),
                ("needs_input", 5),
                ("interrupted", 130),
            ]
        );
        for status in HeadlessStatus::ALL {
            assert_eq!(
                serde_json::to_string(&status).unwrap(),
                format!("\"{}\"", status.as_str())
            );
        }
        for approval in [
            HeadlessApproval::ReadOnly,
            HeadlessApproval::Auto,
            HeadlessApproval::Full,
        ] {
            assert_eq!(
                serde_json::to_string(&approval).unwrap(),
                format!("\"{}\"", approval.as_str())
            );
        }
        assert!(outcome().is_well_formed());
        assert!(
            !HeadlessOutcome {
                exit_code: 3,
                ..outcome()
            }
            .is_well_formed()
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadlessGoalRun {
    pub workspace_id: WorkspaceId,
    pub session_id: SessionId,
    pub goal: crate::GoalIdentity,
    pub run: Box<crate::RunSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadlessGoalOutcome {
    pub workspace_id: WorkspaceId,
    pub goal: Box<crate::GoalSnapshot>,
    pub status: HeadlessGoalStatus,
    pub exit_code: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadlessGoalStatus {
    Achieved,
    Paused,
    Blocked,
    BudgetExhausted,
    Interrupted,
    HarnessFailure,
}

impl HeadlessGoalStatus {
    #[must_use]
    pub const fn from_goal(status: &crate::GoalStatus) -> Option<Self> {
        match status {
            crate::GoalStatus::Active => None,
            crate::GoalStatus::Achieved => Some(Self::Achieved),
            crate::GoalStatus::Paused { .. } => Some(Self::Paused),
            crate::GoalStatus::Blocked { .. } => Some(Self::Blocked),
            crate::GoalStatus::BudgetExhausted { .. } => Some(Self::BudgetExhausted),
        }
    }

    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Achieved => 0,
            Self::Paused => 6,
            Self::Blocked => 7,
            Self::BudgetExhausted => 8,
            Self::Interrupted => 130,
            Self::HarnessFailure => 4,
        }
    }
}
