# Delegated Approval

## Status

| | |
| --- | --- |
| Now | DA6 in review (`feat/eng-862-da6-delegate-surfaces`): the last slice. When it merges this plan is done and is deleted; the ledger remains |
| Shipped | #125 grant storage rule; DA1 [#133](https://github.com/retsu-AI/qq/pull/133); DA4 [#135](https://github.com/retsu-AI/qq/pull/135); DA3 [#143](https://github.com/retsu-AI/qq/pull/143); DA5 [#144](https://github.com/retsu-AI/qq/pull/144) with ADR-0041 accepted; DA2 [#150](https://github.com/retsu-AI/qq/pull/150); docs [#123](https://github.com/retsu-AI/qq/pull/123) |
| Open | DA6 (in review). Acceptance 3 (one week of real use) is recorded in the ledger when the week is up |
| Ledger | [`progress/delegated-approval.md`](./progress/delegated-approval.md) |
| As built | [`../design/tools.md`](../design/tools.md) § Approval Policy, [`../design/protocol.md`](../design/protocol.md) (version 28), [ADR-0041](../adr/0041-jev-delegated-approval.md). The target contract `design/delegated-approval.md` was absorbed and deleted in DA6 |
| Supersedes | ADR-0030's "Jev never authorizes side effects", for the `jev_approval` lane only, by [ADR-0041](../adr/0041-jev-delegated-approval.md). Review and routing are unchanged |

## Goal

An operator who opted into a delegate stops babysitting ordinary side effects.
Today a `Prompt`-class shell call or an ungranted host waits on the human even
when `reviewer_model` is set, because a reviewer `Deny` under `auto` escalates
instead of denying, and the 300 s human deadline keeps running while the
reviewer thinks. Four audited calls died as `denied_timeout` at exactly
300.0 s ([`../design/run-reliability-audit-2026-09-21.md`](../design/run-reliability-audit-2026-09-21.md),
R07). The model must then invent another path.

After this plan:

- a configured delegate settles the calls policy already holds, inside the
  selected mode's ceiling;
- Jev is that delegate when `jev_approval: on` and a key is stored, and the
  existing `reviewer_model` is the fallback when Jev is not configured;
- neither configured means the human, as today;
- an interactive client that is attached is not denied by a server timer;
- `Forbidden`, blocked hosts, managed `deny_*`, and `ask_user` never reach a
  delegate.

Measured on the live store after DA2 and DA4, over one week of ordinary use:
`denied_timeout` on sessions that had a client attached for the wait falls to
zero, and the share of `tool_approval_requested` events that a human answers
falls below the share a delegate settles. No regression in `Forbidden` refusals.

## Non-Goals

- No new approval mode, and no widening of `full`. Modes stay the ceiling.
- No change to `Forbidden`, path containment, blocked hosts, or managed
  `deny_*` (ADR-0020, ADR-0021). A human grant that quotes the exact command
  string lifts `Forbidden` today (`SessionGrants::quotes_exactly`); a
  delegate-recorded grant must not. DA4 records a delegate grant so the
  evaluator can tell it from a human one, and a `Forbidden` call never reaches
  the delegate.
- No delegate answers to `ask_user`. A question is a hold for a human, not a
  permission (ADR-0021).
- No folding of this into `jev_review`. Review judges evidence after a tool
  ran and can serialize the loop; approval happens before dispatch and must be
  able to refuse. The two capabilities stay independent (ADR-0030).
- No configurable allow/deny rule DSL. ADR-0020 deferred that until a real
  user needs it. Exact-command session grants cover the repeated-prompt case.
- No generic approval-provider plugin. `ApprovalReviewer` already exists; Jev
  is a second implementation of it, selected at the composition root. ADR-0004
  stands.
- No change to the run-reliability deadline *policy*. RR9 owns "no server
  deadline while a client is attached; headless denies immediately; the option
  is plumbed from config." DA2 consumes that and only adds the delegate's own
  short deadline. If RR9 has not shipped, DA2 includes the minimum of it and
  says so in the receipt.
- No spending of a TypeSafe key on approval unless the operator set
  `jev_approval: on`. A stored key enables nothing (ADR-0030).

## Principles

1. **The mode is the ceiling; the delegate is who decides inside it.** A
   delegate never raises what `read-only`, `ask`, `auto`, `supervised`, or
   `full` would allow. `full` asks nobody. `read-only` has nothing to delegate.
2. **Refuse stays above every delegate.** `Forbidden`, a blocked host, and a
   managed deny are settled by the classifier with no model in the loop.
3. **Two delegates, one contract.** Jev and `reviewer_model` both speak
   `ReviewDecision::{Approve, Deny, Escalate}`. Selection is configuration, not
   a new trait and not a branch in the hot path.
4. **A delegate decision is durable before it executes.** The verdict, who
   made it, and the preview digest are committed before dispatch. A failed
   write is not an approval.
5. **Fail closed to the human, never to approve.** Timeout, outage, abstain,
   and low confidence escalate. Headless with no client denies immediately as
   a tool result.
6. **A delegate cannot widen its own future authority.** It may record an
   exact-command or exact-host session grant, and only when that value fits a
   session grant. A value that does not fit approves the call once and records
   nothing; it never fails the approval. It may not write `.qq/config.ron`,
   record a prefix grant, or promote "always allow."

## Decision summary

> Keep the five approval modes. Add an opt-in delegate that settles the calls
> those modes already hold: Jev when `jev_approval: on`, otherwise
> `reviewer_model` when set, otherwise the human. A delegate `Deny` is final
> under `auto` and `supervised`. Neither delegate may authorize `Forbidden`,
> answer `ask_user`, or write a workspace grant. Interactive approval waits
> have no server deadline; headless denial is immediate.

Design constraints inherited from `AGENTS.md` and the harness plan:

- one runtime; both delegates are bounded provider calls through the existing
  `ApprovalReviewer` seam, never a second loop;
- persist before publish; a verdict is durable before any client sees it and
  before the tool starts;
- every new dimension is bounded: delegate latency, preview bytes, tokens,
  cost, and grants recorded per run;
- the default hot path (no delegate configured, no hold) may not regress;
- no application configuration type enters `qq-core`. The root package
  translates `jev_approval` and `reviewer_model` into a reviewer handle.

## Task index

| ID | Goal | Inputs | Owned paths | Acceptance |
| --- | --- | --- | --- | --- |
| DA1 | `Deny` is final under `auto` when a reviewer is configured; escalation restarts the human wait; the reviewer prompt stops telling the model that a root deny only escalates | none | `crates/qq-core/src/sessions/approvals.rs`, `src/runtime.rs` | `auto` + reviewer `Deny` settles `denied` with no human prompt; `Escalate` and reviewer timeout still prompt; `supervised` unchanged; prompt text no longer says a root deny only escalates. **Merged, #133** |
| DA2 | Delegate deadline is separate from the human wait; interactive attached clients are not denied by a server timer; headless denies immediately | RR9, or the minimum of RR9 included here | `crates/qq-core/src/sessions.rs`, `sessions/approvals.rs`, `src/runtime.rs`, `qq-config` | reviewer/Jev budget does not consume the human wait; attached interactive wait bounded only by the run deadline; headless denial is a tool result naming the policy; config option plumbed. **In review, #150**: shipped RR9's minimum; delegate backstop is 20 s |
| DA3 | `approval_delegate: by-mode\|on\|off` beside the mode: `ask` may opt in; `by-mode` is the prior behavior; `read-only` and `full` ignore it | DA1 | `crates/qq-core/src/approval.rs`, `qq-config`, `src/runtime.rs` | `ask` without it prompts as today; `ask` with `on` routes holds to the delegate; `off` withdraws the reviewer under `auto`; `full` never calls the reviewer; `read-only` denies without one. **Merged, #143** (a knob beside the mode, not a mode variant: the mode enum is on the wire) |
| DA4 | Delegate-recorded grants are exact-command or exact-host, session-scoped, and never written to config | DA1 | `crates/qq-core/src/sessions/approvals.rs`, `crates/qq-core/src/approval.rs` | a delegate approval of `git commit -m x` covers that exact command for the session and not `git commit -m y`; a value past 256 bytes or a full grant table approves once and records nothing; a prefix grant still requires the human; promotion to `.qq/config.ron` is refused for a delegate verdict. **Merged, #135** |
| DA5 | `jev_approval` opt-in: typed approve/deny/abstain over the approval preview, 5 s bound, fails closed to escalate, spend against the run budget | DA1, DA3; ADR-0041 reserved | `src/runtime.rs`, `src/runtime/approval.rs`, `crates/qq-config`, `qq-core` reviewer seam, `docs/adr/0041-*.md` | key stored and `jev_approval: off` never calls TypeSafe; `on` with no key falls through to `reviewer_model`; abstain and over-bound escalate; `Forbidden` never reaches the client; the delegate grant row records `source = 'jev'`. **Merged, #144**; ADR-0041 accepted |
| DA6 | Surfaces: TUI shows who settled a call and offers "stop delegating for this session"; headless records the delegate on the approval event | DA3, DA5 | `crates/qq-tui`, `crates/qq-protocol`, `crates/qq-client`, `docs/design/tools.md`, `docs/design/protocol.md`, `docs/guide/permissions.md` | a delegated approval renders the delegate; the session toggle (`/delegate`, `set_approval_delegate`) changes who decides for the rest of the session without a restart; headless names the delegate. **In review** as `PROTOCOL_VERSION` 28 (`tool_approval_resolved.delegate`, `tool_approval_escalated`, `set_approval_delegate`), store schema 36. The plan's `docs/runbooks/delegated-approval.md` was not written: the operator text fits in `guide/permissions.md` § "Who decides a held call" and `runbooks/jev.md` already covers the Jev lane |

Order: DA1 → DA2 and DA4 (independent once DA1 has merged) → DA3 → DA5 → DA6.
As delivered: #125 → DA1 → DA4 → DA3 → DA5 → DA2 → DA6; DA2 moved last
because RR9 was still planned when DA3 became unblocked, and it then shipped
RR9's minimum itself.
DA1, DA2, and DA5 touch `sessions/` and approval, so each needs independent
review (`workflow.md` § 4). DA5 is the only slice that changes a durability or
approval invariant enough to need the ADR; reserve ADR-0041 in
`progress/root.md` before opening it.

## Slices

### DA1 — A reviewer denial settles the call

**Inputs:** none.
**Owned paths:** `crates/qq-core/src/sessions/approvals.rs`, `src/runtime.rs`
(the `REVIEWER_PROMPT` text and nothing else in that file).
**Gates:** none. The reviewer is off the default path.
**Acceptance:**
- `auto` session, reviewer returns `Deny`: the call settles `denied`, no
  `tool_approval_requested` is published, the tool result names the reviewer.
- `auto` session, reviewer returns `Escalate` or times out: the human prompt
  is published and its deadline starts at escalation, not at reviewer start.
- `supervised` is unchanged: `Deny` final, `Escalate` prompts.
- the reviewer prompt no longer claims that a root-session denial only
  escalates. It states the mode it is deciding under.
**Docs:** one paragraph in `docs/design/tools.md` § Approval Policy, in this
slice's PR. No ADR: this is the reviewer behaving as `supervised` already does.

### DA2 — Two clocks

**Inputs:** RR9 merged, or this slice includes RR9's minimum and the receipt
says which.
**Owned paths:** `crates/qq-core/src/sessions.rs`,
`crates/qq-core/src/sessions/approvals.rs`, `src/runtime.rs` (plumbing only),
`crates/qq-config`.
**Gates:** none named. Record the added wait on a held call before and after;
the budget is "delegate budget ≤ 10 s and not added to a human wait."
**Acceptance:**
- a reviewer that takes 8 s does not shorten the human wait that follows an
  escalation (shipped in DA1: the wait restarts at the escalation; DA2 adds
  the delegate's own bound so a stuck reviewer is cut off by its own clock
  rather than at the human wait — delivered as a 20 s backstop above the
  delegates' own 5 s / 10 s bounds);
- an interactive session with a client attached is not settled
  `denied_timeout` by the server; the run deadline still cancels it;
- a headless run with no client receives a tool result naming the policy
  without sleeping;
- `approval_timeout` in configuration reaches `SessionRuntimeOptions` for a
  supervisor that wants a bound. Absent means the policy above, not 300 s.
**Docs:** `docs/design/tools.md` § Approval Flow; `docs/design/protocol.md` if
the resolution text changes. RR9's plan row is updated in the same PR if this
slice absorbed it.

### DA3 — Who delegates

**Inputs:** DA1.
**Owned paths:** `crates/qq-core/src/approval.rs`, `crates/qq-config`,
`src/runtime.rs`.
**Gates:** none.
**Acceptance:**
- `approval_delegate: off` prompts as today under `ask` and withdraws the
  reviewer under `auto` and `supervised`;
- `approval_delegate: on` under `ask` routes holds to the configured
  delegate; a delegate `Deny` under `ask` escalates with its reason;
- the default (`by-mode`) consults the reviewer under `auto` and `supervised`
  only when one is configured; with neither `reviewer_model` nor
  `jev_approval`, behavior is unchanged;
- `read-only` denies and `full` executes without calling the reviewer;
- `supervised` keeps its current meaning and gains the same delegate
  selection as `auto`.
**Docs:** `docs/design/tools.md` § Approval Policy. No protocol bump: the mode
enum on the wire does not grow. Delivered in #143 as `ApprovalDelegate {
ByMode, On, Off }` beside the mode rather than a mode variant.

### DA4 — Exact grants, no durable widening

**Inputs:** DA1.
**Owned paths:** `crates/qq-core/src/sessions/approvals.rs`,
`crates/qq-core/src/approval.rs`.
**Gates:** none.
**Acceptance:**
- a delegate `Approve` may record an exact-command session grant or an
  exact-host session grant, counted against a per-run cap (64), and only when
  the value fits a session grant (non-empty, at most 256 bytes, session under
  its 256-grant cap);
- a grant that does not fit still approves the call, once, and records
  nothing. It never fails the approval command. That rule already holds for a
  human choice (`InvalidApprovalGrant` is gone); a delegate verdict must not
  reintroduce it;
- that grant matches the exact command string or exact host and nothing
  broader; a prefix grant is not recorded from a delegate verdict;
- the workspace-lifetime promotion path refuses a delegate verdict and does
  not write `.qq/config.ron`;
- a delegate-recorded grant does not lift `Forbidden`. The exact-string
  escape hatch (`SessionGrants::quotes_exactly`) stays a human grant; the
  recorded row must be distinguishable so the evaluator can refuse it;
- a human approval keeps today's once/session/workspace choices, including
  prefix grants, under the same storage rule.
**Docs:** `docs/design/tools.md` § Grant Lifetimes.

### DA5 — Jev as the delegate

**Inputs:** DA1, DA3. ADR-0041 reserved in `progress/root.md` before the PR.
**Owned paths:** `src/runtime.rs`, `src/runtime/approval.rs`, `crates/qq-config`, the
`ApprovalReviewer` seam in `crates/qq-core`, `docs/adr/0041-jev-delegated-approval.md`.
**Gates:** none named. The call is off the hot path and bounded at 5 s.
**Acceptance:**
- `jev_approval: off` with a stored key makes no TypeSafe request on a hold;
- `jev_approval: on` with no key falls through to `reviewer_model`, then the
  human, and records why;
- the decision is a typed yes/no/abstain over the approval preview (command
  or diff, host, task brief, recent action names), bounded like a checkpoint
  payload; abstain, low confidence, timeout, and a malformed reply escalate;
- a `Forbidden` call and an `ask_user` call never reach the client;
- spend counts against the run's reviewer budget; over-budget escalates;
- the durable resolution records the delegate (`source = 'jev'` on the grant
  row, `DelegateIdentity::Jev` on the verdict) distinct from
  `reviewer_model`, so a later audit can tell them apart;
- `qq --tui-qa-root` rejects `jev_approval: on` the way it rejects other Jev
  capabilities.
**Docs:** ADR-0041, explicitly superseding ADR-0030's "never authorizes side
effects" for this lane only. `docs/design/architecture.md` § Extension Contract
gains the lane. `docs/runbooks/jev.md` gains the capability. Amend
`docs/design/delegated-approval.md` into `tools.md` rather than leaving two
approval designs.

### DA6 — Surfaces and the off switch

**Inputs:** DA3, DA5.
**Owned paths:** `crates/qq-tui`, `docs/design/protocol.md`,
`docs/design/headless-contract.md`, `docs/runbooks/delegated-approval.md`,
and the design amendments DA5 did not finish.
**Gates:** none.
**Acceptance:**
- the TUI renders the delegate and a short preview digest on a settled call,
  and a held escalation says why the delegate abstained;
- one session command disables the delegate for the rest of the session
  without rewriting config and without restarting the server;
- headless JSONL carries the delegate identity if the event gains a field;
  goldens updated in the same PR; no `PROTOCOL_VERSION` bump unless the field
  is required for a supervisor to reconstruct the decision, in which case the
  receipt says so and a root request is filed first.
**Docs:** `docs/runbooks/delegated-approval.md`. Update this plan's status
block. Delete the plan once DA6 has shipped and the durable text lives in
`docs/design/tools.md` and ADR-0041.

## Acceptance for the plan

1. Every DA slice's tests green; workspace gates green; the minimal provider
   profile green for any slice that touches `qq-provider` (none are expected to).
2. `Forbidden` and blocked-host refusals have a regression test that runs with
   `jev_approval: on` and `reviewer_model` set and asserts no delegate call.
3. One week of real use after DA4: no `denied_timeout` on a session that had a
   client attached, and a human answers fewer approval prompts than a delegate
   settles. Record the queries and counts in the ledger.
4. ADR-0041 accepted; `docs/design/tools.md` and `architecture.md` amended;
   `docs/design/delegated-approval.md` deleted or reduced to a pointer.
5. The plan is deleted. The ledger remains as the receipt.

## Risks

- A delegate that approves what the operator would have denied. Mitigated by
  the ceiling, the `Forbidden` floor, exact-only grants, and the session off
  switch. Not mitigated to zero: that is what opt-in means.
- Jev latency on the approval path. Bounded at 5 s and off unless opted in.
  `enforce` review already serializes tools; stacking both is the operator's
  choice and the runbook says so.
- Two plans touching `sessions/approvals.rs`. DA2 and RR9 must not both edit
  the deadline. DA2 lists RR9 as an input and stops if that slice is
  `In progress` on the same lines.
