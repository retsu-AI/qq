# Model-Reviewed Tool Approvals

Status: implemented 2026-08-06 (protocol variants, core seam + gate race,
config `reviewer_model`, composition-root reviewer). Not part of the
terminal-bench repair tranche.

## Problem

`Auto` mode (default since `34b8e93`) executes ordinary work without
prompting, but every call its static danger classifier flags — recursive
deletion, privilege escalation, force-push, piped installers, `dd`,
kill/shutdown shapes — blocks on a human yes/no. Many of those calls are
contextually safe (`rm -rf target/`, `kill` of a process the run started).
Humans are the most expensive and slowest approvers; headless runs deny by
timeout. Claude Code resolves this middle bucket with a small, fast model.

## Design

A reviewer adjudicates only the calls that would otherwise prompt a human.
Static policy stays first and last:

1. `approval::evaluate` runs unchanged. `Execute` and `Deny` are untouched.
2. On `RequireApproval` in `Auto` mode, when a reviewer is configured, the
   gate persists the approval request (clients still see it), then asks the
   reviewer concurrently with the human wait.
3. Reviewer verdict `Approve` resolves the approval durably and the call
   executes. `Deny`, `Escalate`, timeout, or any reviewer error leaves the
   request waiting for the human exactly as today. Fail-safe direction is
   always toward `RequireApproval`, never toward `Execute`.
4. A human response that lands first wins; the existing resolution
   idempotency already arbitrates the race.

`Ask` and `ReadOnly` modes never consult the reviewer. `Full` never needs it.

### Injection surface

The reviewer sees only: tool name, classified shell command and cwd or edit
preview, workspace path, and the session's recorded grants. It never sees
the transcript, so a poisoned context cannot argue its own call safe.

### Seams

- `qq-protocol`: `ApprovalResolution::{ApprovedByReviewer, DeniedByReviewer}`
  (additive; wire is snake_case tagged). `ToolApprovalResolved` events carry
  them unchanged.
- `qq-core`: new `ApprovalReviewer` trait (mirrors `WorkspaceGrantAuthority`),
  injected via `SessionRuntimeOptions`. `ReviewRequest` → `ReviewVerdict
  {Approve, Escalate{reason}, Deny{reason}}`. The gate in
  `sessions/approvals.rs` races reviewer, client, cancellation, and timeout.
  A store path resolves the approval by reviewer inside one transaction,
  losing gracefully if a client resolution already committed.
- Composition root (`src/runtime.rs`): `ModelApprovalReviewer` implements the
  trait with one bounded, non-streaming provider call through the existing
  `qq-provider` path. Config key `reviewer_model: Option<ModelRoute>`
  (precedent: `worker_model`). No reviewer configured → trait absent →
  behavior identical to today.
- Verdict prompt demands a strict one-line JSON verdict; anything else is
  `Escalate`. Reviewer call budget: one attempt, hard timeout (default 10s),
  bounded output tokens.

### Durability

The reviewer's verdict, model route, and bounded rationale persist in the
approval resolution transaction before the call executes. A failed write
denies the auto-approval (the human path remains open).

## Delivery

1. `feat(protocol)`: reviewer resolution variants + serde tests.
2. `feat(core)`: `ApprovalReviewer` seam, gate race, store resolution path,
   regression tests (reviewer approve, deny-falls-to-human, error-falls-to-
   human, client-wins-race, cancellation).
3. `feat(config)`: `reviewer_model` key, provenance, validation.
4. `feat(cli)`: wire `ModelApprovalReviewer` in the composition root; headless
   `qq run` gains the same safe middle ground between `auto` and `full`.

Each step lands independently; the feature activates only when
`reviewer_model` is configured.
