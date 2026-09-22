# QQ run-reliability audit: why sessions do not finish

Research snapshot: 2026-09-21, QQ `2e5e2ce`. Reference trees under `.source/`
at the revisions recorded in
[`harness-scale-audit-2026-09-16.md`](harness-scale-audit-2026-09-16.md)
§ Scope. This document records evidence and proposes work; it does not claim
implementation. Implementation status lives in
[`../plans/progress/run-reliability.md`](../plans/progress/run-reliability.md).

## Question

The user's report: "most sessions in QQ don't complete or take far too long to
complete; OpenCode and fx are much more reliable and fail less." This audit
answers with (1) the operator's own session database, which records every run
outcome since July, and (2) a mechanism-level comparison of how Codex,
OpenCode, Pi, and fx handle the same failure classes.

## Evidence 1 — the live session store

Read-only copy of `~/.local/share/qq/sessions.sqlite3` (117 MiB, schema 32,
83 sessions, 190 runs, 7 331 tool calls, 2026-07-28 → 2026-09-21). Queries are
in § Appendix.

### Outcomes

| Run kind | Completed | Failed | Cancelled | Interrupted | Running |
| --- | ---: | ---: | ---: | ---: | ---: |
| `prompt` (179) | 115 | **48 (27 %)** | 13 | 2 | 1 |
| `compaction` (11) | 5 | 6 | | | |

33 of the failed runs had started; they consumed **8.46 h** of wall time before
failing. Failed runs averaged **97 model turns** (completed runs 58), i.e. they
died late, after most of the work.

### Failure taxonomy (54 failed runs)

| Class | Runs | Terminal message (abridged) | Avoidable by the harness? |
| --- | ---: | --- | --- |
| Context overflow, compaction "already attempted" / "irreducible" | 9 | `estimated … context requires 352 546 input tokens … exceeding the … 272 000-token window; automatic compaction was already attempted for this prompt` | **Yes.** 8 of 9 have `started_at IS NULL`: the session is wedged and every later prompt fails identically. 3 sessions were lost this way. One 55-min run on a 500 k window died at 729 k estimated tokens |
| Jev "exhausted its two correction attempts" | 9 | policy | **Yes.** A completed answer the reviewer disliked twice becomes a *failed run*. Operator has since set `jev_review: off` locally |
| Provider overload/unavailable | 8 | `HTTP 529: Overloaded (gave up after 8 attempts with backoff)` ×5; `503 … BedrockException` ×1; `Internal server error` ×2 | **Yes.** Every reference retries the model *turn*, not only the HTTP send, and none give up in 30 s. Runs of 2–22 min lost |
| Unknown slash command / empty prompt | 8 | `unknown command or skill /clear`, `/agents`; `slash invocation names must start with …`; `conversation messages must not be empty` | **Yes.** A typo creates a run row, fails it, and leaves "The previous run failed" in the next prompt's context |
| Tool call on the tool-free checkpoint turn | 5 | `provider requested a tool on the tool-free checkpoint turn, which declares none` | **Yes.** All via LiteLLM→Anthropic. Runs of 6, 10, 10, 14 and **80 min** lost |
| Transport error mid-run | 4 | `provider request failed: error sending request` | **Yes.** 17, 35, 53, 56 min lost; each via the LiteLLM route |
| Max output tokens as a hard error | 4 + 1 | `provider response was incomplete: … reached the maximum output token limit`; `stopped at its output token limit (2048 tokens) on 4 consecutive turns` | **Yes.** Sessions persisted `max_output_tokens` 2 048/4 096 from before the 16 384 default; a truncated tool call is fatal instead of "re-issue" |
| Compaction summary missing required sections | 1 | policy | **Yes.** A summary without the six headings fails the auto-compaction run instead of re-prompting |
| Project trust required | 3 | configuration | Expected (first run in a new checkout) |
| Wrong API for model | 2 | `openai.gpt-5.6-sol does not support /v1/responses` | Configuration; should be caught at plan compile |

Roughly **35 of 48 prompt failures (73 %) were harness policy decisions** on
top of a recoverable situation. The model or provider produced something QQ
declined to continue from.

### Tool-call layer

7 331 calls; 146 (2 %) failed, 11 denied, 4 denied by the 300 s approval
timeout (`denied_timeout`, exactly 300.0 s each). Failures that are contract
strictness rather than model error:

| Pattern | Count | Example |
| --- | ---: | --- |
| `edit_file` refused: "has not been read in this session" / "changed since it was last read" | 14 | Includes cases where the file *was* read in a prior run of the same session |
| `search` argument shape | 8 | `invalid type: string "[\"**/*.rs\"]", expected a sequence`; `unknown field cwd`; `context must be at most 5`; `max_per_file must be between 1 and 50` |
| MCP `skills`/`list-artifacts` with no arguments | 3 | `expected "object"` — QQ sent `null`/absent instead of `{}` |
| `spawn_agent` over the 8-per-run cap | 5 | `this run already spawned 8 sub-agents` |
| `spawn_agent` child died of the checkpoint bug | 2 | `the sub-agent run failed: provider requested a tool on the tool-free checkpoint turn` |

Every one of these costs a full model turn (20–35 s on the observed routes).

### Where the time goes

For completed prompt runs over 60 s: mean 451 s, of which tool execution is
**19 %**; the remainder is provider latency × turn count. Per-turn wall time
(previous turn complete → this turn complete, includes tools): LiteLLM
`claude-fable-5-1` 20 s (n = 1 804), `xai/grok-4.6` 34 s (n = 331), direct
`bedrock-mantle/claude-sonnet-5` 6.8 s (n = 269). Runs averaging 57 turns at
20–34 s per turn are 20–30 minutes by construction; the "takes far too long"
complaint is turn count and route latency, then the retry-free failures that
discard the run at minute 50. `spawn_agent` averaged 577 s per child
(n = 24), so a coordinator that spawns eight serial children spends over an
hour waiting.

## Evidence 2 — how the references stay alive

Four read-only agents traced the same failure classes through each reference.
Anchors are `path:line` in the snapshot. What matters is the shared shape, not
any one implementation.

### The shared structure

| Property | Codex | OpenCode | Pi | fx | QQ today |
| --- | --- | --- | --- | --- | --- |
| Provider/transport error retries the **model turn** (prompt rebuilt from history) | 5 stream retries × 4 HTTP; connection failures retried **unbounded** 5→60 s (`responses_retry.rs:58-83`) | 5 turn retries, 2 s·2ⁿ ≤ 30 s, status ≥ 500 **or** message pattern (`session/retry.ts:85-155`) | 3 turn retries 2/4/8 s, regex classifier incl. `stream ended before message_stop` (`ai/src/utils/retry.ts:26-90`) | 10 attempts per turn, durable, resumable; exhaustion **pauses** (`model_response_recovery.zig:3,109-169`) | 4 HTTP sends in 30 s, pre-first-event only; anything later fails the run (ADR-0005) |
| Mid-stream disconnect | retryable `Stream` error | `ResponseStreamError`, retryable; 300 s SSE idle watchdog | retryable pattern | `response_interrupted` → `continue_response` | run failure |
| `max_tokens` / `length` | retryable stream error | normal completion, no error (`prompt.ts:1111-1116`) | normal stop; truncated tool calls get a "re-issue" tool result (`agent-loop.ts:379-404`) | text: completed with notice; with tool calls: turn failed, history intact | hard failure when a tool call was open; else ≤ 3 continuations then failure |
| Context overflow | compact at 90 % from usage, **mid-turn between tool calls**, no once-only guard; compaction request that overflows drops oldest item and retries (`compact.rs:313-322`) | overflow (estimated or provider 413) inserts a compaction step + synthetic "continue" user message; repeatable (`prompt.ts:1320-1328`) | compact before every turn inside a run; overflow recovery once **per progress**, reset on any good turn (`agent-session.ts:643,694-696`) | compact at 80 % to 10 % target inside the step loop; one reactive retry on provider overflow | between prompts only; later-turn overflow fails closed; "already attempted" wedges the session |
| Tool argument / unknown tool / exception | `RespondToModel` → failed `FunctionCallOutput`; no `deny_unknown_fields` | `invalid` fallback tool; `additionalProperties` not enforced; repair hook | TypeBox `Value.Convert` coercion + custom string→number/bool; `edit.edits` accepts a JSON string (`tools/edit.ts:103-134`); streaming JSON repaired | canonical `retry_with` correction object; numeric strings and JSON-string objects normalized (`shell.zig:280-315`) | `#[serde(deny_unknown_fields)]` on every tool; no coercion; result is an error the model must re-issue |
| Tool-free checkpoint / hard tool-call cap | none | none (`steps` opt-in; V2 settles a violating call as a tool error) | none | none; "Summarize what you just did" injected **with tools still declared** | 256-call slice → tools removed → a tool call is a **protocol failure** |
| Loop detection | none | 3 identical calls → `doom_loop` permission *ask* | none | 3 batches all-malformed, or identical failing shell command → turn failed, history intact | none |
| Approval wait | no timeout; headless `Never` rejects instantly to the model | no timeout; headless auto-rejects with warning | hooks; headless `confirm()` → false immediately | no timeout; headless → `permission_required` **tool result** | 300 s then `denied_timeout` |
| Unknown slash command | client-side | client-side; server throws before any message exists | client-side | transcript notice only | failed run in the store |
| Provider stream protocol nits | unknown events ignored; missing ids back-filled; ordering issues log-only in release (`util.rs:93-99`) | AI SDK tolerant | blocks keyed by index; unknown deltas ignored | tolerant | reasoning-block order, empty id, >128-byte name, 1 MiB reasoning → run failure |
| What remains after the worst case | turn `Error`, thread continuable | error on the message, session idle | `stopReason: "error"` message, errored messages excluded from next prompt | `paused` with recovery checkpoint, `/continue-recovery` | `failed` run; next prompt inherits "The previous run failed" |

The consistent lesson: in every reference the unit that fails is a **message
or turn**, the session stays continuable, and the harness's own bounds are
enforced by *what it sends next*, never by declaring the model's response
illegal.

### Where QQ is already ahead

Keep these: persist-before-publish and settlement (`sessions/settlement.rs`),
exact-argv `exec`, effect-classified approval with a `Forbidden` tier,
bounded output with spill handles, multi-range hash-checked reads, batch edits
with CAS, run-wide deadlines (F02), attachment provenance (F05), retained-
context assembly (F06), bounded fold compaction (F04). None of the references
match the durability story; several of them fail unrecoverably on crash
mid-turn (OpenCode V2 defers it; Pi drops in-flight state). The reliability gap
is entirely in *policy*, which is cheaper to fix than durability.

## Findings

Priority: **P0** discards completed work or wedges a session; **P1** wastes a
turn or blocks a run; **P2** hygiene. Evidence: **D** observed in the session
database; **S** source-confirmed; **R** reference behaviour differs. QQ paths
are under `crates/qq-core/src/` unless prefixed.

### R01 — The provider is the only retry owner, and it stops at the first event

**P0 · D/S/R.** `qq-provider/src/http.rs:19-22` (4 attempts, 500 ms → 8 s,
30 s budget), `exchange.rs:195-259` (`with_restart` never restarts once an
event was yielded), `lib.rs:1868-1875` (any `Err` from the stream →
`RuntimeEvent::Failed`). ADR-0005 chose this to stop 24× amplification; the
audit that motivated it measured sends, not completed runs. Consequence in the
data: 8 `provider_unavailable` + 4 `provider_transport` runs, 12 in total,
several after 30–56 minutes. 529 is not in `is_retryable_status` (`http.rs:234`)
but Anthropic's `overloaded_error` body maps to `Unavailable`, so it is retried
pre-stream and then exhausted in ≤ 30 s. `Retry-After` is clamped to
`max_delay` = 8 s (`http.rs:126`), so a provider asking for 20 s is retried
early and burns an attempt.

**Fix.** Add a **session-level turn recovery** owned by the run loop, distinct
from the provider's pre-stream restart, so ownership stays unambiguous: provider
owns *send* retries (unchanged), the run owns *turn* retries. On a transient
provider failure (`Unavailable`, `RateLimited`, `Transport`, stream ended
early, `ResponseIncomplete` from a disconnect) after the first event: commit
the partial assistant message as `truncated`, exclude it from the next
provider request (or keep it as a prefix and send a continue notice when the
adapter supports it), emit `RuntimeEvent::TurnRetry { attempt, delay, reason }`,
sleep with cancellation, re-issue the turn. Budget: 5 attempts per turn, reset
on any completed turn; backoff 2 s · 2ⁿ, `Retry-After` honoured up to 60 s;
connection-level failures (`Transport` before headers) retry until the run
deadline with 5 → 60 s backoff, as Codex does. Exhaustion produces
`RunOutcome::Paused { resume: TurnRetry }` (new), not `Failed`. Supersede
ADR-0005 with an ADR that names two owners by *phase*, not by error kind.

**Acceptance.** Fake provider: 529×3 then success mid-run completes the run
with 1 `TurnRetry` chain; disconnect after 3 events resumes with the partial
turn persisted and excluded; `Retry-After: 20` sleeps ≥ 20 s; cancel during
backoff settles `Cancelled` within 100 ms; run deadline during backoff settles
`BudgetExhausted`; amplification metric measured per *completed* run, not per
send.

### R02 — Later-turn overflow fails closed and wedges the session

**P0 · D/S/R.** `sessions/execution.rs:2088-2097` sets
`CompactionDisposition::BetweenRunsOnly` for every turn after the first;
`context.rs:270-273,331-334` rejects. Then the next prompt is admitted with
`Exhausted(Attempted)` if the fold already ran (`context.rs:308-317`), which is
the "already attempted" message in 8 runs that never started. The 4 bytes/token
estimate (`context.rs:9-14`) is 30–40 % high for code, so a 272 k model rejects
at ~350 k *estimated* tokens with no provider round trip.

**Fix.** (a) Mid-run compaction at a tool boundary: when `plan` returns
`Compact` on turn *n* > 1, commit a compaction marker at the run's own turn
boundary (the C2 stale-read stub already proves the store can cut inside a
run), run one bounded summarizer step, and continue the same run with the
summary + retained suffix. (b) Reactive path: a provider `ContextExceeded`
after an estimated `Send` marks occupancy full and compacts on the next turn,
as Codex does; never fail the run on the first provider overflow. (c) Admission:
when the fold is exhausted, do not reject the prompt — start the run with the
last successful summary as the sole history and a runtime notice; the user
loses detail, not the session. (d) Calibrate the estimate from the previous
turn's reported `input_tokens` when compatible (already deferred in F04; make
it a slice).

**Acceptance.** Scripted run whose tool results exceed the window three times
completes with three compaction markers inside one run; provider 413 on turn 7
recovers on turn 8; a session that hit `Exhausted` accepts the next prompt;
estimate error after calibration < 10 % on a code-heavy fixture.

*Status 2026-09-21:* (a) shipped in #92 (`322aa94`, ADR-0039) after this
snapshot: the loop compacts its own turns at a tool boundary through
`InRunCompactor`; `BetweenRunsOnly` at `execution.rs:2109-2113` is now only
the fail-closed backstop after the loop's own compaction could not help. (b),
(c), and (d) remain open as RR6/RR7.

### R03 — The tool-free checkpoint fails the run on a tool call

**P0 · D/S/R.** `lib.rs:1424-1438` removes tools when `slice_tool_calls + 16 >
256`; `lib.rs:1695-1704` fails the run if the model calls a tool anyway. Over
OpenAI-compatible routes Anthropic models keep emitting `tool_use` when the
transcript is dense with tool calls. 5 runs, 120 minutes lost, plus two
sub-agents. No reference has this construct.

**Fix.** Keep the 256-call slice as an accounting boundary, but keep tools
declared and use `tool_choice: none` where the adapter supports it; treat any
tool call on that turn as an admitted call with a rejection result ("checkpoint
turn: re-issue after the summary"), exactly the path already used for calls
past `MAX_TOOL_CALLS_PER_TURN` (`lib.rs:92-95`). If the model returns text, the
slice resets as today. Consider dropping the checkpoint entirely once R02 (a)
exists: mid-run compaction *is* the checkpoint.

**Acceptance.** Fake provider emits a tool call on the checkpoint turn → run
continues, call has a rejection result, next turn has tools; existing slice
tests still pass; 257-call fixture completes.

### R04 — Output-token stop is fatal when a tool call was open, and the default reached the model as 2 048

**P1 · D/S/R.** `providers/anthropic.rs:818` yields `Incomplete(OutputTokens)`
on `max_tokens`; but sessions persisted `max_output_tokens` 2 048/4 096 from
the pre-16 384 default (`resolved_model_json.max_output_tokens` in 4 failed
runs). `lib.rs:2010-2025` gives 3 continuations then fails
`ProviderOutputTruncated`; a truncated turn with a half-emitted tool call is
`ResponseIncomplete` from the adapter and fails immediately.

**Fix.** Treat a stored `max_output_tokens` below the model preset's floor as
unset at plan compile (`src/runtime.rs` load path). A truncated tool call is
dropped and gets a synthetic tool result "arguments were cut off at the output
limit; re-issue" (Pi `agent-loop.ts:379-404`); the run continues. After
`MAX_OUTPUT_CONTINUATIONS`, complete the run with a `truncated` notice instead
of failing; the partial answer is already in the transcript.

**Acceptance.** Session with persisted 2 048 resolves 16 384; fake `max_tokens`
mid-tool-call continues; 4th consecutive truncation completes with a notice.

### R05 — Jev enforcement converts a finished answer into a failed run

**P1 · D/S.** `lib.rs:2430-2433` and `:3138`: two RED verdicts →
`RunFailureKind::Policy`. 9 runs on 2026-09-19/20. The operator disabled it.

**Fix.** Exhausted repair completes the run with the candidate and a
`CheckpointReviewed` outcome of `Rejected`; headless callers read the outcome
field. A reviewer disagreement is evidence for the user, not a run failure.
Same for `Unavailable` at `lib.rs:2423-2429`: reviewer outage must not fail the
task run.

### R06 — Unknown slash commands and empty prompts become failed runs

**P1 · D/S/R.** `workspace/guidance.rs:85-92,125-165` and `lib.rs:1181-1204`
run inside the run loop. 8 runs; each pollutes the next prompt with
"The previous run failed". The TUI forwards any `/name` it does not own
(`qq-tui/src/app.rs:1521-1553`).

*Correction 2026-09-21 (RR2):* only 3 of the 8 are slash names. The other 5
are "conversation messages must not be empty" on ordinary prompts, both
sessions on 2026-09-11 after runs whose assistant turns were all
reasoning/tool-only; #27 (`1747435`) fixed that the next day.

**Fix.** Validate at admission (`sessions/commands.rs` SubmitPrompt): unknown
slash names and empty usable text return a typed `SessionRuntimeError` and no
run row; the TUI shows a notice with suggestions and restores the composer.
Add `/clear` as a client alias.

### R07 — Approval waits time out server-side at 300 s and deny

**P1 · D/S/R.** `sessions.rs:344`, `sessions/approvals.rs:94,209`. Not
configurable (`SessionRuntimeOptions.approval_timeout` exists but is not
plumbed from config). 4 timeouts observed; the operator was away from the
terminal. No reference times out an interactive approval.

**Fix.** Interactive sessions: no deadline (bounded by the run deadline, which
F02 already enforces); headless: deny immediately with a tool result naming the
policy, as `qq run` already has `needs_input`. Plumb the option from config for
supervisors that want a bound.

### R08 — Tool arguments are decoded with `deny_unknown_fields` and no coercion

**P1 · D/S/R.** Every built-in args struct (`tools/{search,read,edit,write,
tree,shell,fetch,ask}.rs`, `dispatch.rs:370,380`). 8 `search` failures from
stringified arrays, unknown fields, and out-of-range integers; 3 MCP failures
from absent-vs-`{}` arguments. Each costs a turn.

**Fix.** One lenient decode layer in `tools/dispatch.rs`: parse to
`serde_json::Value`; if a field expected as an array is a string that parses as
a JSON array, replace it; clamp bounded integers to their range and note it in
the result header; drop unknown fields and note them; send `{}` to MCP tools
with no arguments. Keep `deny_unknown_fields` for approval-relevant fields
(`shell.command`, `exec.program`, paths). Add a fallback for an unknown tool
name: a result, not a protocol failure.

**Acceptance.** Fixture of the 11 observed malformed calls all execute with a
note; adversarial cases (extra `command` on `exec`) still reject.

### R09 — Read-before-edit does not survive a run boundary

**P1 · D/S.** `tools/edit.rs` requires a read *in this session*; 14 refusals,
including after a reopen. `if_hash` bypasses it but models rarely know the hash.

**Fix.** Persist the read-hash ledger with the session (it already exists
in-memory as `file_state`); accept an edit when the stored hash matches the
current file. Return the current hash in the refusal so the retry is one call.

### R10 — Provider stream strictness fails runs on model quirks

**P2 · S/R.** `lib.rs:1604-1646` (reasoning block order), `:1710-1727` (empty
id, long name), `:1625-1631` (1 MiB reasoning cap). Codex logs these in
release; Pi keys blocks by index. Not yet observed in the store, but the same
LiteLLM route that produced R03 is exposed.

**Fix.** Auto-close an open reasoning block; synthesize an id for an empty one
(`call_{turn}_{ordinal}`); truncate names; turn the reasoning cap into a
display cap (stop yielding deltas, keep streaming).

### R11 — No loop detection; the only backstop is the fatal checkpoint

**P2 · S/R.** OpenCode asks on 3 identical calls; fx stops on 3 all-malformed
batches. QQ has neither, which is part of why the 256-call slice was made
fatal.

**Fix.** Track a digest of (name, arguments) for the last 3 calls; on a match,
return a tool result "identical call repeated 3×; change approach or ask the
user" instead of executing. Cheap, and it replaces the checkpoint's
runaway-loop role.

### R12 — Cost of delegation and route latency dominate wall time

**P2 · D.** `spawn_agent` 577 s mean; LiteLLM routes 3× slower per turn than
direct Bedrock Mantle. Not a defect, but the TUI shows neither per-turn
latency nor cumulative child wait, so the user experiences "too long" without
attribution.

**Fix.** Surface per-turn provider latency and child wait in `RunStats`; let
the coordinator run read-only children concurrently by default (the pool
already allows 3).

## Proposed order

| Order | Slices | Saves | Why first |
| --- | --- | --- | --- |
| 1 | R03 checkpoint, R06 admission, R05 Jev outcome | 22 runs, ~2.5 h | One-branch changes; zero design risk |
| 2 | R01 turn recovery (+ ADR superseding 0005) | 12 runs, ~4.5 h | Largest single class of lost work |
| 3 | R02 reactive overflow and un-wedging (mid-run compaction itself shipped in #92) | 9 runs, 3 sessions | Remaining design change is small: occupancy marking and admission policy |
| 4 | R04 output tokens, R07 approval, R08 argument leniency, R09 edit ledger | 5 runs + ~25 wasted turns | Turn-savers |
| 5 | R10, R11, R12 | latent | Hygiene and visibility |

Do not use the fixture suite alone as acceptance. After order 2 lands, re-run
the § Appendix queries against a week of real use and report the failure
share; the target is **harness-caused failures < 5 % of prompt runs** and
**zero wedged sessions**.

## Appendix — queries

Against a copy of the store (`cp ~/.local/share/qq/sessions.sqlite3* /tmp/`;
`sqlite3 -readonly`):

```sql
select status, kind, count(*) from runs group by 1,2;
select json_extract(outcome_json,'$.failure.kind'),
       (finished_at_ms-started_at_ms)/1000,
       json_extract(resolved_model_json,'$.route'),
       json_extract(outcome_json,'$.failure.message')
  from runs where status='failed' order by 1, 2 desc;
select name, is_error, count(*), avg(finished_at_ms-started_at_ms)/1000.0
  from tool_calls group by 1,2 order by 3 desc;
select approval_resolution, count(*),
       max(resolved_at_ms-requested_at_ms)/1000.0 from tool_calls group by 1;
with t as (select run_id, completed_at_ms,
           lag(completed_at_ms) over (partition by run_id order by turn_ordinal) prev
           from model_turns)
select count(*), avg(completed_at_ms-prev)/1000.0 from t where prev is not null;
```

No runtime code was changed by this audit. Reference trees were read, not
built or executed.
