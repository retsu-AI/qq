# Remaining executable tool allowance

## TB1 contract

Root reported actual run `f127ca3d16dbf6e84904a3945bb9f0d4` exhausted a six-call
budget after zero calls. Frozen8591c70 checks whether charged calls plus the fixed
16-call maximum fit before a turn, while dispatch independently permits sixteen.
TB1 fixes that shared allowance rather than raising the approved limits.

Before each ordinary turn, core computes `min(requested_turn_tools,
max_tool_calls.saturating_sub(charged_calls))`; absent a caller tool bound, the
allowance is the existing requested maximum of16. The typed `BudgetDecision`
Continue variant carries this allowance. The turn loop binds that exact value
and uses it to cap executable dispatch and describe excess-call rejections.
Excess calls retain ordinary rejection results before any tool effect, and only
nonrejected executable calls are charged. The raw provider-call safety bound64
remains independent and unchanged.

An imposed caller tool bound with zero remaining work grants the existing single
tool-free final response, then terminal budget exhaustion. Zero requested tools
without a spent imposed bound remains an ordinary tool-free turn. Duration,
model-turn, token/cost unknown, cancellation, approval, retry, slice and final
response precedence retain their existing rules. Existing conservative slice
reservation is outside this fix unless integration proves it must use the new
allowance to remain correct. Tool-stage6/total24 and MCP concurrency1 remain fixed.

## Ownership and preservation

Author owns `crates/qq-core/src/runtime/budget.rs`, the related turn/dispatch
sections and focused tests in `crates/qq-core/src/lib.rs`, one relevant existing
architecture/protocol paragraph, and this plan's progress ledger. Worktree is
`startup/worktrees/qq-tool-budget-20261013`, branch
`fix/qq-tool-budget-20261013`, base8591c705cf97ab4d38ea6dc0b4894e0af15aacd0.
Existing author and nonauthor verifier are reused; the coordinator integrates.
The separate QQ lead owns helper event-order repairs. No auth, JEV, public
protocol, durable schema, provider or tool implementation change is assigned.

All state remains core-owned and per-run. No new storage, migration, background
work, dependencies or I/O is introduced. Protocol31/store43, historical events,
Keychain protections and accepted JD1 remain preserved. Root alone qualifies
combined source, signs/selects installation, operates server61239/tmux and runs
provider trials. Retain the source/patch/binary/review and runtime rollback.
Owned disposable build output is exactly this worktree's `target/`.

## Acceptance and recording

1. A fresh six-call budget admits an ordinary turn and executes a tool.
2. A fresh one-call budget executes at most one call. A seven-call batch under
   six executes at most six and rejects excess before any side effect.
3. Four charged calls from six leave two; those two charge exactly, then one
   tool-free final response and one exhaustion settle. A new run recovers with
   its own fresh allowance. Rejected calls do not consume caller allowance.
4. Default/unbounded allowance remains sixteen, with excess-call behavior and
   raw safety bound unchanged. Other budget/final-response tests retain their
   meaning; update expectations only where they encode the reproduced defect.
5. Record the new required regression baseline before implementation and the
   candidate's meaningful focused verification as they execute. Keep actual
   timestamped asciicast plus raw logs and MP4 rendition, exact revision,
   environment, literal expected/observed results and failure receipts. A
   source/test recording does not establish provider or visible Warp acceptance.
6. Run focused checks, then required workspace formatting, tests, Clippy and
   build; obtain independent nonauthor acceptance of the final candidate and
   recording coverage. Preserve all failed results and do not rerun accepted
   unchanged suites merely to obtain footage.

Record via the documented CLI terminal recorder, without input/secret capture,
uploads or native Warp screen capture. Existing Computer safety denial remains.
Root records the upcoming qualified live run separately. No live provider retry,
server restart, signing/install or public publication belongs to this lane.
