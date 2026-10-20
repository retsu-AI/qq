# Backend performance residuals

No active implementation phase. The compiled-plan backend, extension seams,
headless contract and phases 0–6 are shipped; see
[architecture](../design/architecture.md) and [headless contract](../design/headless-contract.md).
This file retains only acceptance that has not closed. Historical receipts:
[speed-first ledger](progress/speed-first.md), [phase 5b](progress/g-phase-5b.md),
[phase 6](progress/g-phase-6.md). New qualification follows
[performance gates](progress/performance-gates.md) and the
[recording runbook](../runbooks/perf-recording.md).

## Open measurements

- Phase 5a H0 tails: retain historical baseline `1c08cef`, but use the
  current fixture/budget v5 comparability rules, not old v4 receipts.
  Quiet-host matched A/B and same-binary A/A, I/O some avg10 well under 20%.
  #315 / ENG-834 repair merged as `21332992`; maintainer accepted the known
  gap for release, **H0 remains failed/unqualified**. No budget waiver.
- H20: eight-stream mixed control/output p95 ≤20ms, then tighten executable
  service-gap budget 50→20ms. Median evidence is not tail acceptance.
  Retain 1MiB/512KiB streaming scaling ≤2.2x and default-path H0 regression
  acceptance from the phase-6 receipt.
- ENG-840: full native Windows workspace qualification is independent;
  targeted teardown CI does not establish it and it does not gate H10.

## Deferred repairs (reproduce before scheduling)

| ID | Deferred work | Reason / required contract |
| --- | --- | --- |
| H22-auth | StaticHttpAuth and header deduplication | Public provider recipe compatibility and per-protocol header safety; separate ADR/slice |
| H22-output | Headless output off Tokio worker | Preserve writer failure exit codes and stdout/stderr interleave with contract tests |
| H22-config | Parse configuration once per load | Trust/organization/merge ordering on a cold path; only change with a concrete need |
| H22-reviewer | Reviewer through PlanCache | Existing epoch cache misses config changes; behavioral invalidation fix, not mechanical cleanup |
| H22-enums | TurnMode / StreamEnd types | Revisit when changing run-loop behavior |
| H22-arguments | Parse tool arguments once | Preserve verbatim transcript bytes; revisit with run-loop change |
| H22-calls | Shared immutable ModelTurnCommit calls | Measure clone cost first; store still requires ownership |

Approval-wait sleep_until is **not pending work**: it iterates per reviewer
verdict, not per tick. Do not optimize it without new evidence.

## Demand-gated extensions

### H10 — OS process sandbox

Now planned in [sandbox](sandbox.md) (S0–S6); the acceptance below is kept
as its floor.

Requires a platform threat model and a concrete process contract. Root's
adopted lane decision makes H10 independent of T13 and full Windows
qualification; it must still use evidence to choose any new terminal
contract rather than treating sandboxing as authorization to ship T10.

Acceptance: filesystem/network/process/secret boundaries tested; shared
local/sandbox process conformance; no silent unsandboxed fallback; no process
leaks on cancellation, timeout, shutdown or recovery; optional/feature-gated
platform dependencies. Record every extension-required R6–R8 result in
[Terminal-Bench](terminal-bench.md).

### H11 — compatibility adapter

Only for a real consumer. Use qq-client/native HTTP, no parallel runtime or
direct store access. Native events stay authoritative; test/document
capability loss; product auth/tenancy stays outside core. Disabling it leaves
the base hot path unaffected.

### H12 — integrated extension qualification

Depends on H10/H11 and the R6–R8 milestones required by the shipped extension.
Re-run phase 5–6 baselines and enforce measured gains in executable budgets.
Matrix: cold/warm direct/TUI/server; 1/10/100 sessions; long text/reasoning;
pre-send and ambiguous post-send provider failures; saturated/disk-full/
corrupt/migrated/restarted store; client reconnect during text/tool/approval/
terminal; addon discovery/refresh/crash/timeout/overload/shutdown; slow/stale
context sources; MCP/embedded conformance; cancellation at provider/tool/
child boundaries; terminal cleanup and sandbox escapes; same-model quality;
minimal/full binary, RSS, startup and latency. Paid benchmark success alone
does not close crash/security qualification.
