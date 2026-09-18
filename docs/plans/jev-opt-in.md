# Optional Jev decisions and review repairs

User direction (2026-09-18): implement the recommendations in
[`jev-runtime-review-2026-09-18.md`](../design/jev-runtime-review-2026-09-18.md)
as a stacked PR, keep the design small, move promptly and update docs with code.
Base PR: #72, `feat/jev-runtime-checkpoints` at `dc59d14`.
Owning issue: ENG-791 (connector reauthentication currently prevents live read).
The user's explicit stack instruction overrides the workflow's merged-base rule.

## Status and acceptance map

| Slice | Deliverable | Required evidence | State |
| --- | --- | --- | --- |
| J1 | Preserve queued and interrupting steering across final review | Runtime regression; no completion using obsolete task | In progress |
| J2 | Default-off trusted configuration, setup/status/off, independent review/routing choices | Config/runtime/CLI tests, stored credential does not enable; child and reload identity | Planned |
| J3 | Bounded evidence selection from effective task/history; remove ineffective cache; retain tool batching for selective review | Long-run, continuation, steering, no-tool, off batching tests | Planned |
| J4 | Bounded cancellable reviewer requests, usage accounting, spend admission and finite repair budget | Budget, timeout, cancellation, persistence, body overflow regressions | Planned |
| J5 | Mask every outgoing evidence field; narrow actionable questions and explicit uncertainty | HTTP adapter contract tests, failure criterion IDs, masking tests | Planned |
| J6 | Independent optional authorized root/child model/effort routing with pinned-choice precedence | Dispatch, capability, fallback, cancellation, spend and durable identity tests | Planned |
| J7 | Advisory observation plus visible effective/pending/outcome/spend state | Observer and client/headless replay tests; does not gate authoritative completion | Planned |
| J8 | Evidence-led qualification and current docs | Baseline/candidate off-path comparison, mode workload evaluation, full workspace gates and independent reviews | Planned |
| J9 | Stacked PR and accurate tracker/delivery state | PR targets #72 branch, owned commits, checks inspected; no self-merge | Planned |

The concrete scope is the Jev review's repairs, optional-mode contract, routing
and qualification. The reference audit's broader product hypotheses remain
tracked roadmap work, not new terminal/browser/sandbox products in this stack.
No speed or quality claim will be made from fake-model measurements. Live paired
evaluation requires available authorized credentials and an explicit spend cap.

## Agreed test boundaries

The accepted review's acceptance list defines the seams: resolved configuration
and command parsing; public runtime/session commands and durable events; provider
HTTP requests; client reducer/headless output; and existing performance fixtures.
Use deterministic providers/reviewers only at external inference boundaries.
Keep one failing behavior test followed by its fix; no generic plugin framework,
additional crate, automatic global activation or separate agent implementation.

## Design choices

Reuse configuration layering and provenance, compiled profile identity,
existing run accounting, post-commit observers and provider effort transport.
Separate credential storage from activation. Resolve modes before credentials,
freeze decisions for a run, preserve explicit user choices and off overrides.
Bound requests, evidence, responses, retries, tasks and spend. Jev never authorizes
side effects and never substitutes for executed tests or authentic source data.

Ledger: [`progress/jev-opt-in.md`](progress/jev-opt-in.md).
