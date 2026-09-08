use std::time::Duration;

use qq_protocol::{BudgetExhaustion, BudgetLimitKind, ModelPricing, RunLimits, TokenUsage};
use tokio::time::Instant;

/// The final tool-free turn a run is granted once its work budget is spent.
pub(crate) const BUDGET_FINAL_RESPONSE_NOTICE: &str = "The run's budget is exhausted, so no \
tools are available for this reply. Report concisely what was accomplished, what remains, and \
the exact next step. This is the final response of the run.";

/// One inherited allowance and the parent's absolute deadline. Preparation
/// and queueing may reduce the allowance's clock, never restart it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ChildBudget {
    pub(crate) limits: RunLimits,
    pub(crate) deadline: Option<Instant>,
}

impl ChildBudget {
    pub(crate) fn limits_at(self, now: Instant) -> Result<RunLimits, BudgetLimitKind> {
        let mut limits = self.limits;
        if let Some(deadline) = self.deadline {
            let left = deadline.saturating_duration_since(now);
            if left.is_zero() {
                return Err(BudgetLimitKind::Duration);
            }
            limits.max_duration_ms =
                Some(u64::try_from(left.as_millis()).unwrap_or(u64::MAX).max(1));
        }
        Ok(limits)
    }
}

/// Core-owned accounting of one run against its caller-imposed `RunLimits`.
///
/// The meter charges turns, tool calls, tokens, and cost as the runtime
/// observes them and answers one question before every model turn: may the
/// run keep working, must it spend its reserved final response, or must it
/// settle now. Charges saturate; an exceeded bound is reported exactly once.
pub(crate) struct BudgetMeter {
    limits: RunLimits,
    pricing: Option<ModelPricing>,
    started: Instant,
    turns: u16,
    tool_calls: u32,
    /// Total input plus output tokens across every turn, `None` once a turn
    /// omitted usage while a token or cost bound was imposed.
    tokens: Option<u64>,
    /// Fresh plus cached input tokens across every turn; `None` like `tokens`.
    input_tokens: Option<u64>,
    /// Output tokens across every turn; `None` like `tokens`.
    output_tokens: Option<u64>,
    /// Bytes of tool results fed back to the model after truncation.
    tool_output_bytes: u64,
    /// Estimated spend, `None` once unmeasurable under a cost bound.
    cost_usd_nanos: Option<u64>,
    /// The limit that spent the work budget once the reserved final response
    /// has been requested; the next check settles with it instead of
    /// requesting another.
    final_response_requested: Option<BudgetLimitKind>,
}

/// What the meter permits before the next provider turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BudgetDecision {
    /// Work may continue with tools.
    Continue,
    /// The work budget is spent but one tool-free response can be afforded.
    FinalResponse(BudgetLimitKind),
    /// Nothing more can be afforded, or the final response was already spent.
    Exhausted(BudgetExhaustion),
}

impl BudgetMeter {
    pub(crate) fn new(limits: RunLimits, pricing: Option<ModelPricing>, started: Instant) -> Self {
        Self {
            limits,
            pricing,
            started,
            turns: 0,
            tool_calls: 0,
            tokens: Some(0),
            input_tokens: Some(0),
            output_tokens: Some(0),
            tool_output_bytes: 0,
            cost_usd_nanos: Some(0),
            final_response_requested: None,
        }
    }

    /// The instant the wall-clock budget elapses, when one was imposed.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.limits
            .max_duration_ms
            .map(|ms| self.started + Duration::from_millis(ms))
    }

    /// Charges one completed provider turn. A missing usage report makes
    /// tokens and cost unknown for the rest of the run.
    pub(crate) fn charge_turn(&mut self, usage: Option<TokenUsage>) {
        self.turns = self.turns.saturating_add(1);
        match usage {
            Some(usage) => {
                let input = usage
                    .input_tokens
                    .saturating_add(usage.cache_read_input_tokens)
                    .saturating_add(usage.cache_write_input_tokens);
                self.tokens = self.tokens.map(|total| {
                    total
                        .saturating_add(input)
                        .saturating_add(usage.output_tokens)
                });
                self.input_tokens = self.input_tokens.map(|total| total.saturating_add(input));
                self.output_tokens = self
                    .output_tokens
                    .map(|total| total.saturating_add(usage.output_tokens));
                self.cost_usd_nanos = self.cost_usd_nanos.and_then(|total| {
                    let pricing = self.pricing.as_ref()?;
                    let cost = crate::sessions::run_cost(usage, pricing)?;
                    total.checked_add(cost)
                });
            }
            None => {
                self.tokens = None;
                self.input_tokens = None;
                self.output_tokens = None;
                self.cost_usd_nanos = None;
            }
        }
    }

    /// Charges the bytes of one turn's tool results as the model will see them.
    pub(crate) fn charge_tool_output(&mut self, bytes: usize) {
        self.tool_output_bytes = self
            .tool_output_bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
    }

    pub(crate) fn charge_tool_calls(&mut self, count: usize) {
        self.tool_calls = self
            .tool_calls
            .saturating_add(u32::try_from(count).unwrap_or(u32::MAX));
    }

    /// Charges sub-agent spend that the parent run is accountable for. Tokens
    /// roll up under the parent's token bounds exactly like its own turns;
    /// an unknown child cost or usage makes the parent's aggregate unknown.
    pub(crate) fn charge_child(&mut self, usage: Option<TokenUsage>, cost_usd_nanos: Option<u64>) {
        self.cost_usd_nanos = match (self.cost_usd_nanos, cost_usd_nanos) {
            (Some(total), Some(cost)) => total.checked_add(cost),
            _ => None,
        };
        match usage {
            Some(usage) => {
                let input = usage
                    .input_tokens
                    .saturating_add(usage.cache_read_input_tokens)
                    .saturating_add(usage.cache_write_input_tokens);
                self.tokens = self.tokens.map(|total| {
                    total
                        .saturating_add(input)
                        .saturating_add(usage.output_tokens)
                });
                self.input_tokens = self.input_tokens.map(|total| total.saturating_add(input));
                self.output_tokens = self
                    .output_tokens
                    .map(|total| total.saturating_add(usage.output_tokens));
            }
            None => {
                self.tokens = None;
                self.input_tokens = None;
                self.output_tokens = None;
            }
        }
    }

    pub(crate) fn child_budget(&self, now: Instant) -> Result<ChildBudget, BudgetLimitKind> {
        self.remaining(now).map(|limits| ChildBudget {
            limits,
            deadline: self.deadline(),
        })
    }

    /// The budget a child spawned now may be given: every imposed cost,
    /// wall-clock, and token bound reduced by what this run has already
    /// spent. Turn, tool-call, byte, and child counts are per-run and are not
    /// inherited. A bound whose remainder is zero, or whose spend is unknown,
    /// refuses the child by naming that family: a parent that cannot afford
    /// more work must not hand a fresh cap to a child.
    pub(crate) fn remaining(&self, now: Instant) -> Result<RunLimits, BudgetLimitKind> {
        let limits = &self.limits;
        let max_duration_ms = match self.deadline() {
            Some(deadline) => {
                let left = deadline.saturating_duration_since(now);
                if left.is_zero() {
                    return Err(BudgetLimitKind::Duration);
                }
                Some(u64::try_from(left.as_millis()).unwrap_or(u64::MAX).max(1))
            }
            None => None,
        };
        let max_cost_usd_nanos = match limits.max_cost_usd_nanos {
            Some(limit) => match self.cost_usd_nanos {
                Some(spent) if spent < limit => Some(limit - spent),
                Some(_) => return Err(BudgetLimitKind::Cost),
                None => return Err(BudgetLimitKind::CostUnknown),
            },
            None => None,
        };
        let remaining_tokens =
            |limit: Option<u64>, spent: Option<u64>, kind: BudgetLimitKind| match limit {
                Some(limit) => match spent {
                    Some(spent) if spent < limit => Ok(Some(limit - spent)),
                    Some(_) => Err(kind),
                    None => Err(BudgetLimitKind::TokensUnknown),
                },
                None => Ok(None),
            };
        Ok(RunLimits {
            max_duration_ms,
            max_model_turns: None,
            max_tool_calls: None,
            max_total_tokens: remaining_tokens(
                limits.max_total_tokens,
                self.tokens,
                BudgetLimitKind::TotalTokens,
            )?,
            max_cost_usd_nanos,
            max_input_tokens: remaining_tokens(
                limits.max_input_tokens,
                self.input_tokens,
                BudgetLimitKind::InputTokens,
            )?,
            max_output_tokens: remaining_tokens(
                limits.max_output_tokens,
                self.output_tokens,
                BudgetLimitKind::OutputTokens,
            )?,
            max_tool_output_bytes: None,
            max_children: None,
            max_concurrent_children: None,
        })
    }

    /// Whether a limit has already been exceeded by observed work, checked
    /// after each turn commits. `None` means every imposed bound still holds.
    pub(crate) fn exceeded(&self, now: Instant) -> Option<BudgetLimitKind> {
        let limits = &self.limits;
        if self.deadline().is_some_and(|deadline| now >= deadline) {
            return Some(BudgetLimitKind::Duration);
        }
        if limits.max_cost_usd_nanos.is_some() && self.cost_usd_nanos.is_none() {
            return Some(BudgetLimitKind::CostUnknown);
        }
        if let (Some(limit), Some(cost)) = (limits.max_cost_usd_nanos, self.cost_usd_nanos)
            && cost > limit
        {
            return Some(BudgetLimitKind::Cost);
        }
        // Token accounting lost under any token bound fails closed with the
        // explicit "unknown" kind: the caller must not believe a bound held
        // when the provider stopped reporting usage.
        let token_bound = limits.max_total_tokens.is_some()
            || limits.max_input_tokens.is_some()
            || limits.max_output_tokens.is_some();
        if token_bound && self.tokens.is_none() {
            return Some(BudgetLimitKind::TokensUnknown);
        }
        if let (Some(limit), Some(tokens)) = (limits.max_total_tokens, self.tokens)
            && tokens > limit
        {
            return Some(BudgetLimitKind::TotalTokens);
        }
        if let (Some(limit), Some(tokens)) = (limits.max_input_tokens, self.input_tokens)
            && tokens > limit
        {
            return Some(BudgetLimitKind::InputTokens);
        }
        if let (Some(limit), Some(tokens)) = (limits.max_output_tokens, self.output_tokens)
            && tokens > limit
        {
            return Some(BudgetLimitKind::OutputTokens);
        }
        if limits
            .max_tool_output_bytes
            .is_some_and(|limit| self.tool_output_bytes > limit)
        {
            return Some(BudgetLimitKind::ToolOutputBytes);
        }
        if limits
            .max_tool_calls
            .is_some_and(|limit| self.tool_calls > limit)
        {
            return Some(BudgetLimitKind::ToolCalls);
        }
        if limits
            .max_model_turns
            .is_some_and(|limit| self.turns > limit)
        {
            return Some(BudgetLimitKind::ModelTurns);
        }
        None
    }

    /// Decides the next turn. The work budget is spent when the next ordinary
    /// turn could not be completed within a bound (turn or tool-call caps
    /// reserve one turn for the final response; the other bounds trip once
    /// observed work crosses them).
    pub(crate) fn before_turn(&mut self, now: Instant, next_turn_tools: usize) -> BudgetDecision {
        let limits = &self.limits;
        if let Some(spent) = self.final_response_requested {
            // The reserve has been used; whatever tripped since is secondary.
            let kind = self.exceeded(now).unwrap_or(spent);
            return BudgetDecision::Exhausted(self.exhaustion(kind, true, now));
        }
        if let Some(kind) = self.exceeded(now) {
            return self.settle(kind, now);
        }
        // Turn and tool caps reserve their final response from within the
        // cap: the last permitted turn is the tool-free status reply, so the
        // provider is never asked for more turns than the caller allowed.
        let turns_spent = limits
            .max_model_turns
            .is_some_and(|limit| self.turns.saturating_add(1) >= limit);
        let tools_spent = limits.max_tool_calls.is_some_and(|limit| {
            self.tool_calls
                .saturating_add(u32::try_from(next_turn_tools).unwrap_or(u32::MAX))
                > limit
        });
        let kind = if turns_spent {
            Some(BudgetLimitKind::ModelTurns)
        } else if tools_spent {
            Some(BudgetLimitKind::ToolCalls)
        } else {
            None
        };
        match kind {
            None => BudgetDecision::Continue,
            Some(kind) => self.settle(kind, now),
        }
    }

    fn settle(&mut self, kind: BudgetLimitKind, now: Instant) -> BudgetDecision {
        // The reserve is one more provider turn. A tripped wall clock or an
        // unmeasurable cost cannot afford it; the countable bounds can.
        let affordable = match kind {
            BudgetLimitKind::Duration
            | BudgetLimitKind::CostUnknown
            | BudgetLimitKind::TokensUnknown => false,
            BudgetLimitKind::Cost
            | BudgetLimitKind::TotalTokens
            | BudgetLimitKind::InputTokens
            | BudgetLimitKind::OutputTokens
            | BudgetLimitKind::ToolOutputBytes => {
                // Cost, tokens, and tool bytes are only observed after a turn;
                // permitting the final response bounds the overshoot to one
                // tool-free reply, which the caller accepted by imposing the
                // cap.
                true
            }
            BudgetLimitKind::ModelTurns | BudgetLimitKind::ToolCalls => true,
        };
        if !affordable {
            return BudgetDecision::Exhausted(self.exhaustion(kind, false, now));
        }
        self.final_response_requested = Some(kind);
        BudgetDecision::FinalResponse(kind)
    }

    /// The typed outcome for a limit that settled the run.
    pub(crate) fn exhaustion(
        &self,
        kind: BudgetLimitKind,
        final_response: bool,
        now: Instant,
    ) -> BudgetExhaustion {
        let limits = &self.limits;
        let message = match kind {
            BudgetLimitKind::Duration => format!(
                "the run exceeded its {} ms wall-clock budget after {} ms",
                limits.max_duration_ms.unwrap_or_default(),
                now.saturating_duration_since(self.started).as_millis()
            ),
            BudgetLimitKind::ModelTurns => format!(
                "the run exhausted its {} model turn budget",
                limits.max_model_turns.unwrap_or_default()
            ),
            BudgetLimitKind::ToolCalls => format!(
                "the run exhausted its {} tool call budget after {} calls",
                limits.max_tool_calls.unwrap_or_default(),
                self.tool_calls
            ),
            BudgetLimitKind::TotalTokens => format!(
                "the run's {} total tokens exceeded its {} token budget",
                self.tokens.unwrap_or_default(),
                limits.max_total_tokens.unwrap_or_default()
            ),
            BudgetLimitKind::InputTokens => format!(
                "the run's {} input tokens exceeded its {} input token budget",
                self.input_tokens.unwrap_or_default(),
                limits.max_input_tokens.unwrap_or_default()
            ),
            BudgetLimitKind::OutputTokens => format!(
                "the run's {} output tokens exceeded its {} output token budget",
                self.output_tokens.unwrap_or_default(),
                limits.max_output_tokens.unwrap_or_default()
            ),
            BudgetLimitKind::TokensUnknown => {
                "the run's token usage became unknown, so its token budget could not be enforced"
                    .to_owned()
            }
            BudgetLimitKind::ToolOutputBytes => format!(
                "the run's {} bytes of tool output exceeded its {} byte budget",
                self.tool_output_bytes,
                limits.max_tool_output_bytes.unwrap_or_default()
            ),
            BudgetLimitKind::Cost => format!(
                "the run's estimated cost exceeded its budget ({} > {} USD nanos)",
                self.cost_usd_nanos.unwrap_or_default(),
                limits.max_cost_usd_nanos.unwrap_or_default()
            ),
            BudgetLimitKind::CostUnknown => {
                "the run's cost became unknown, so its hard cost budget could not be enforced"
                    .to_owned()
            }
        };
        BudgetExhaustion {
            limit: kind,
            final_response,
            message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: output,
            reasoning_tokens: None,
        }
    }

    fn pricing() -> ModelPricing {
        ModelPricing {
            input_usd_nanos_per_token: 1_000,
            output_usd_nanos_per_token: 2_000,
            cache_read_usd_nanos_per_token: None,
            cache_write_usd_nanos_per_token: None,
            context_tier: None,
            provenance: "test".to_owned(),
        }
    }

    #[test]
    fn unlimited_meter_always_continues() {
        let now = Instant::now();
        let mut meter = BudgetMeter::new(RunLimits::default(), None, now);
        for _ in 0..1_000 {
            meter.charge_turn(None);
            meter.charge_tool_calls(16);
            assert_eq!(meter.before_turn(now, 16), BudgetDecision::Continue);
        }
        assert_eq!(meter.deadline(), None);
    }

    #[test]
    fn turn_budget_reserves_the_last_turn_as_the_final_response_then_settles() {
        let now = Instant::now();
        let limits = RunLimits {
            max_model_turns: Some(3),
            ..RunLimits::default()
        };
        let mut meter = BudgetMeter::new(limits, None, now);
        assert_eq!(meter.before_turn(now, 0), BudgetDecision::Continue);
        meter.charge_turn(None);
        assert_eq!(meter.before_turn(now, 0), BudgetDecision::Continue);
        meter.charge_turn(None);
        // Turn three is the last permitted turn: it must be the final response.
        assert_eq!(
            meter.before_turn(now, 0),
            BudgetDecision::FinalResponse(BudgetLimitKind::ModelTurns)
        );
        meter.charge_turn(None);
        let BudgetDecision::Exhausted(exhaustion) = meter.before_turn(now, 0) else {
            panic!("a second exhausted check must settle")
        };
        assert_eq!(exhaustion.limit, BudgetLimitKind::ModelTurns);
        assert!(exhaustion.final_response);
    }

    #[test]
    fn tool_call_budget_reserves_room_for_the_next_turn() {
        let now = Instant::now();
        let limits = RunLimits {
            max_tool_calls: Some(20),
            ..RunLimits::default()
        };
        let mut meter = BudgetMeter::new(limits, None, now);
        meter.charge_tool_calls(10);
        assert_eq!(meter.before_turn(now, 10), BudgetDecision::Continue);
        assert_eq!(
            meter.before_turn(now, 11),
            BudgetDecision::FinalResponse(BudgetLimitKind::ToolCalls)
        );
    }

    #[test]
    fn deadline_cannot_afford_a_final_response() {
        let start = Instant::now();
        let limits = RunLimits {
            max_duration_ms: Some(100),
            ..RunLimits::default()
        };
        let mut meter = BudgetMeter::new(limits, None, start);
        assert_eq!(meter.deadline(), Some(start + Duration::from_millis(100)));
        assert_eq!(
            meter.before_turn(start + Duration::from_millis(99), 0),
            BudgetDecision::Continue
        );
        let BudgetDecision::Exhausted(exhaustion) =
            meter.before_turn(start + Duration::from_millis(100), 0)
        else {
            panic!("an elapsed deadline must settle immediately")
        };
        assert_eq!(exhaustion.limit, BudgetLimitKind::Duration);
        assert!(!exhaustion.final_response);
    }

    #[test]
    fn cost_budget_trips_on_spend_and_fails_closed_when_usage_goes_missing() {
        let now = Instant::now();
        let limits = RunLimits {
            max_cost_usd_nanos: Some(10_000),
            ..RunLimits::default()
        };
        let mut meter = BudgetMeter::new(limits, Some(pricing()), now);
        meter.charge_turn(Some(usage(4, 2))); // 4_000 + 4_000
        assert_eq!(meter.before_turn(now, 0), BudgetDecision::Continue);
        meter.charge_turn(Some(usage(4, 2)));
        assert_eq!(
            meter.before_turn(now, 0),
            BudgetDecision::FinalResponse(BudgetLimitKind::Cost)
        );

        let mut unmetered = BudgetMeter::new(limits, Some(pricing()), now);
        unmetered.charge_turn(Some(usage(1, 1)));
        unmetered.charge_turn(None);
        let BudgetDecision::Exhausted(exhaustion) = unmetered.before_turn(now, 0) else {
            panic!("unknown cost under a cost cap must settle")
        };
        assert_eq!(exhaustion.limit, BudgetLimitKind::CostUnknown);
        assert!(!exhaustion.final_response);

        let mut child = BudgetMeter::new(limits, Some(pricing()), now);
        child.charge_child(Some(usage(1, 1)), None);
        assert_eq!(
            child.exceeded(now),
            Some(BudgetLimitKind::CostUnknown),
            "an unmetered child makes the parent's cost unknown"
        );
    }

    #[test]
    fn token_budget_counts_every_turn_and_child_free() {
        let now = Instant::now();
        let limits = RunLimits {
            max_total_tokens: Some(100),
            ..RunLimits::default()
        };
        let mut meter = BudgetMeter::new(limits, None, now);
        meter.charge_turn(Some(usage(60, 30)));
        assert_eq!(meter.exceeded(now), None);
        meter.charge_turn(Some(usage(10, 5)));
        assert_eq!(meter.exceeded(now), Some(BudgetLimitKind::TotalTokens));
        // Cost stays unknown without pricing, but no cost cap was imposed.
        let mut priceless = BudgetMeter::new(limits, None, now);
        priceless.charge_turn(Some(usage(1, 1)));
        assert_eq!(priceless.exceeded(now), None);
    }

    #[test]
    fn child_usage_rolls_up_into_every_parent_token_bound() {
        let now = Instant::now();
        let limits = RunLimits {
            max_total_tokens: Some(100),
            max_input_tokens: Some(70),
            max_output_tokens: Some(40),
            ..RunLimits::default()
        };
        let mut meter = BudgetMeter::new(limits, None, now);
        meter.charge_turn(Some(usage(20, 10)));
        meter.charge_child(Some(usage(45, 10)), Some(0));
        assert_eq!(meter.exceeded(now), None);
        meter.charge_child(Some(usage(10, 0)), Some(0));
        assert_eq!(meter.exceeded(now), Some(BudgetLimitKind::InputTokens));

        let mut meter = BudgetMeter::new(limits, None, now);
        meter.charge_child(Some(usage(0, 41)), Some(0));
        assert_eq!(meter.exceeded(now), Some(BudgetLimitKind::OutputTokens));

        // A child whose usage was lost makes the parent's tokens unknown, so
        // a token bound fails closed exactly like a usage-less parent turn.
        let mut meter = BudgetMeter::new(limits, None, now);
        meter.charge_child(None, Some(0));
        assert_eq!(meter.exceeded(now), Some(BudgetLimitKind::TokensUnknown));
        // Without any token bound, lost child usage is not an exhaustion.
        let mut unbounded = BudgetMeter::new(RunLimits::default(), None, now);
        unbounded.charge_child(None, None);
        assert_eq!(unbounded.exceeded(now), None);
    }

    #[test]
    fn remaining_budget_is_the_unspent_part_of_each_inherited_bound() {
        let now = Instant::now();
        let limits = RunLimits {
            max_duration_ms: Some(10_000),
            max_model_turns: Some(8),
            max_tool_calls: Some(50),
            max_total_tokens: Some(1_000),
            max_cost_usd_nanos: Some(4_000_000),
            max_input_tokens: Some(600),
            max_output_tokens: Some(400),
            max_tool_output_bytes: Some(9_999),
            max_children: Some(4),
            max_concurrent_children: Some(2),
        };
        // 0% spent: the child receives the full inherited caps and none of
        // the per-run ones.
        let fresh = BudgetMeter::new(limits, Some(pricing()), now);
        let remaining = fresh.remaining(now).unwrap();
        assert_eq!(remaining.max_duration_ms, Some(10_000));
        assert_eq!(remaining.max_total_tokens, Some(1_000));
        assert_eq!(remaining.max_input_tokens, Some(600));
        assert_eq!(remaining.max_output_tokens, Some(400));
        assert_eq!(remaining.max_cost_usd_nanos, Some(4_000_000));
        assert_eq!(remaining.max_model_turns, None);
        assert_eq!(remaining.max_tool_calls, None);
        assert_eq!(remaining.max_tool_output_bytes, None);
        assert_eq!(remaining.max_children, None);
        assert_eq!(remaining.max_concurrent_children, None);

        // 50% spent: each bound is the difference, wall clock included.
        let mut half = BudgetMeter::new(limits, Some(pricing()), now);
        half.charge_turn(Some(usage(300, 200)));
        let later = now + Duration::from_millis(5_000);
        let remaining = half.remaining(later).unwrap();
        assert_eq!(remaining.max_duration_ms, Some(5_000));
        assert_eq!(remaining.max_total_tokens, Some(500));
        assert_eq!(remaining.max_input_tokens, Some(300));
        assert_eq!(remaining.max_output_tokens, Some(200));
        // pricing(): 1_000 nanos per input token, 2_000 per output token.
        let spent = 300 * 1_000 + 200 * 2_000;
        assert_eq!(remaining.max_cost_usd_nanos, Some(4_000_000 - spent));

        // 100% spent (or over): the exhausted family refuses the child.
        let mut spent = BudgetMeter::new(limits, Some(pricing()), now);
        spent.charge_turn(Some(usage(600, 0)));
        assert_eq!(spent.remaining(now), Err(BudgetLimitKind::InputTokens));
        let mut timed_out = BudgetMeter::new(limits, Some(pricing()), now);
        timed_out.charge_turn(Some(usage(1, 1)));
        assert_eq!(
            timed_out.remaining(now + Duration::from_millis(10_000)),
            Err(BudgetLimitKind::Duration)
        );
        let mut unknown = BudgetMeter::new(limits, Some(pricing()), now);
        unknown.charge_turn(None);
        assert!(matches!(
            unknown.remaining(now),
            Err(BudgetLimitKind::CostUnknown | BudgetLimitKind::TokensUnknown)
        ));

        // No inherited bounds at all: the child is unbounded too.
        let free = BudgetMeter::new(RunLimits::default(), None, now);
        assert_eq!(free.remaining(now), Ok(RunLimits::default()));
    }
    #[test]
    fn split_token_and_tool_output_bounds_settle_with_their_own_kinds() {
        let now = Instant::now();
        let mut meter = BudgetMeter::new(
            RunLimits {
                max_input_tokens: Some(100),
                ..RunLimits::default()
            },
            None,
            now,
        );
        meter.charge_turn(Some(usage(60, 500)));
        assert_eq!(meter.exceeded(now), None);
        meter.charge_turn(Some(usage(60, 0)));
        assert_eq!(meter.exceeded(now), Some(BudgetLimitKind::InputTokens));
        assert!(
            meter
                .exhaustion(BudgetLimitKind::InputTokens, false, now)
                .message
                .contains("120 input tokens")
        );

        let mut meter = BudgetMeter::new(
            RunLimits {
                max_output_tokens: Some(10),
                ..RunLimits::default()
            },
            None,
            now,
        );
        meter.charge_turn(Some(usage(1_000, 11)));
        assert_eq!(meter.exceeded(now), Some(BudgetLimitKind::OutputTokens));
        assert_eq!(
            meter.before_turn(now, 0),
            BudgetDecision::FinalResponse(BudgetLimitKind::OutputTokens)
        );

        // Lost usage under any token bound is the explicit unknown kind, which
        // cannot afford a final response.
        let mut meter = BudgetMeter::new(
            RunLimits {
                max_output_tokens: Some(10),
                ..RunLimits::default()
            },
            None,
            now,
        );
        meter.charge_turn(None);
        assert_eq!(meter.exceeded(now), Some(BudgetLimitKind::TokensUnknown));
        let BudgetDecision::Exhausted(exhaustion) = meter.before_turn(now, 0) else {
            panic!("unknown tokens must settle immediately")
        };
        assert_eq!(exhaustion.limit, BudgetLimitKind::TokensUnknown);
        assert!(!exhaustion.final_response);

        let mut meter = BudgetMeter::new(
            RunLimits {
                max_tool_output_bytes: Some(1_000),
                ..RunLimits::default()
            },
            None,
            now,
        );
        meter.charge_tool_output(600);
        assert_eq!(meter.exceeded(now), None);
        meter.charge_tool_output(401);
        assert_eq!(meter.exceeded(now), Some(BudgetLimitKind::ToolOutputBytes));
        assert!(
            meter
                .exhaustion(BudgetLimitKind::ToolOutputBytes, true, now)
                .message
                .contains("1001 bytes")
        );
    }
}
