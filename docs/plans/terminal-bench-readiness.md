# QQ Harness Reliability, Cost, And Terminal-Bench Readiness

Status: Phases 1–5 shipped and qualified (Phase 4 2026-09-01, Phase 5
2026-09-02); their contracts are in `docs/design/` (`headless-contract.md`,
`architecture.md` § Runtime, § Provider Compilation, § Persistence;
`tools.md` § Context Budget, § Workspace Instructions) and their receipts in
[`progress/terminal-bench.md`](./progress/terminal-bench.md) and Git history
(`git log -- docs/plans/terminal-bench-readiness.md` before `#66`). Phase 6's
candidate designs shipped through [`tool-layer.md`](./tool-layer.md) T2/T5,
with the persistent terminal (T10) gated on the T13 ablation this plan's
method owns. Open: R6-terminal evidence (T13), R7, R8, and the paid
Terminal-Bench baseline (`TB-pilot`).

This plan makes QQ a trustworthy autonomous terminal harness before
optimizing it against Terminal-Bench. The public benchmark is an evaluation
target, not the product architecture: every improvement must also make
ordinary local, server, and remote sessions more correct, faster, or cheaper.

## Goal

QQ should maximize **verified successful work per dollar and per minute**.
Raw process speed matters, but it is subordinate to completing the task
correctly.

The priority order is:

1. Fixed-model verified task success.
2. Dollars and uncached tokens per successful task.
3. Wall-clock time per successful task.
4. Harness reliability and variance.
5. Local startup, streaming, persistence, replay, and rendering overhead.

The implementation is successful when:

- A non-interactive `qq run` exercises the same durable `SessionRuntime` as
  the TUI and server.
- Every committed turn remains valid future model context across completion,
  failure, cancellation, interruption, restart, and compaction.
- Long model and reasoning streams have linear persistence cost and remain
  visible without starving other sessions.
- Context budgeting uses the selected model's effective limits and reported
  usage rather than a global byte threshold alone.
- Every evaluation trial produces a complete replayable QQ trace and a valid
  Agent Trajectory Interchange Format (ATIF) trajectory.
- QQ can be compared against another harness with the same model, effort,
  resources, timeouts, and task set.
- Changes are promoted by paired evidence: a statistically meaningful
  capability gain, or equal capability at materially lower cost or latency.

## Measurement Contract

### Trial Identity

Every headless trial must record:

- QQ version and source revision.
- System-prompt version and content hash.
- Tool declaration version or stable schema hash.
- Effective provider/model route.
- Reasoning effort or other benchmark-relevant generation settings.
- Effective output and context limits.
- Pricing provenance.
- Workspace identity without leaking host-only secrets.
- Benchmark dataset, task, trial, seed, machine class, timeout, and resource
  configuration when the Harbor adapter supplies them.

Historical runs must remain explainable after configuration changes.

### Per-Run And Per-Turn Measurements

Record monotonic durations or timestamp pairs for:

- Command received and durably acknowledged.
- Run queued, claimed, runtime ready, and finished.
- Model request begun, first semantic provider event, and model turn completed.
- Tool requested, started, and finished.
- Cancellation requested and observed.
- Compaction started and committed.

Record these counters:

- Fresh input, cache-read input, cache-write input, reasoning when the provider
  reports it, and output tokens.
- Estimated and reported cost, with unknown cost represented as unknown.
- Model turns, tool calls, failed tool calls, retries, compactions, and
  sub-agents.
- Context occupancy and reserved output budget per turn.
- Queue wait and persistence delay.
- Peak resident memory and process CPU for evaluation jobs when available from
  the outer benchmark runner.

Do not introduce a generic telemetry framework before there is a second real
consumer. The first implementation should persist concrete run/turn fields and
emit the headless JSONL form. The Harbor adapter and `xtask` evaluation report
are the two consumers that justify a stable trace projection.

### Initial Local SLOs

These are engineering budgets, not promises. Recalibrate them only after a
repeatable baseline demonstrates that a different threshold is necessary.

| Signal | Initial target |
| --- | ---: |
| Local durable command acknowledgement | p95 <= 10 ms |
| Warm claimed run to provider send, excluding queue/provider latency | p95 <= 25 ms |
| Provider semantic delta to durable event | p95 <= 15 ms; p99 <= 40 ms |
| Provider delta to visible TUI update | p95 <= 25 ms; p99 <= 60 ms |
| Cancellation observed by active model/tool work | p95 <= 100 ms |
| One MiB stream persistence | Doubling bytes costs no more than 2.2x wall time |
| Eight simultaneous output streams | No stream starved by persistence for more than 50 ms |
| One MiB request plus 32 tool schemas encoded | p95 <= 10 ms; temporary heap <= 2x encoded body |
| Context overflow sent to a provider | Zero |
| Compaction shrinkage | At least 8x tokens with required-fact tests passing |
| Stable-prefix cache use after turn two, where supported | At least 80% |
| Retry amplification outside incidents | Fewer than 1.05 HTTP sends per logical turn |
| Harness-caused benchmark failure | Below 0.5% |

### Evaluation Scorecard

Publish these together for every experiment:

- Pass rate or normalized reward with confidence interval.
- Dollars per attempted task and dollars per passed task.
- Total and uncached tokens per passed task.
- Median and p95 wall time per passed task.
- Harness/infrastructure failure rate.
- Failure category counts.

The first competitive release gate is:

1. Remain within one percentage point of the strongest comparable harness
   under the same model and settings.
2. Then beat its dollars-per-pass or successful-task wall time by at least
   20-25%, or produce a statistically meaningful pass-rate lead.

Do not hide a capability regression behind a cheaper average.

## Shipped Phases

| Phase | Shipped | Contract now lives in |
| --- | --- | --- |
| 1 — Durable autonomous headless run (`qq run`, JSONL, ATIF, Harbor adapter) | `f7b34a1`, `DEV-726`, `DEV-730` | `headless-contract.md`; `benchmarks/harbor/README.md` |
| 2 — Authoritative context projection; atomic child runs | `fec1e0a`, `681ee07` | `architecture.md` § Runtime (projection, runtime notices), § Concurrency (child ownership) |
| 3 — Completion contract; workspace instructions; slash commands and skills | `f8847d2`, `DEV-731`, `DEV-725` | `tools.md` § Agent Instructions; `architecture.md` § Agent Packs, slice checkpoints |
| 4 — Linear and fair durable streaming | 2026-09-01 (R4) | `architecture.md` § Persistence (append-only chunks, capacity accounting, group commit, fairness) |
| 5 — Resolved model, context planning, compaction hardening, run budgets | 2026-09-02 (R5) | `architecture.md` § Provider Compilation (resolved model, occupancy reuse), § run loop step 3; `tools.md` § Context Budget (pruning, compaction bounds, `search_history`); `protocol.md` (`RunLimits`, `budget_exhausted`) |

## Phase 6: Tool-Contract Tournament And Terminal Sessions

Priority: P1. Use benchmark evidence; do not ship every candidate.

The concrete candidate designs (search, patch/edit batch, persistent
terminal) and their delivery slices now live in
[`tool-layer.md`](./tool-layer.md) (T2, T5, T10). This section keeps the
experiment method and acceptance targets, which that plan inherits.

### Experiment Method

For each candidate tool contract:

1. Add deterministic adversarial fixtures.
2. Run paired agent evaluations with the same model and prompt.
3. Measure pass rate, tool turns, failed calls, input tokens, cost, and time.
4. Keep the candidate only if it improves capability or materially improves
   efficiency without a capability loss.
5. Remove experimental code and schemas for rejected variants.

### Candidates

The search and patch candidates shipped as `tool-layer.md` T2 (`search` v2,
`tree`) and T5 (`edit_file` v2 with the matching cascade); their contracts are
in `tools.md` §§ Read-Side Walk and Edit Semantics. The persistent terminal
candidate is `tool-layer.md` D6 / T10 and ships only if the T13 ablation and
R6-terminal trajectories show stdin, background-service, or interactive
failures the one-shot `shell` and `exec` cannot cover. The paired evaluation
that decides all three is `tool-layer.md` T13, run with this section's method.

### Acceptance

- Internal tool-contract failures stay below 1%.
- Tool changes reduce median discovery/edit turns by at least 25% or produce a
  statistically meaningful task-success gain.
- No tool introduces unbounded state or weakens workspace containment.
- Terminal cancellation leaves no descendants.

## Phase 7: Sub-Agent Economics And Provider-Aware Scheduling

Priority: P1.

Complete the pending sub-agent work (the read-only sub-agent plan's Phases
A–C shipped; that plan was removed 2026-09-04):

- Add a configured worker-model selection with parent fallback.
- Persist the child's resolved model independently.
- Roll child usage and cost into parent totals without double-counting the
  child session display.
- Include child trajectories in headless and ATIF output.
- Record child queue wait and concurrency.
- Depth is configurable up to a ceiling of three, default one until the A3
  arm in `supervised-delegation.md` wins; write authority is limited to
  serialized `Supervised` depth-one children (amended 2026-09-03).

Evaluate delegation by task shape:

- Breadth-shaped repository research.
- Several independent questions.
- Depth-shaped single-file work that should remain inline.
- Tasks where worker-model errors force expensive parent recovery.

The promotion gate is at least 20% lower dollars-per-pass on the task class
with no statistically meaningful pass-rate loss. Disable delegation by
default for classes where the ablation loses.

Add provider-aware scheduling only after telemetry shows pressure:

- Per-provider concurrent request bounds.
- Rate and token budgets.
- Fair root/child scheduling.
- Retry-attempt visibility.
- Cancellation of queued child work.

Do not add automatic swarms or worktree orchestration in this plan. Editing
sub-agents are permitted only as one serialized `Supervised` child per run
(`supervised-delegation.md` D4, amended 2026-09-03).

## Phase 8: Warm-Path Runtime And Request Efficiency

Priority: P2. Start only after end-to-end measurements identify the cost.

Hand-off recorded 2026-09-04: the retry-exposure, shared-message-storage, and
request-encoding-benchmark candidates below are implemented by H14 (D3) and
H18 (D5) in
[`speed-first-extensible-agent-harness.md`](./speed-first-extensible-agent-harness.md)
after its audit confirmed nested provider/core retries (up to 24 sends per
turn) and a full history clone per attempt. This phase keeps the remaining
candidates.

Candidate work:

- Cache effective configuration and compiled runtimes by validated generation
  or digest so each claimed run does not repeat unchanged resolution work.
- Cache valid credential leases in memory with expiry and single-flight
  refresh; never log or persist secret material in cache keys.
- Invalidate runtimes when configuration, credentials, MCP declarations, or
  organization selection changes.
- Share immutable message/tool storage where ownership is genuinely shared.
- Bound total MCP tool count and schema bytes.
- Select only concretely relevant tools when a measured use case justifies
  selection; do not introduce a generic tool router in advance.
- Keep serialized stable prefixes deterministic for provider caching.
- Benchmark realistic request encoding and temporary allocation.
- Expose retries and distinguish safe pre-stream retries from ambiguous
  failures that could duplicate spend. Use provider idempotency support where
  available.

Acceptance requires end-to-end improvement. Provider-recipe nanoseconds alone
do not justify complexity.

## Benchmark Program

### Evaluation Layers

1. **Per-PR deterministic tests**
   - Fake providers, temporary workspaces, temporary SQLite, crash injection,
     context projection, tool contracts, and performance regression fixtures.
2. **Private shadow suite**
   - Generic tasks covering repository discovery, editing, build/test repair,
     background processes, interactive input, data transformation, recovery,
     and long context.
   - Tasks must not copy public benchmark solutions or encode task-specific
     hints.
3. **Terminal-Bench development runs**
   - Small infrastructure smoke selection while the adapter stabilizes.
   - Full current dataset with one repetition at implementation milestones.
4. **Submission runs**
   - The exact official dataset command, resources, timeouts, and required
     repetition count.
   - Current Terminal-Bench 2.1 submissions use `k=5`; verify the official
     contract again before every submission.

### Comparison Discipline

- Compare QQ and another harness with the same model, reasoning effort,
  provider route, task timeout, resources, and machine class.
- Use paired per-task results and report confidence intervals.
- Separate model changes from harness changes.
- Run at least three seeds for internal comparisons when the benchmark
  contract does not already require more.
- Store the QQ revision, prompt version, tool hash, and full configuration with
  every result.
- Never choose only the tasks improved by a change.

### Failure Taxonomy

Every failed trial receives one primary category:

- Task misunderstanding.
- Workspace/instruction discovery.
- Missing or irrelevant evidence.
- Tool contract or tool misuse.
- Incorrect mutation.
- Dependency/environment failure.
- Verification omitted.
- Verification failed and recovery stopped.
- Repeated-work/stall loop.
- Context loss or compaction loss.
- Provider/authentication/rate failure.
- Timeout or budget exhaustion.
- Persistence/replay/harness failure.
- Benchmark infrastructure or invalid task.

Retain the trajectory link and supporting event/tool identifiers. Do not infer
the category from final text alone.

### Integrity

- Do not alter benchmark task timeouts or resources.
- Do not provide task-specific instructions, encrypted solutions, test
  contents unavailable to other agents, or internet-retrieved solutions.
- Use the same generic QQ prompt and tool contracts across the dataset.
- Produce ATIF for every passing trial and run the published trajectory judge
  when available.
- Keep benchmark adapters auditable and separate from task execution logic.

Official references:

- [Terminal-Bench 2.1 leaderboard](https://www.tbench.ai/leaderboard/terminal-bench/2.1)
- [Terminal-Bench 2.1 release](https://www.tbench.ai/news/terminal-bench-2-1)
- [Harbor custom-agent documentation](https://www.harborframework.com/docs/agents)
- [ATIF documentation](https://www.harborframework.com/docs/agents/trajectory-format)
- [Terminal-Bench leaderboard integrity policy](https://www.tbench.ai/news/leaderboard-integrity-update)

## Non-Goals

- Remote instance discovery, multi-machine workspace selection, or mobile UI.
- Distributed scheduling or hosted coordination.
- Editing sub-agents or automatic agent swarms.
- Run snapshots or generalized undo beyond the separate
  `docs/plans/run-snapshots.md`.
- A plugin interface, public tool registry, or alternate protocol.
- Replacing SQLite without measurements that show it remains the bottleneck
  after linearization.
- Model-specific branches in the core agent loop.
- Mid-stream provider replay without a provider-supported idempotent contract.
- Benchmark-specific prompts, task heuristics, or resource changes.
- Broad TUI restructuring unrelated to durable event projection.

The HTTP/SSE protocol, durable store, and shared runtime remain the foundation
for future remote and mobile clients. This plan strengthens that foundation;
it does not pre-build those clients.
