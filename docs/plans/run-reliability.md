# Run Reliability: sessions finish

## Status

| | |
| --- | --- |
| Now | RR1 in review on `feat/rr1-checkpoint-tolerance` (stacked on the plan PR). Next: RR2 |
| Shipped | — |
| Open | RR1–RR12 |
| Ledger | [`progress/run-reliability.md`](./progress/run-reliability.md) |

## Goal

Make QQ the operator's primary development harness by removing every case
where the harness itself ends a run that the model or provider could have
continued. Measured on the live session store (queries in the audit's
appendix):

- Harness-caused failures (`policy`, `provider_protocol`, `invalid_command`,
  retry-exhausted `provider_unavailable`/`provider_transport`,
  `provider_output_truncated`) **< 5 % of prompt runs**, down from 20 %.
- **Zero wedged sessions**: a session that failed one prompt accepts the next.
- Tool-contract failures that cost a model turn (argument shape, unknown
  field, read-before-edit across runs) **< 0.5 % of tool calls**, from 0.6 %.
- No completed-run regression in amplification: mean provider sends per
  *completed* run may rise; sends per *failed* run is the number that must
  fall.

Speed follows: every failed 50-minute run is 50 minutes of wall time and
tokens with no result.

## Non-Goals

- No change to persistence, settlement, or persist-before-publish (ADR-0002,
  0003, 0012). Recovery is a policy layer on top of the existing durable turn.
- No generic hook/plugin system for retries. One typed recovery path in the
  run loop.
- No OS sandbox, terminal tool, or new providers here.
- No relaxation of `Forbidden`, path containment, or approval classes.

## Principles

1. **The unit of failure is a turn, not a run.** A provider or model fault
   commits what arrived, records why, and the run decides whether to retry,
   continue, pause, or complete. `Failed` is reserved for configuration,
   authentication, and invariant violations inside QQ.
2. **Bounds are enforced by what QQ sends next**, never by declaring the
   model's reply illegal. A cap produces a rejection *tool result*; a
   checkpoint keeps tools declared; a review verdict is an outcome field.
3. **Paused is a real outcome.** Retry exhaustion and unanswered approvals
   leave a resumable run, not a terminal one.
4. **Coerce, note, continue.** Tool argument leniency for read-only fields;
   strictness only where the field carries authority.
5. **Nothing wedges.** Admission may downgrade a prompt's context; it may not
   refuse it because an earlier run overflowed.

## Task index

| ID | Goal | Audit | Owned paths | Acceptance |
| --- | --- | --- | --- | --- |
| RR1 | Checkpoint turn keeps tools declared; a tool call on it becomes a rejection result | R03 | `crates/qq-core/src/lib.rs` (slice/checkpoint block) | Fake provider tool call on checkpoint → run continues; 257-call fixture completes; existing slice tests green |
| RR2 | Slash-command and empty-prompt validation at admission; TUI notice; `/clear` alias | R06 | `crates/qq-core/src/sessions/commands.rs`, `workspace/guidance.rs`, `crates/qq-tui/src/app.rs`, `commands.rs` | `/nope` returns typed error and no run row; TUI shows notice; `lib.rs:4088` table case moves to admission |
| RR3 | Jev RED exhaustion and reviewer outage complete the run with a `Rejected`/`Unavailable` checkpoint outcome | R05 | `crates/qq-core/src/lib.rs` (checkpoint blocks), `qq-protocol` outcome docs | Two RED verdicts → `Completed` + outcome; headless JSONL golden updated |
| RR4 | Turn-level recovery: transient provider failure after the first event commits the partial turn and retries the turn; 5 per turn, reset on success; connection failures retry to the run deadline; exhaustion → `Paused`; `TurnRetry` event; ADR-0040 superseding 0005 | R01 | `crates/qq-core/src/lib.rs` run loop, `sessions/{execution,settlement}.rs`, `qq-protocol` (`RunOutcome::Paused`, `TurnRetry`), `qq-client::state`, TUI status | Fake 529×3 mid-run completes; disconnect after 3 events resumes; `Retry-After: 20` honoured; cancel/deadline during backoff settle correctly; amplification measured per completed run |
| RR5 | `Retry-After` honoured above `max_delay` (≤ 60 s); 529 in `is_retryable_status`; HTTP-date form parsed | R01 | `crates/qq-provider/src/http.rs` | Fake-clock tests; minimal provider profile green |
| RR6 | Reactive overflow: provider `ContextExceeded` marks occupancy full and the loop's in-run compaction (#92) runs on the next turn; exhausted fold admits with summary-only history instead of rejecting | R02 (b), (c) | `crates/qq-core/src/sessions/{execution,context}.rs`, `lib.rs` plan step | 413 on turn 7 recovers on turn 8; `Exhausted` session accepts next prompt; independent review required (touches `sessions/`) |
| RR7 | Estimate calibration from previous turn's reported input tokens | R02 | `sessions/context.rs`, `execution.rs` | Error < 10 % on code-heavy fixture; no change when usage incompatible |
| RR8 | Output-token handling: persisted `max_output_tokens` below preset floor treated as unset; truncated tool call → synthetic re-issue result; continuation exhaustion completes with notice | R04 | `src/runtime.rs` load path, `crates/qq-core/src/lib.rs` continuation block, adapters' incomplete mapping | Session with 2 048 resolves 16 384; `max_tokens` mid-tool-call continues; 4th truncation completes |
| RR9 | Approval: no server deadline for interactive sessions; immediate deny-as-result headless; option plumbed from config | R07 | `sessions.rs`, `sessions/approvals.rs`, `src/runtime.rs`, `qq-config` | 2 s option test; interactive wait bounded only by run deadline; headless denial result text |
| RR10 | Lenient tool-argument decode: stringified arrays, clamped integers, dropped unknown fields (noted); `{}` for MCP no-arg tools; unknown tool name → result | R08 | `crates/qq-core/src/tools/dispatch.rs`, `tools/search.rs`, `src/mcp.rs` | 11 observed malformed calls execute with a note; `exec` extra `command` still rejects |
| RR11 | Persist the read-hash ledger with the session; edit accepted when stored hash matches | R09 | `crates/qq-core/src/tools/edit.rs`, `sessions/{store,transcript}.rs` (schema bump) | Read in run 1, edit in run 2 succeeds; file changed → refusal carries current hash |
| RR12 | Stream leniency (auto-close reasoning, synthesize ids, display cap for reasoning) and 3-identical-call loop result; per-turn latency in `RunStats` | R10, R11, R12 | `crates/qq-core/src/lib.rs`, `qq-protocol` stats | Fixtures for each quirk; loop fixture returns result not execution |

Order: RR1–RR3 (one PR each, no design risk) → RR4+RR5 → RR6+RR7 → RR8–RR11
→ RR12. RR4 and RR6 require independent review per `workflow.md` § 4.

## Design notes

### Turn recovery (RR4)

The provider keeps ownership of *send* retries while no event has been
yielded (ADR-0005 unchanged for that phase). Once an event has been yielded
the provider returns the error as today; the **run loop** owns what happens
next:

```text
provider Err(kind) after ≥1 event
  ├─ kind ∈ {Unavailable, RateLimited, Transport, Response(ended early)}
  │    commit partial assistant turn (truncated=true, recovery=Some(reason))
  │    attempts_this_turn += 1
  │    attempts ≤ 5 → emit TurnRetry{attempt, delay, reason}; sleep(select cancel, deadline); re-issue turn
  │    attempts > 5 → settle Paused{resume: TurnRetry}
  └─ otherwise → Failed (unchanged)
```

The retried request excludes the partial assistant message unless the adapter
supports assistant-prefix continuation (Anthropic, Bedrock), in which case it
is kept and `OUTPUT_TRUNCATED_CONTINUE_NOTICE` follows. Attempt count resets
on a completed turn so a 3-hour run survives many isolated blips. Pre-header
connection failures use 5 → 60 s backoff to the run deadline (F02 already
enforces the deadline across sleeps). `Paused` is a new `RunOutcome` and
session status; `ResumeRun` re-admits it as a continuation without a new
prompt. Protocol version bump; headless golden updated.

### Reactive overflow and un-wedging (RR6)

Mid-run compaction at a tool boundary shipped as F03 (#92, ADR-0039) while
this plan was being written: the loop hands its own turns to an
`InRunCompactor` when the C2 prune still overflows, and `BetweenRunsOnly` is
now only the fail-closed backstop. RR6 keeps the two remaining parts of R02.
A provider `ContextExceeded` after an estimated `Send` sets
`context_occupancy` to the window so the next turn takes the in-run
compaction path instead of failing the run on the first provider overflow. At
admission, `Exhausted(Attempted)` starts the run with the last marker as sole
history and a runtime notice rather than rejecting the prompt.

### Checkpoint (RR1)

Tools stay declared on the checkpoint turn; adapters that support
`tool_choice: none` set it. A tool call on that turn is admitted with the
rejection text already used for over-cap calls and the turn is re-run as a
continuation. With in-run compaction shipped (#92), evaluate deleting the
checkpoint after RR1: compaction is the durable checkpoint, and RR12's loop
result covers the runaway case.

## Acceptance for the plan

1. Every RR slice's tests green; workspace gates green; minimal provider
   profile green for RR5/RR8.
2. One week of real use after RR1–RR6: audit appendix queries show harness-
   caused failures < 5 % of prompt runs and zero `Exhausted` admissions.
3. `docs/design/architecture.md` § run loop and `tools.md` § Loop Bounds
   amended; ADR-0040 (two-phase retry ownership) accepted;
   `headless-contract.md` documents `Paused`.
4. The plan is deleted and this section's durable content moves to `design/`.
