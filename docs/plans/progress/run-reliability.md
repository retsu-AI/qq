# Ledger — Run reliability

Plan: [`../run-reliability.md`](../run-reliability.md). Only the agent
working this plan edits this file. Current state on top; dated entries
appended below, newest last.

| Slice | Goal | Status | Linear | Branch / PR | Notes |
| --- | --- | --- | --- | --- | --- |
| RR1 | Checkpoint turn tolerates a tool call | Shipped (#108) | [ENG-863](https://linear.app/retsu-ai/issue/ENG-863) | `feat/rr1-checkpoint-tolerance` | 5 runs / 120 min in the audit |
| RR2 | Slash/empty-prompt validation at admission | Shipped (#116) | [ENG-864](https://linear.app/retsu-ai/issue/ENG-864) | `fix/rr2-slash-admission` | 3 slash runs; the 5 "messages must not be empty" runs predate #27 |
| RR3 | Jev exhaustion is an outcome, not a failure | Shipped (#117) | [ENG-865](https://linear.app/retsu-ai/issue/ENG-865) | `fix/rr3-jev-verdict-outcome` | 9 runs |
| RR4 | Turn-level recovery; `Paused`; `TurnRetry`; ADR-0040 superseding 0005 | Shipped (#120) | [ENG-867](https://linear.app/retsu-ai/issue/ENG-867) | `feat/rr4-turn-recovery` | 12 runs / 4.5 h; protocol 25 → 26 |
| RR5 | `Retry-After` ≤ 60 s; 529 retryable; HTTP-date | Shipped (#118) | [ENG-866](https://linear.app/retsu-ai/issue/ENG-866) | `fix/rr5-retry-after` | provider crate; minimal profile green |
| RR6 | Reactive overflow; un-wedge admission (mid-run compaction shipped in #92) | In review | [ENG-868](https://linear.app/retsu-ai/issue/ENG-868) | `feat/eng-868-rr6-reactive-overflow` | 9 runs / 3 sessions; independent review |
| RR7 | Estimate calibration from reported usage | In review | [ENG-869](https://linear.app/retsu-ai/issue/ENG-869) | `feat/eng-869-rr7-estimate-calibration` | deferred from F04 |
| RR8 | Output-token handling and persisted `max_output_tokens` floor | Planned | [ENG-870](https://linear.app/retsu-ai/issue/ENG-870) | | 5 runs |
| RR9 | Approval deadline policy | Planned | [ENG-871](https://linear.app/retsu-ai/issue/ENG-871) | | 4 timeouts |
| RR10 | Lenient tool-argument decode | Planned | [ENG-872](https://linear.app/retsu-ai/issue/ENG-872) | | ~11 wasted turns |
| RR11 | Read-hash ledger persisted | Planned | [ENG-873](https://linear.app/retsu-ai/issue/ENG-873) | | 14 refusals |
| RR12 | Stream leniency, loop result, latency stats | Planned | [ENG-874](https://linear.app/retsu-ai/issue/ENG-874) | | latent |

## Entries

### 2026-09-21 — plan opened

Research in `docs/design/run-reliability-audit-2026-09-21.md`: read-only
analysis of the operator's live store (190 runs, 27 % prompt failure, 73 % of
failures harness-caused) plus four reference traces (Codex, OpenCode, Pi, fx).
No code changed. Root granted ADR-0040 for two-phase retry ownership
(supersedes ADR-0005; 0038/0039 were already taken by retention and in-run
compaction). `RunOutcome::Paused` is a protocol bump and needs a root row
before RR4 starts. Rebased onto `322aa94`: F03 (#92) shipped in-run
compaction, so RR6 narrows to reactive overflow and admission un-wedging.

### 2026-09-21 — RR1 checkpoint tolerance

The checkpoint turn keeps its tool schemas; `request_has_tools` no longer
excludes it. A tool call streamed on that turn is admitted with
`SLICE_CHECKPOINT_REJECTION` through the same `PendingToolCall.rejection` path
as over-cap calls, does not count toward the slice, and settles as one
not-executed result the continuation turn sees; the slice resets whether or
not the model obeyed the notice. The `ProviderProtocol` failure for this case
is gone (the "declared no tools" branch remains for budget-final turns).
Regression: `a_tool_call_on_the_checkpoint_turn_settles_as_a_rejection_and_the_run_continues`
(255 executed reads, one rejected call, continuation carries the result,
run completes). Fixtures that identified the checkpoint by an empty tool
list now key on the notice. The measured-token chain now varies only by the
notice bytes across the seam. `tool_choice: none` was not added: no adapter
carries a tool-choice field today and the rejection result makes it
unnecessary for correctness. Consider deleting the checkpoint once RR12's
loop result lands (in-run compaction is already the durable boundary).

### 2026-09-21 — RR2 slash admission

The `invalid_command` bucket split on inspection: 3 runs were slash names
(`/clear`, `/agents`, one malformed) and 5 were "conversation messages must
not be empty" on ordinary prompts in two sessions whose prior runs had only
reasoning/tool-only assistant turns. Those 5 are dated 2026-09-11 05:42Z and
18:21Z; #27 (`1747435`, 2026-09-12 03:52Z, `usable_conversation`) fixed
exactly that and they have not recurred, so RR2 does not touch it. For the
slash cases: `SessionCommand::SubmitPrompt` now validates the rendered
prompt's leading name against the grammar and the reserved vocabulary
(`validate_slash_prompt`, no I/O) and refuses with the new
`SessionRuntimeError::InvalidSlashCommand(SlashCommandError)`; no run row,
no failure notice on the next prompt. Server maps it to `InvalidRequest`,
headless to `InvalidConfiguration`. `/clear` joins the client vocabulary as
an alias of `/new` (protocol constant 20 → 21 entries; additive, no
`PROTOCOL_VERSION` bump). Unknown-but-well-formed names still fail the run:
only the workspace index can decide them, and the TUI already completes
from that index. Regression:
`unresolvable_slash_prompts_are_refused_at_admission_without_a_run_row`,
`slash_clear_is_a_client_alias_for_a_new_session`.

### 2026-09-21 — RR3 Jev verdicts are evidence

Six `RunFailureKind::Policy` exits in the run loop turned a reviewer's
opinion into a failed run: two-RED exhaustion (final and tool phases),
reviewer `Unavailable` (timeout/malformed reply, final and tool phases), and
the two "exceeded the exact review bound" cases. All six now record the
`CheckpointReviewed` event (and the `[JEV …]` marker on the retained result
for tool phases) and let the run continue; RED still redirects while
`CheckpointContext::repair()` has an attempt left. The 32-request limit and
cost admission stay failures: those are harness bounds. Tests renamed to
match (`…_and_completes`), the repeated-RED loop case now asserts 32 durable
Contradicted verdicts and no "correction attempts" failure, and
`a_final_candidate_rejected_twice_completes_with_the_verdicts_on_record`
is the regression for the 9 audited runs. The operator can re-enable
`jev_review` without a disagreement costing the run.

### 2026-09-21 — RR5 Retry-After and 529

`http.rs`: 529 joins `is_retryable_status` (five audited runs "gave up after
8 attempts" on Anthropic `overloaded_error`, which the provider-agnostic
status check never retried before the stream layer saw it). `Retry-After`
is now a floor rather than a value clamped to `max_delay`: honoured in full
above 8 s up to `RETRY_AFTER_CAP` (60 s), never jittered down, and parsed in
HTTP-date form via `httpdate` (already in the lock through hyper; zero
transitive additions). The ledger charges a server-directed wait at the
exponential rate so honouring the server cannot by itself exhaust the 30 s
budget. #114 (retry attempts that outlive the budget) landed first from
another lane and is compatible. Tests: HTTP-date past/future, 529, cap,
ledger charge; both provider profiles green.

### 2026-09-21 — RR4 turn recovery (ADR-0040)

The run loop owns recovery of a turn once the provider's own ledger is
spent. A stream `Err` of kind `ProviderUnavailable` / `ProviderRateLimited`
/ `ProviderTransport`, or a stream that ends after events without a
terminal event, now commits the partial assistant turn through the existing
`AssistantTurnCompleted` path, emits `RuntimeEvent::TurnRetrying`, sleeps
under `TurnRecoveryPolicy` (default 2 s → 60 s; `Runtime::with_turn_recovery`
and `AgentProfile::with_turn_recovery` for embedders and tests) in a
`select!` with cancellation and the run deadline, and re-issues the turn with
`TURN_RETRY_CONTINUE_NOTICE` after any partial text. `turn_retries` resets on
a completed turn. Exhaustion at `MAX_TURN_RETRIES` (5, in `qq-protocol` so
clients render against the same bound) settles `RunOutcome::Paused { pause }`
with run status `paused`. Non-transient kinds still `Failed` at once. The
decision to also retry pre-event faults (rather than pause immediately) is
deliberate: the provider's ledger is sub-minute; the run's is minute-scale.

Protocol 25 → 26: `SessionEvent::RunTurnRetrying`, `RunOutcome::Paused`,
`RunStatus::Paused`, `RunPause`; `v26/` goldens (`event_run_turn_retrying`,
`event_run_finished_paused`), harbor traces bumped. No schema change;
`'paused'` added to every terminal-status list (claim fold-stop, child
owner queries, snapshot accounting, transcript notice, abandoned-child
recovery) — two of those lists had also omitted `'budget_exhausted'`, now
included. Client reducer shows a warning notice per retry and on pause; TUI
sidebar/transcript render `paused` like `budget_exhausted`; headless maps
`paused` → `task_failed` (exit 1) with the pause named. Sub-agent `paused`
returns a tool error to the parent; a paused compaction step stops the fold.

Tests: `transient_faults_retry_the_turn_and_exhaustion_pauses` (three
transient kinds × 6 sends → paused; auth fails once; mid-stream 529×2 then
completion yields `part0 part1 done` with the continue notice),
`the_turn_retry_allowance_resets_on_a_completed_turn` (9 faults across 9
turns, never a second attempt, completes),
`a_retry_sleep_yields_to_cancellation_and_the_deadline`. Session fixtures
that asserted `Failed` for an offline provider now assert `Paused` and
`n + MAX_TURN_RETRIES` sends; `loaded_runtime` uses a 1 ms policy so the
suite stays at ~15 s. Replaces `the_run_loop_never_resends_a_turn`.
Workspace green incl. minimal provider profile. Independent review
requested per `workflow.md` § 4 (touches `sessions/`, protocol bump).

### 2026-09-22 — RR6 reactive overflow and un-wedged admission (ENG-868)

(b) In the run loop, a `ProviderContextExceeded` stream error before any
block streamed, on a run with a compactor and a compaction boundary, sets
`provider_overflowed` and `continue 'turns`: the next pass forces the stub
and in-run compaction paths regardless of the estimate, then re-issues the
turn. Granted once per turn ordinal (`reactive_compaction_turn`); a second
rejection fails as before. New informational `RuntimeEvent::ProviderOverflow`
clears the session's measured occupancy basis. (c) At admission, a
`Reject(Exhausted(Attempted))` plan — or a known-overflow repeat whose fold
is exhausted — no longer fails the prompt: `admit_with_summary_only_history`
reloads the reserved prompt, prepends `SUMMARY_ONLY_NOTICE` + the latest
between-run summary (from the new `Store::latest_compaction_summary`), and
loops once (`summary_only_admission`); the smaller request is judged on its
own. The known-overflow basis check is skipped for the downgraded shape
since the shape-level basis is byte-independent. No store schema change,
no protocol change. Tests:
`a_provider_window_rejection_compacts_the_run_and_continues` (scripted 413
at 7 results with a 200k window → one in-run compaction, 12 calls once,
next prompt normal) and
`an_exhausted_fold_admits_the_prompt_with_summary_only_history` (step one
commits, prompt rejected, step two rejected → retry runs from the summary,
four prompts still in the store). Five fixtures that asserted the old
"exhausted → refuse" policy now assert summary-only admission; the
`Sequence` scripts account for the summarizer's turn retries. New harness
script `ShellRepeatedlyWithProviderOverflow`. Independent review requested
(touches `sessions/`).

### 2026-09-22 — RR7 estimate calibration (ENG-869)

The measured-occupancy chain already seeds from the provider's reported
input tokens; what it charged for byte deltas was the fixed four
bytes/token, so on a code-heavy transcript (~3.1 B/t) every appended turn
was under-charged by ~22 % and the estimate drifted low until the provider
rejected. `adjust_measured_tokens` now charges and credits at
`calibrated_bytes_per_token(measured_tokens, measured_bytes)`: the ratio
the measurement itself implies, rounded to nearest, clamped to 2–6,
defaulting to 4 below 2 000 measured tokens (noise floor). The run loop
calibrates once against the whole measured request and applies each
component delta via `adjust_measured_tokens_at`. The raw byte estimate for
unmeasured requests is unchanged (that is the first request of a session;
RR6 covers a provider rejection of it). No schema or protocol change. Tests:
`deltas_are_charged_at_the_ratio_the_measurement_established`, the
acceptance fixture
`calibration_holds_the_estimate_within_ten_percent_on_a_code_heavy_transcript`
(eight turns at 3.1 B/t, worst error < 10 %; the default would be 22 %
under per delta), plus the existing chain and boundary tests.
