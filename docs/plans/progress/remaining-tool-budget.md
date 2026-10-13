# Remaining tool allowance progress

| Slice | Goal | Status | Branch | Notes |
| --- | --- | --- | --- | --- |
| TB1 | Admit and cap remaining executable calls consistently | Implementation candidate ready for focused verification | fix/qq-tool-budget-20261013 | Baseline recorded on d223bf0; root owns live integration |

## 2026-10-13

- Root assigned core budget/dispatch ownership after its actual zero-call
  budget exhaustion. Existing author independently confirmed fixed16 preturn
  reservation and separate dispatch cap. No source edited before specification.
- Coordinator created the isolated worktree and specification. Existing JD1
  candidate and current live runtime remain preserved. Video tools are present;
  actual new regression recording is pending, not reconstructed evidence.
- Added baseline-compiling runtime regressions in `crates/qq-core/src/lib.rs`:
  `caller_tool_budget_caps_each_batch_and_rejects_excess_before_execution`
  covers fresh one-, two-, and six-call bounds plus a seven-call batch;
  `caller_tool_budget_spends_remaining_calls_then_allows_one_final_response`
  covers four charged calls, two remaining calls, the tool-free final response,
  terminal exhaustion, and three provider requests. The caps test also checks
  the final request declares no tool choice and emits exactly one terminal
  exhaustion. Tests have not been run by this author. Before TB1, the fixed
  sixteen-call preturn reservation is expected to reject these bounded runs
  before their first tool turn; parent owns the recorded baseline execution and
  raw/video evidence.
- Parent recorded that baseline separately on clean `d223bf0`: both runtime
  regressions failed, including the six-call case with zero completed tools;
  retained evidence is under `evidence/qq-tool-budget-20261013` (baseline
  cast, runtime log, and checks receipt). The implementation draft now carries
  a typed per-turn allowance from `BudgetMeter` into dispatch, preserves the
  tool-free final-response boundary, and keeps rejected calls out of charging.
  Focused tests and formatting remain pending; this author has not rerun them
  after implementation.
