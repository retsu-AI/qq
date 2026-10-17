//! Goal defaults and administrator ceilings, independent of runtime ownership.
use crate::{ConfigError, SourceIdentity};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Default limits for one goal-sponsored run; absent fields impose no extra cap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GoalRunLimits {
    pub max_duration_ms: Option<u64>,
    pub max_model_turns: Option<u32>,
    pub max_tool_calls: Option<u32>,
    pub max_total_tokens: Option<u64>,
    pub max_cost_usd_nanos: Option<u64>,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub max_tool_output_bytes: Option<u64>,
    pub max_children: Option<u16>,
    pub max_concurrent_children: Option<u16>,
}

/// Effective defaults for new goals. Durations are in seconds unless named otherwise.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GoalsConfig {
    pub default_deadline_seconds: u64,
    pub max_deadline_seconds: u64,
    pub default_max_runs: u32,
    pub max_runs: u32,
    pub max_backoff_seconds: u64,
    pub check_timeout_seconds: u64,
    pub approval_wait_seconds: u64,
    pub per_run: GoalRunLimits,
}

impl Default for GoalsConfig {
    fn default() -> Self {
        Self {
            default_deadline_seconds: 86400,
            max_deadline_seconds: 604800,
            default_max_runs: 200,
            max_runs: 2000,
            max_backoff_seconds: 1800,
            check_timeout_seconds: 600,
            approval_wait_seconds: 600,
            per_run: GoalRunLimits {
                max_model_turns: Some(256),
                ..GoalRunLimits::default()
            },
        }
    }
}

/// Only managed configuration may declare these monotonic goal ceilings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GoalCeilings {
    pub max_deadline_seconds: Option<u64>,
    pub max_runs: Option<u32>,
    pub max_backoff_seconds: Option<u64>,
    pub max_check_timeout_seconds: Option<u64>,
    pub max_approval_wait_seconds: Option<u64>,
    pub per_run: GoalRunLimits,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct GoalsPatch {
    default_deadline_seconds: Option<u64>,
    max_deadline_seconds: Option<u64>,
    default_max_runs: Option<u32>,
    max_runs: Option<u32>,
    max_backoff_seconds: Option<u64>,
    check_timeout_seconds: Option<u64>,
    approval_wait_seconds: Option<u64>,
    per_run: GoalRunLimits,
}

impl GoalRunLimits {
    fn validate(self) -> Result<(), ConfigError> {
        if self.max_duration_ms == Some(0) {
            return Err(ConfigError::InvalidGoals {
                field: "per_run.max_duration_ms",
                reason: "must be positive",
            });
        }
        if self.max_model_turns == Some(0) {
            return Err(ConfigError::InvalidGoals {
                field: "per_run.max_model_turns",
                reason: "must be positive",
            });
        }
        if self.max_tool_calls == Some(0) {
            return Err(ConfigError::InvalidGoals {
                field: "per_run.max_tool_calls",
                reason: "must be positive",
            });
        }
        if self.max_total_tokens == Some(0) {
            return Err(ConfigError::InvalidGoals {
                field: "per_run.max_total_tokens",
                reason: "must be positive",
            });
        }
        if self.max_cost_usd_nanos == Some(0) {
            return Err(ConfigError::InvalidGoals {
                field: "per_run.max_cost_usd_nanos",
                reason: "must be positive",
            });
        }
        if self.max_input_tokens == Some(0) {
            return Err(ConfigError::InvalidGoals {
                field: "per_run.max_input_tokens",
                reason: "must be positive",
            });
        }
        if self.max_output_tokens == Some(0) {
            return Err(ConfigError::InvalidGoals {
                field: "per_run.max_output_tokens",
                reason: "must be positive",
            });
        }
        if self.max_tool_output_bytes == Some(0) {
            return Err(ConfigError::InvalidGoals {
                field: "per_run.max_tool_output_bytes",
                reason: "must be positive",
            });
        }
        if self.max_children == Some(0) {
            return Err(ConfigError::InvalidGoals {
                field: "per_run.max_children",
                reason: "must be positive",
            });
        }
        if self.max_concurrent_children == Some(0) {
            return Err(ConfigError::InvalidGoals {
                field: "per_run.max_concurrent_children",
                reason: "must be positive",
            });
        }
        if self.max_children.is_some_and(|value| value > 8) {
            return Err(ConfigError::InvalidGoals {
                field: "per_run.max_children",
                reason: "must be at most 8",
            });
        }
        if self.max_concurrent_children.is_some_and(|value| value > 3) {
            return Err(ConfigError::InvalidGoals {
                field: "per_run.max_concurrent_children",
                reason: "must be at most 3",
            });
        }
        Ok(())
    }

    fn apply(
        &mut self,
        patch: Self,
        source: &SourceIdentity,
        provenance: &mut BTreeMap<String, SourceIdentity>,
    ) {
        if let Some(value) = patch.max_duration_ms {
            self.max_duration_ms = Some(value);
            provenance.insert("per_run.max_duration_ms".to_owned(), source.clone());
        }
        if let Some(value) = patch.max_model_turns {
            self.max_model_turns = Some(value);
            provenance.insert("per_run.max_model_turns".to_owned(), source.clone());
        }
        if let Some(value) = patch.max_tool_calls {
            self.max_tool_calls = Some(value);
            provenance.insert("per_run.max_tool_calls".to_owned(), source.clone());
        }
        if let Some(value) = patch.max_total_tokens {
            self.max_total_tokens = Some(value);
            provenance.insert("per_run.max_total_tokens".to_owned(), source.clone());
        }
        if let Some(value) = patch.max_cost_usd_nanos {
            self.max_cost_usd_nanos = Some(value);
            provenance.insert("per_run.max_cost_usd_nanos".to_owned(), source.clone());
        }
        if let Some(value) = patch.max_input_tokens {
            self.max_input_tokens = Some(value);
            provenance.insert("per_run.max_input_tokens".to_owned(), source.clone());
        }
        if let Some(value) = patch.max_output_tokens {
            self.max_output_tokens = Some(value);
            provenance.insert("per_run.max_output_tokens".to_owned(), source.clone());
        }
        if let Some(value) = patch.max_tool_output_bytes {
            self.max_tool_output_bytes = Some(value);
            provenance.insert("per_run.max_tool_output_bytes".to_owned(), source.clone());
        }
        if let Some(value) = patch.max_children {
            self.max_children = Some(value);
            provenance.insert("per_run.max_children".to_owned(), source.clone());
        }
        if let Some(value) = patch.max_concurrent_children {
            self.max_concurrent_children = Some(value);
            provenance.insert("per_run.max_concurrent_children".to_owned(), source.clone());
        }
    }

    fn tighten(&mut self, ceiling: Self) {
        if let Some(cap) = ceiling.max_duration_ms {
            self.max_duration_ms = Some(self.max_duration_ms.map_or(cap, |value| value.min(cap)));
        }
        if let Some(cap) = ceiling.max_model_turns {
            self.max_model_turns = Some(self.max_model_turns.map_or(cap, |value| value.min(cap)));
        }
        if let Some(cap) = ceiling.max_tool_calls {
            self.max_tool_calls = Some(self.max_tool_calls.map_or(cap, |value| value.min(cap)));
        }
        if let Some(cap) = ceiling.max_total_tokens {
            self.max_total_tokens = Some(self.max_total_tokens.map_or(cap, |value| value.min(cap)));
        }
        if let Some(cap) = ceiling.max_cost_usd_nanos {
            self.max_cost_usd_nanos =
                Some(self.max_cost_usd_nanos.map_or(cap, |value| value.min(cap)));
        }
        if let Some(cap) = ceiling.max_input_tokens {
            self.max_input_tokens = Some(self.max_input_tokens.map_or(cap, |value| value.min(cap)));
        }
        if let Some(cap) = ceiling.max_output_tokens {
            self.max_output_tokens =
                Some(self.max_output_tokens.map_or(cap, |value| value.min(cap)));
        }
        if let Some(cap) = ceiling.max_tool_output_bytes {
            self.max_tool_output_bytes = Some(
                self.max_tool_output_bytes
                    .map_or(cap, |value| value.min(cap)),
            );
        }
        if let Some(cap) = ceiling.max_children {
            self.max_children = Some(self.max_children.map_or(cap, |value| value.min(cap)));
        }
        if let Some(cap) = ceiling.max_concurrent_children {
            self.max_concurrent_children = Some(
                self.max_concurrent_children
                    .map_or(cap, |value| value.min(cap)),
            );
        }
    }
}

impl GoalCeilings {
    pub(crate) fn validate(self) -> Result<(), ConfigError> {
        if self
            .max_deadline_seconds
            .is_some_and(|value| value == 0 || value > 604800)
        {
            return Err(ConfigError::InvalidGoals {
                field: "policy.goals.max_deadline_seconds",
                reason: "outside the supported positive range",
            });
        }
        if self
            .max_runs
            .is_some_and(|value| value == 0 || value > 2000)
        {
            return Err(ConfigError::InvalidGoals {
                field: "policy.goals.max_runs",
                reason: "outside the supported positive range",
            });
        }
        if self
            .max_backoff_seconds
            .is_some_and(|value| value == 0 || value > 86400)
        {
            return Err(ConfigError::InvalidGoals {
                field: "policy.goals.max_backoff_seconds",
                reason: "outside the supported positive range",
            });
        }
        if self
            .max_check_timeout_seconds
            .is_some_and(|value| value == 0 || value > 3600)
        {
            return Err(ConfigError::InvalidGoals {
                field: "policy.goals.max_check_timeout_seconds",
                reason: "outside the supported positive range",
            });
        }
        if self
            .max_approval_wait_seconds
            .is_some_and(|value| value == 0 || value > 86400)
        {
            return Err(ConfigError::InvalidGoals {
                field: "policy.goals.max_approval_wait_seconds",
                reason: "outside the supported positive range",
            });
        }
        self.per_run.validate()
    }

    pub(crate) fn compose(
        &mut self,
        patch: Self,
        source: &SourceIdentity,
        provenance: &mut BTreeMap<String, SourceIdentity>,
    ) {
        if let Some(value) = patch.max_deadline_seconds
            && self
                .max_deadline_seconds
                .is_none_or(|current| value < current)
        {
            self.max_deadline_seconds = Some(value);
            provenance.insert("policy.max_deadline_seconds".to_owned(), source.clone());
        }
        if let Some(value) = patch.max_runs
            && self.max_runs.is_none_or(|current| value < current)
        {
            self.max_runs = Some(value);
            provenance.insert("policy.max_runs".to_owned(), source.clone());
        }
        if let Some(value) = patch.max_backoff_seconds
            && self
                .max_backoff_seconds
                .is_none_or(|current| value < current)
        {
            self.max_backoff_seconds = Some(value);
            provenance.insert("policy.max_backoff_seconds".to_owned(), source.clone());
        }
        if let Some(value) = patch.max_check_timeout_seconds
            && self
                .max_check_timeout_seconds
                .is_none_or(|current| value < current)
        {
            self.max_check_timeout_seconds = Some(value);
            provenance.insert(
                "policy.max_check_timeout_seconds".to_owned(),
                source.clone(),
            );
        }
        if let Some(value) = patch.max_approval_wait_seconds
            && self
                .max_approval_wait_seconds
                .is_none_or(|current| value < current)
        {
            self.max_approval_wait_seconds = Some(value);
            provenance.insert(
                "policy.max_approval_wait_seconds".to_owned(),
                source.clone(),
            );
        }
        if let Some(value) = patch.per_run.max_duration_ms
            && self
                .per_run
                .max_duration_ms
                .is_none_or(|current| value < current)
        {
            self.per_run.max_duration_ms = Some(value);
            provenance.insert("policy.per_run.max_duration_ms".to_owned(), source.clone());
        }
        if let Some(value) = patch.per_run.max_model_turns
            && self
                .per_run
                .max_model_turns
                .is_none_or(|current| value < current)
        {
            self.per_run.max_model_turns = Some(value);
            provenance.insert("policy.per_run.max_model_turns".to_owned(), source.clone());
        }
        if let Some(value) = patch.per_run.max_tool_calls
            && self
                .per_run
                .max_tool_calls
                .is_none_or(|current| value < current)
        {
            self.per_run.max_tool_calls = Some(value);
            provenance.insert("policy.per_run.max_tool_calls".to_owned(), source.clone());
        }
        if let Some(value) = patch.per_run.max_total_tokens
            && self
                .per_run
                .max_total_tokens
                .is_none_or(|current| value < current)
        {
            self.per_run.max_total_tokens = Some(value);
            provenance.insert("policy.per_run.max_total_tokens".to_owned(), source.clone());
        }
        if let Some(value) = patch.per_run.max_cost_usd_nanos
            && self
                .per_run
                .max_cost_usd_nanos
                .is_none_or(|current| value < current)
        {
            self.per_run.max_cost_usd_nanos = Some(value);
            provenance.insert(
                "policy.per_run.max_cost_usd_nanos".to_owned(),
                source.clone(),
            );
        }
        if let Some(value) = patch.per_run.max_input_tokens
            && self
                .per_run
                .max_input_tokens
                .is_none_or(|current| value < current)
        {
            self.per_run.max_input_tokens = Some(value);
            provenance.insert("policy.per_run.max_input_tokens".to_owned(), source.clone());
        }
        if let Some(value) = patch.per_run.max_output_tokens
            && self
                .per_run
                .max_output_tokens
                .is_none_or(|current| value < current)
        {
            self.per_run.max_output_tokens = Some(value);
            provenance.insert(
                "policy.per_run.max_output_tokens".to_owned(),
                source.clone(),
            );
        }
        if let Some(value) = patch.per_run.max_tool_output_bytes
            && self
                .per_run
                .max_tool_output_bytes
                .is_none_or(|current| value < current)
        {
            self.per_run.max_tool_output_bytes = Some(value);
            provenance.insert(
                "policy.per_run.max_tool_output_bytes".to_owned(),
                source.clone(),
            );
        }
        if let Some(value) = patch.per_run.max_children
            && self
                .per_run
                .max_children
                .is_none_or(|current| value < current)
        {
            self.per_run.max_children = Some(value);
            provenance.insert("policy.per_run.max_children".to_owned(), source.clone());
        }
        if let Some(value) = patch.per_run.max_concurrent_children
            && self
                .per_run
                .max_concurrent_children
                .is_none_or(|current| value < current)
        {
            self.per_run.max_concurrent_children = Some(value);
            provenance.insert(
                "policy.per_run.max_concurrent_children".to_owned(),
                source.clone(),
            );
        }
    }
}

impl GoalsConfig {
    pub(crate) fn apply(
        &mut self,
        patch: &GoalsPatch,
        source: &SourceIdentity,
        provenance: &mut BTreeMap<String, SourceIdentity>,
    ) {
        if let Some(value) = patch.default_deadline_seconds {
            self.default_deadline_seconds = value;
            provenance.insert("default_deadline_seconds".to_owned(), source.clone());
        }
        if let Some(value) = patch.max_deadline_seconds {
            self.max_deadline_seconds = value;
            provenance.insert("max_deadline_seconds".to_owned(), source.clone());
        }
        if let Some(value) = patch.default_max_runs {
            self.default_max_runs = value;
            provenance.insert("default_max_runs".to_owned(), source.clone());
        }
        if let Some(value) = patch.max_runs {
            self.max_runs = value;
            provenance.insert("max_runs".to_owned(), source.clone());
        }
        if let Some(value) = patch.max_backoff_seconds {
            self.max_backoff_seconds = value;
            provenance.insert("max_backoff_seconds".to_owned(), source.clone());
        }
        if let Some(value) = patch.check_timeout_seconds {
            self.check_timeout_seconds = value;
            provenance.insert("check_timeout_seconds".to_owned(), source.clone());
        }
        if let Some(value) = patch.approval_wait_seconds {
            self.approval_wait_seconds = value;
            provenance.insert("approval_wait_seconds".to_owned(), source.clone());
        }
        self.per_run.apply(patch.per_run, source, provenance);
    }

    pub(crate) fn finish(mut self, ceiling: GoalCeilings) -> Result<Self, ConfigError> {
        if self.default_deadline_seconds == 0 {
            return Err(ConfigError::InvalidGoals {
                field: "default_deadline_seconds",
                reason: "must be positive",
            });
        }
        if self.max_deadline_seconds == 0 {
            return Err(ConfigError::InvalidGoals {
                field: "max_deadline_seconds",
                reason: "must be positive",
            });
        }
        if self.default_max_runs == 0 {
            return Err(ConfigError::InvalidGoals {
                field: "default_max_runs",
                reason: "must be positive",
            });
        }
        if self.max_runs == 0 {
            return Err(ConfigError::InvalidGoals {
                field: "max_runs",
                reason: "must be positive",
            });
        }
        if self.max_backoff_seconds == 0 {
            return Err(ConfigError::InvalidGoals {
                field: "max_backoff_seconds",
                reason: "must be positive",
            });
        }
        if self.check_timeout_seconds == 0 {
            return Err(ConfigError::InvalidGoals {
                field: "check_timeout_seconds",
                reason: "must be positive",
            });
        }
        if self.approval_wait_seconds == 0 {
            return Err(ConfigError::InvalidGoals {
                field: "approval_wait_seconds",
                reason: "must be positive",
            });
        }
        if self.default_deadline_seconds > 604800 {
            return Err(ConfigError::InvalidGoals {
                field: "default_deadline_seconds",
                reason: "exceeds the supported maximum",
            });
        }
        if self.max_deadline_seconds > 604800 {
            return Err(ConfigError::InvalidGoals {
                field: "max_deadline_seconds",
                reason: "exceeds the supported maximum",
            });
        }
        if self.default_max_runs > 2000 {
            return Err(ConfigError::InvalidGoals {
                field: "default_max_runs",
                reason: "exceeds the supported maximum",
            });
        }
        if self.max_runs > 2000 {
            return Err(ConfigError::InvalidGoals {
                field: "max_runs",
                reason: "exceeds the supported maximum",
            });
        }
        if self.max_backoff_seconds > 86400 {
            return Err(ConfigError::InvalidGoals {
                field: "max_backoff_seconds",
                reason: "exceeds the supported maximum",
            });
        }
        if self.check_timeout_seconds > 3600 {
            return Err(ConfigError::InvalidGoals {
                field: "check_timeout_seconds",
                reason: "exceeds the supported maximum",
            });
        }
        if self.approval_wait_seconds > 86400 {
            return Err(ConfigError::InvalidGoals {
                field: "approval_wait_seconds",
                reason: "exceeds the supported maximum",
            });
        }
        self.per_run.validate()?;
        if let Some(cap) = ceiling.max_deadline_seconds {
            self.max_deadline_seconds = self.max_deadline_seconds.min(cap);
        }
        if let Some(cap) = ceiling.max_runs {
            self.max_runs = self.max_runs.min(cap);
        }
        if let Some(cap) = ceiling.max_backoff_seconds {
            self.max_backoff_seconds = self.max_backoff_seconds.min(cap);
        }
        if let Some(cap) = ceiling.max_check_timeout_seconds {
            self.check_timeout_seconds = self.check_timeout_seconds.min(cap);
        }
        if let Some(cap) = ceiling.max_approval_wait_seconds {
            self.approval_wait_seconds = self.approval_wait_seconds.min(cap);
        }
        self.default_deadline_seconds =
            self.default_deadline_seconds.min(self.max_deadline_seconds);
        self.default_max_runs = self.default_max_runs.min(self.max_runs);
        self.per_run.tighten(ceiling.per_run);
        Ok(self)
    }
}
