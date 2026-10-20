//! Session goals: durable objectives the server pursues across runs until a
//! completion check passes or a budget runs out (see `docs/guide/tui.md`
//! § Session goals). The desktop reads goal defaults from the workspace's
//! configuration exactly as the CLI does, builds budgets from the same
//! presets as the TUI's goal form, and sends `SetGoal` / `GoalControl`
//! against the goal revision the user saw, so a changed goal is never
//! silently overwritten.

use std::path::Path;

use qq_config::{ConfigLoader, GoalRunLimits, GoalsConfig, LoadRequest};
use qq_protocol::{GoalBudget, GoalCheck, GoalCheckScope, GoalControl, RunLimits};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use ts_rs::TS;

/// Objective length the TUI accepts.
pub const MAX_OBJECTIVE_BYTES: usize = 2048;
/// Check command length the TUI accepts.
pub const MAX_CHECK_BYTES: usize = 1024;
/// The focused preset's caps.
const FOCUSED_DURATION_MS: u64 = 3_600_000;
const FOCUSED_RUNS: u32 = 20;

/// The workspace's effective goal defaults and ceilings, for the dialog.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[cfg_attr(test, derive(TS))]
pub struct GoalDefaultsDto {
    #[cfg_attr(test, ts(type = "number"))]
    pub duration_ms: u64,
    #[cfg_attr(test, ts(type = "number"))]
    pub max_duration_ms: u64,
    pub default_runs: u32,
    pub max_runs: u32,
    #[cfg_attr(test, ts(type = "number"))]
    pub check_timeout_ms: u64,
    #[serde(skip)]
    #[cfg_attr(test, ts(skip))]
    pub per_run: RunLimits,
}

impl GoalDefaultsDto {
    pub fn from_config(goals: &GoalsConfig) -> Self {
        Self {
            duration_ms: goals.default_deadline_seconds.saturating_mul(1000),
            max_duration_ms: goals.max_deadline_seconds.saturating_mul(1000),
            default_runs: goals.default_max_runs,
            max_runs: goals.max_runs,
            check_timeout_ms: goals.check_timeout_seconds.saturating_mul(1000),
            per_run: run_limits(goals.per_run),
        }
    }
}

fn run_limits(limits: GoalRunLimits) -> RunLimits {
    RunLimits {
        max_duration_ms: limits.max_duration_ms,
        max_model_turns: limits.max_model_turns,
        max_tool_calls: limits.max_tool_calls,
        max_total_tokens: limits.max_total_tokens,
        max_cost_usd_nanos: limits.max_cost_usd_nanos,
        max_input_tokens: limits.max_input_tokens,
        max_output_tokens: limits.max_output_tokens,
        max_tool_output_bytes: limits.max_tool_output_bytes,
        max_children: limits.max_children,
        max_concurrent_children: limits.max_concurrent_children,
    }
}

/// Blocking: the workspace's goal configuration, as `qq` would load it.
pub fn defaults(workspace: &Path) -> Result<GoalDefaultsDto, String> {
    let loader = ConfigLoader::system().map_err(|error| error.to_string())?;
    let request =
        LoadRequest::from_process_env(workspace, None).map_err(|error| error.to_string())?;
    let snapshot = loader
        .load_for_client(&request)
        .map_err(|error| error.to_string())?;
    Ok(GoalDefaultsDto::from_config(snapshot.goals()))
}

/// How long and how many runs a goal may take, chosen in the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetPreset {
    /// Editing: keep the goal's current deadline and limits.
    Keep,
    /// The configured default deadline and run allowance.
    Defaults,
    /// Up to one hour and 20 runs.
    Focused,
    /// The workspace maximum.
    Extended,
}

/// The budget for `preset`, starting `now_ms`. `Keep` needs `current`; other
/// presets keep the current per-run and token/cost limits when editing and
/// otherwise start from the configured per-run limits. Every value is clamped
/// to the workspace ceilings, as the TUI's form does.
pub fn budget(
    preset: BudgetPreset,
    defaults: &GoalDefaultsDto,
    current: Option<&GoalBudget>,
    now_ms: u64,
) -> Result<GoalBudget, String> {
    let mut budget = current.cloned().unwrap_or(GoalBudget {
        max_goal_runs: defaults.default_runs,
        deadline_ms: 0,
        max_total_tokens: None,
        max_cost_usd_nanos: None,
        per_run: defaults.per_run,
    });
    let (duration, runs) = match preset {
        BudgetPreset::Keep => {
            return current
                .cloned()
                .ok_or_else(|| "there is no current budget to keep".to_owned());
        }
        BudgetPreset::Defaults => (defaults.duration_ms, defaults.default_runs),
        BudgetPreset::Focused => (
            defaults.duration_ms.min(FOCUSED_DURATION_MS),
            defaults.default_runs.min(FOCUSED_RUNS),
        ),
        BudgetPreset::Extended => (defaults.max_duration_ms, defaults.max_runs),
    };
    budget.max_goal_runs = runs.min(defaults.max_runs).max(1);
    budget.deadline_ms = now_ms.saturating_add(duration.min(defaults.max_duration_ms));
    Ok(budget)
}

/// The optional completion check, trimmed and bounded.
pub fn check(command: Option<&str>, timeout_ms: u64) -> Result<Option<GoalCheck>, String> {
    let Some(command) = command.map(str::trim).filter(|command| !command.is_empty()) else {
        return Ok(None);
    };
    if command.len() > MAX_CHECK_BYTES {
        return Err(format!(
            "check commands are limited to {MAX_CHECK_BYTES} bytes"
        ));
    }
    Ok(Some(GoalCheck {
        command: command.to_owned(),
        timeout_ms,
    }))
}

/// The objective, trimmed and bounded.
pub fn objective(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("describe the goal first".to_owned());
    }
    if text.len() > MAX_OBJECTIVE_BYTES {
        return Err(format!(
            "goal objectives are limited to {MAX_OBJECTIVE_BYTES} bytes"
        ));
    }
    Ok(text.to_owned())
}

/// What the UI asks of a goal; mapped to the protocol's `GoalControl`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GoalAction {
    Pause,
    Resume,
    Clear,
    /// Consent for exactly the displayed check command.
    ApproveCheck {
        scope: CheckScope,
        command: String,
    },
    DenyCheck,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckScope {
    Once,
    ForGoal,
}

impl From<GoalAction> for GoalControl {
    fn from(action: GoalAction) -> Self {
        match action {
            GoalAction::Pause => Self::Pause,
            GoalAction::Resume => Self::Resume,
            GoalAction::Clear => Self::Clear,
            GoalAction::ApproveCheck { scope, command } => Self::ApproveCheck {
                scope: match scope {
                    CheckScope::Once => GoalCheckScope::Once,
                    CheckScope::ForGoal => GoalCheckScope::ForGoal,
                },
                command,
            },
            GoalAction::DenyCheck => Self::DenyCheck,
        }
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> GoalDefaultsDto {
        GoalDefaultsDto::from_config(&GoalsConfig::default())
    }

    #[test]
    fn presets_follow_the_tui_form_and_workspace_ceilings() {
        let defaults = defaults();
        let now = 1_000;
        let configured = budget(BudgetPreset::Defaults, &defaults, None, now).unwrap();
        assert_eq!(configured.max_goal_runs, 200);
        assert_eq!(configured.deadline_ms, now + 86_400_000);
        let focused = budget(BudgetPreset::Focused, &defaults, None, now).unwrap();
        assert_eq!(
            (focused.max_goal_runs, focused.deadline_ms),
            (20, now + 3_600_000)
        );
        let extended = budget(BudgetPreset::Extended, &defaults, None, now).unwrap();
        assert_eq!(
            (extended.max_goal_runs, extended.deadline_ms),
            (2000, now + 604_800_000)
        );
        // New goals take the configured per-run limits.
        assert_eq!(configured.per_run, defaults.per_run);
    }

    #[test]
    fn editing_keeps_or_rebases_the_current_budget() {
        let defaults = defaults();
        let current = GoalBudget {
            max_goal_runs: 7,
            deadline_ms: 5,
            max_total_tokens: Some(1_000),
            max_cost_usd_nanos: Some(2_000),
            per_run: RunLimits {
                max_model_turns: Some(9),
                ..RunLimits::default()
            },
        };
        assert_eq!(
            budget(BudgetPreset::Keep, &defaults, Some(&current), 99).unwrap(),
            current
        );
        let rebased = budget(BudgetPreset::Focused, &defaults, Some(&current), 100).unwrap();
        assert_eq!(
            (rebased.max_goal_runs, rebased.deadline_ms),
            (20, 3_600_100)
        );
        // Other limits survive a preset change.
        assert_eq!(rebased.max_total_tokens, Some(1_000));
        assert_eq!(rebased.per_run.max_model_turns, Some(9));
        assert!(budget(BudgetPreset::Keep, &defaults, None, 0).is_err());
    }

    #[test]
    fn small_ceilings_clamp_every_preset() {
        let config = GoalsConfig {
            default_deadline_seconds: 600,
            max_deadline_seconds: 900,
            default_max_runs: 5,
            max_runs: 8,
            ..GoalsConfig::default()
        };
        let defaults = GoalDefaultsDto::from_config(&config);
        let focused = budget(BudgetPreset::Focused, &defaults, None, 0).unwrap();
        assert_eq!((focused.max_goal_runs, focused.deadline_ms), (5, 600_000));
        let extended = budget(BudgetPreset::Extended, &defaults, None, 0).unwrap();
        assert_eq!((extended.max_goal_runs, extended.deadline_ms), (8, 900_000));
    }

    #[test]
    fn objectives_and_checks_are_trimmed_and_bounded() {
        assert_eq!(objective("  ship it ").unwrap(), "ship it");
        assert!(objective("   ").is_err());
        assert!(objective(&"x".repeat(MAX_OBJECTIVE_BYTES + 1)).is_err());
        assert_eq!(check(Some("  "), 5).unwrap(), None);
        assert_eq!(check(None, 5).unwrap(), None);
        assert_eq!(
            check(Some(" cargo test "), 5).unwrap(),
            Some(GoalCheck {
                command: "cargo test".to_owned(),
                timeout_ms: 5
            })
        );
        assert!(check(Some(&"x".repeat(MAX_CHECK_BYTES + 1)), 5).is_err());
    }

    #[test]
    fn ui_actions_map_to_protocol_controls() {
        let action: GoalAction = serde_json::from_str(
            r#"{"type":"approve_check","scope":"for_goal","command":"cargo test"}"#,
        )
        .unwrap();
        assert_eq!(
            GoalControl::from(action),
            GoalControl::ApproveCheck {
                scope: GoalCheckScope::ForGoal,
                command: "cargo test".to_owned()
            }
        );
        let pause: GoalAction = serde_json::from_str(r#"{"type":"pause"}"#).unwrap();
        assert_eq!(GoalControl::from(pause), GoalControl::Pause);
    }
}
