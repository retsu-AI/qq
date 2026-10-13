# Remaining tool allowance progress

| Slice | Goal | Status | Branch | Notes |
| --- | --- | --- | --- | --- |
| TB1 | Admit and cap remaining executable calls consistently | Session regression repaired; parent full gates pending | fix/qq-tool-budget-20261013 | Baseline recorded on d223bf0; root owns live integration |

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
  retained evidence is under
  `/Users/romanmondello/Documents/Codex/2026-07-26/rev/reviews/alignment-20261011/qq-recovery-20261012/qq-tool-budget-20261013`
  (`baseline.cast`, `baseline-runtime-regressions.log`, and
  `baseline-checks.json`). The implementation draft now carries
  a typed per-turn allowance from `BudgetMeter` into dispatch, preserves the
  tool-free final-response boundary, and keeps rejected calls out of charging.
- Parent verified candidate `2813717` with the two runtime regressions and ten
  budget unit tests passing; the recorded baseline remains labeled separately.
  Baseline files are retained at the absolute path above; rendered candidate
  footage is retained by the parent. Pinned formatting was then
  applied to the two owned layouts in `lib.rs` and `runtime/budget.rs`; no
  tests or builds were rerun by this author.
- The workspace gate later passed fmt, all-target/all-feature Clippy with
  `-D warnings`, and build on `e6b0879`; workspace tests exposed only the stale
  sessions expectation for the old fixed-turn reservation. This repair renames
  that test, uses `max_tool_calls: Some(3)`, asserts three successful calls,
  one final `ToolChoice::None` request, one terminal exhaustion, an idle session,
  and a fresh one-call run in the same session.
- The repaired candidate `ba84158` passed the focused session check, fmt,
  workspace tests, and build; Clippy then reported only
  `await_holding_lock` in the new session assertion. The lock assertions are
  now in a lexical scope so no guard crosses the subsequent awaits.
