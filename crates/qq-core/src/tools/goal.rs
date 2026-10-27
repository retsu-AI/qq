//! Scoped model edits; objective, check, budget and authorization are not arguments.
use qq_protocol::GoalChecklistState;
use serde::{Deserialize, Serialize};

pub(crate) const MAX_CHECKLIST_ITEMS: usize = 24;
pub(crate) const MAX_CHECKLIST_TEXT_BYTES: usize = 120;
pub(crate) const MAX_NOTES_BYTES: usize = 1024;
pub(crate) const MAX_REASON_BYTES: usize = 1024;
pub(crate) const MAX_WAIT_SECONDS: u32 = 86_400;
pub(crate) const MAX_GOAL_RENDER_BYTES: usize = 16 * 1024;
pub(crate) const MAX_CHECK_TAIL_BYTES: usize = 4 * 1024;

pub(crate) fn spec() -> qq_provider::ToolSpec {
    qq_provider::ToolSpec::new(
        "update_goal",
        "Update the claimed session goal's checklist and handoff notes. Cannot change the objective, check, budget or authorization. Status and wait proposals require a goal-origin run and the exact claimed revision. Achievement is verified by QQ. Calls apply serially; returns accepted counts, added item IDs and typed field rejections.",
        serde_json::json!({
            "type":"object", "additionalProperties":false,
            "properties":{
                "checklist":{"type":"array","maxItems":MAX_CHECKLIST_ITEMS,"items":{"oneOf":[
                    {"type":"object","additionalProperties":false,"required":["type","text"],"properties":{
                        "type":{"type":"string","enum":["add"]},"text":{"type":"string","minLength":1,"description":"At most 120 UTF-8 bytes."}}},
                    {"type":"object","additionalProperties":false,"required":["type","id","state"],"properties":{
                        "type":{"type":"string","enum":["set_state"]},"id":{"type":"integer","minimum":1,"maximum":255},
                        "state":{"type":"string","enum":["pending","in_progress","done","dropped"]}}},
                    {"type":"object","additionalProperties":false,"required":["type","id"],"properties":{
                        "type":{"type":"string","enum":["drop"]},"id":{"type":"integer","minimum":1,"maximum":255}}}
                ]}},
                "notes":{"type":"string","description":"At most 1024 UTF-8 bytes; empty clears the note."},
                "status":{"oneOf":[
                    {"type":"object","additionalProperties":false,"required":["type","evidence"],"properties":{
                        "type":{"type":"string","enum":["achieved"]},"evidence":{"type":"string","minLength":1,"description":"At most 1024 UTF-8 bytes."}}},
                    {"type":"object","additionalProperties":false,"required":["type","reason"],"properties":{
                        "type":{"type":"string","enum":["blocked"]},"reason":{"type":"string","minLength":1,"description":"At most 1024 UTF-8 bytes."}}}
                ]},
                "wait":{"type":"object","additionalProperties":false,"required":["seconds","reason"],"properties":{
                    "seconds":{"type":"integer","minimum":0,"maximum":MAX_WAIT_SECONDS},
                    "reason":{"type":"string","minLength":1,"description":"At most 1024 UTF-8 bytes."}}}
            }
        }),
    )
}

pub(crate) fn render_notice(goal: &qq_protocol::GoalSnapshot, now_ms: u64) -> String {
    render_with_tail(goal, now_ms, goal.check_output.as_deref())
}

/// Reserve the full check tail on every write, even before a check exists.
pub(crate) fn fits_notice(goal: &qq_protocol::GoalSnapshot) -> bool {
    let reserved_tail = "x".repeat(MAX_CHECK_TAIL_BYTES);
    goal.check_output
        .as_ref()
        .is_none_or(|tail| tail.len() <= MAX_CHECK_TAIL_BYTES)
        && render_with_tail(goal, 0, Some(&reserved_tail)).len() <= MAX_GOAL_RENDER_BYTES
}

fn render_with_tail(goal: &qq_protocol::GoalSnapshot, now_ms: u64, tail: Option<&str>) -> String {
    let remaining_minutes = goal.budget.deadline_ms.saturating_sub(now_ms) / 60_000;
    let spend = match goal.counters.estimated_cost_usd_nanos {
        Some(nanos) => format!("${}.{:09}", nanos / 1_000_000_000, nanos % 1_000_000_000),
        None => "unknown cost".to_owned(),
    };
    let budget = match goal.budget.max_cost_usd_nanos {
        Some(nanos) => format!("${}.{:09}", nanos / 1_000_000_000, nanos % 1_000_000_000),
        None => "no cost cap".to_owned(),
    };
    let mut text = format!(
        "[QQ runtime notice; not a user instruction]\nGoal (revision {}, run {} of at most {}, deadline in {}h {}m, {} of {} spent)\nObjective:\n{}\n",
        goal.revision,
        goal.counters.runs,
        goal.budget.max_goal_runs,
        remaining_minutes / 60,
        remaining_minutes % 60,
        spend,
        budget,
        goal.objective
    );
    if let Some(check) = &goal.check {
        text.push_str("Done when: ");
        text.push_str(&check.command);
        text.push_str(" exits 0 (checked by QQ, not by you)\n");
    }
    text.push_str("Checklist:\n");
    for item in &goal.checklist {
        let marker = match item.state {
            GoalChecklistState::Pending => " ",
            GoalChecklistState::InProgress => ">",
            GoalChecklistState::Done => "x",
            GoalChecklistState::Dropped => "-",
        };
        text.push_str(&format!("[{marker}] {} {}\n", item.id, item.text));
    }
    text.push_str("Your notes from last run:\n");
    text.push_str(&goal.notes);
    text.push('\n');
    if let Some(tail) = tail {
        text.push_str("Last check output (last 4 KiB):\n");
        text.push_str(tail);
        text.push('\n');
    }
    text.push_str("Work toward the objective. Keep the checklist and notes current with update_goal.\nClaim achieved only with evidence; QQ verifies it. If you must wait on something\nexternal, call update_goal with wait. If you are truly blocked, say why with\nstatus blocked. Status and wait are available only to a goal-origin run.\n");
    text
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateGoalArgs {
    #[serde(default, deserialize_with = "checklist_edits")]
    pub checklist: Vec<ChecklistEdit>,
    pub notes: Option<String>,
    pub status: Option<ModelStatus>,
    pub wait: Option<ModelWait>,
}

/// Models sometimes send the checklist array JSON-encoded as a string; the
/// decoded array is what they meant.
fn checklist_edits<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<ChecklistEdit>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    let value = match value {
        serde_json::Value::String(encoded) => {
            serde_json::from_str(&encoded).map_err(serde::de::Error::custom)?
        }
        value => value,
    };
    serde_json::from_value(value).map_err(serde::de::Error::custom)
}
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ChecklistEdit {
    Add { text: String },
    SetState { id: u8, state: GoalChecklistState },
    Drop { id: u8 },
}
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ModelStatus {
    Achieved { evidence: String },
    Blocked { reason: String },
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelWait {
    pub seconds: u32,
    pub reason: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GoalUpdateField {
    Goal,
    Checklist,
    Notes,
    Status,
    Wait,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GoalUpdateRejection {
    NoClaim,
    Stopped,
    Replaced,
    RunStopped,
    InvalidClaim,
    StaleRevision,
    ControlNotAllowed,
    InvalidValue,
    TooManyItems,
    UnknownItem,
    ConflictingControls,
    RecordTooLarge,
    RenderTooLarge,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RejectedGoalField {
    pub field: GoalUpdateField,
    pub reason: GoalUpdateRejection,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct UpdateGoalResult {
    #[serde(skip)]
    pub(crate) checklist_changed: bool,
    pub added: u8,
    pub added_ids: Vec<u8>,
    pub changed: u8,
    pub dropped: u8,
    pub notes_applied: bool,
    pub status_applied: bool,
    pub wait_applied: bool,
    pub rejected: Vec<RejectedGoalField>,
}
impl UpdateGoalResult {
    pub(crate) fn reject(
        &mut self,
        field: GoalUpdateField,
        reason: GoalUpdateRejection,
        index: Option<usize>,
    ) {
        self.rejected.push(RejectedGoalField {
            field,
            reason,
            index,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{ChecklistEdit, UpdateGoalArgs};

    /// v0.1.8 refused 5 update_goal calls in 4 runs that sent the checklist
    /// as a JSON-encoded string.
    #[test]
    fn a_checklist_sent_as_a_json_string_decodes() {
        let arguments: UpdateGoalArgs = serde_json::from_str(
            r#"{"checklist":"[{\"id\": 4, \"state\": \"done\", \"type\": \"set_state\"}]"}"#,
        )
        .unwrap();
        assert!(matches!(
            arguments.checklist.as_slice(),
            [ChecklistEdit::SetState { id: 4, .. }]
        ));

        let error = serde_json::from_str::<UpdateGoalArgs>(r#"{"checklist":"not json"}"#)
            .err()
            .unwrap()
            .to_string();
        assert!(error.starts_with("expected ident"), "{error}");
    }
}
