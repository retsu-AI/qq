# Supervised Delegation, Continuation, And Audit

Status: D1–D5 and D6a shipped (2026-09-03; the H23/H24 ownership and budget
repairs shipped in speed-first Phase 5a, `1e6a901`/`f482b37`). Their
contracts are in [`architecture.md`](../design/architecture.md) (continuation
on truncation, the delegation roster and depth ceiling, supervised write
children, the final-answer audit) and [`protocol.md`](../design/protocol.md);
their design text is in Git history (`git log -- docs/plans/supervised-delegation.md`
before `#66`). Open: D6b — the paid paired runs whose arm overlays and runbook
are in `benchmarks/arms/`, and the delegation-depth and worker-model defaults
they decide. Ledger:
[`progress/supervised-delegation.md`](./progress/supervised-delegation.md).

## Decision Summary

> Continue truncated turns from persisted partial output; attenuate authority
> and budget at every child boundary; adjudicate write-level child actions with
> a bounded, budgeted reviewer; audit root answers only when enabled (default `off`); and
> promote nothing to default-on without a paired win.

Design constraints inherited from the harness plan and `AGENTS.md`:

- one runtime; children, reviewers, and auditors are ordinary runs or bounded
  provider calls through the existing seams, never a second loop;
- persist before publish; a continuation, review verdict, or audit verdict is
  durable before any client sees it;
- every new dimension is bounded: continuations per run, children per run,
  descendants per tree, depth, write children per run, reviewer and auditor
  tokens/time/cost, roster size, schema and prompt bytes;
- the default hot path (no children, no truncation, audit not triggered) may
  not regress more than the accepted gate; and
- no application configuration type enters `qq-core`.

## D6 — Paired Evaluation

Owner: `xtask`, `benchmarks/harbor`, `benchmarks/arms`, `qq-protocol`.

D6a (done) built the instrument; D6b is the paid measurement and the default
decisions it feeds. Nothing in D6b is code: the remaining work is operator
time, credentials, and spend on a host with Python and Harbor 0.20.0.

Instrument (D6a, done):

- `cargo xtask eval compare --baseline JOB --candidate JOB`: per-task paired
  pass outcomes, an exact two-sided McNemar on discordant pairs, a seeded
  percentile bootstrap on the dollars-per-pass ratio, and the scorecard delta.
  Refuses jobs that differ in model route, organization, output limit, context
  window, approval, run limits, prompt version, instruction hash, workspace
  identity, machine class, or Harbor configuration, and arms sharing a label;
  tolerates and lists the arm label, QQ version and revision, system-prompt
  and tool-schema hashes, and guidance.
- `eval run --arm LABEL` stamps `QQ_EVAL_ARM` on every trial record; arm
  configuration travels as `QQ_CONFIG_CONTENT` through the Harbor adapter.
- `TokenUsage.reasoning_tokens` costs reasoning-heavy arms truthfully.
- Per-trial rows carry cost, tokens, wall time, reasoning tokens, child count,
  continuation count, and the truncated-failure flag.
- `delegation.max_depth: 0` is the A0 control: the root itself is never
  offered `spawn_agent`.
- `benchmarks/arms/*.ron` are the six arm overlays, validated by a
  configuration test; `benchmarks/arms/README.md` is the runbook.

D6b runbook (not started; requires spend):

1. Choose the task subset: 30–40 Terminal-Bench 2 tasks stratified by shape
   (breadth-heavy research, depth-heavy implementation, long-output), plus a
   long-output subset for T1. Record the exact `--include-task-name` list in
   `benchmarks/arms/README.md` so every arm uses it verbatim.
2. Fill the `PROVIDER/...` placeholders in each overlay with authenticated
   routes; the primary route must equal `--model` on every arm.
3. Run the deterministic pre-checks listed in the runbook (scripted-provider
   tests for every arm's events, accounting, and ATIF conversion). They cost
   nothing and must pass before the first paid trial.
4. Run A0, A1, A2, A3, B1, and C1 with `--n-attempts 3` (the three seeds),
   identical `--timeout-seconds`, `--max-turns`, `--max-cost-usd`, and
   `--machine-class`, one job per arm. Classify every non-passing trial.
5. Run T1 as two jobs on the long-output subset: the commit before `e074f89`
   and `main`, both arm A0. `compare` tolerates the revision difference.
6. Compare each arm against A0 (and A3 against A2) and record the JSON
   outputs under `benchmarks/arms/results/`.
7. Apply the gates below by editing defaults and this plan's status, then
   record the receipts in `progress/terminal-bench.md` R7.

Gates: delegation defaults per task class follow R7 — the candidate's
`cost_per_pass_ratio_ci95_high` below 0.80 with no meaningful pass-rate loss
(`delta.pass_rate` not below zero beyond `mcnemar_p_value` noise). Audit
defaults to `off` since context-usability C3 (2026-09-16): every default run
was paying a second unbounded agent loop. B1 may flip it back to `heuristic`
only if it raises pass rate meaningfully or lowers dollars per pass. Depth above one stays opt-in
unless A3 beats A2 on the same gate. Continuation is a correctness fix and
needs no gate beyond T1 showing no pass-rate loss.

Not measured yet, and honestly uncertain: whether any delegation arm beats A0
at all. Published results favor breadth-shaped work at several times the
token cost and penalize depth-shaped work; the stratified subset exists so the
answer can differ per task class rather than average to nothing.

## Sequence

| Task | Status |
| --- | --- |
| D6a compare command, arm stamping, reasoning tokens | done |
| D1 continuation | done; protocol 16, schema 22 |
| D2 accounting and authority repair | done |
| D3 roster | done; descriptor 4, prompt 10 |
| D4a supervised write children at depth one; D4b configurable depth to three | done; schema 23 |
| D5 audit | done; schema 24. Default flipped to `off` by context-usability C3 (#59) pending B1 |
| D6b paired runs and gate decisions | not started: needs spend; runbook in `benchmarks/arms/README.md` |
