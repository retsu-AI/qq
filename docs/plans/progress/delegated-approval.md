# Ledger — delegated approval

Plan: [`../delegated-approval.md`](../delegated-approval.md).
Only the agent working this plan edits this file. Current state on top;
dated entries appended below, newest last.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| DA1 | Reviewer `Deny` is final under `auto`; escalation restarts the human wait | In review | `feat/eng-862-da1-reviewer-deny-final`, stacked on #125 | No dependency. Independent review (touches `sessions/`) |
| DA2 | Delegate clock separate from the human wait; no server deadline while a client is attached | Planned | | Input: RR9, or include its minimum and say so. Do not start while RR9 is `In progress` on `sessions/approvals.rs` |
| DA3 | `approval.delegate` opt-in; default `auto` enables it only when a delegate is configured | Planned | | Input: DA1. No protocol bump |
| DA4 | Delegate grants are exact-command or exact-host, session-scoped, never written to config | In review | `feat/eng-862-da4-delegate-grants`, stacked on DA1 | Input: DA1. Independent of DA2. Store schema 34 → 35 |
| DA5 | `jev_approval` typed yes/no/abstain; ADR-0041 | Planned | | Inputs: DA1, DA3. Reserve ADR-0041 in `root.md` before the PR |
| DA6 | TUI delegate rendering, session off switch, headless delegate identity, runbook | Planned | | Inputs: DA3, DA5 |

## Entries

### 2026-09-21 — plan opened

Tracked by [ENG-862](https://linear.app/retsu-ai/issue/ENG-862/docsplans-delegated-approval-jev-then-a-reviewer-then-the-human).
No code. The operator asked for an opt-in where Jev decides held approvals and
a model decides them when Jev is not configured, with stricter modes for
operators who want to be asked. Research recorded in the plan and in
[`../../design/delegated-approval.md`](../../design/delegated-approval.md).

Facts checked against the tree before writing:

- `ModelApprovalReviewer` already exists (`src/runtime.rs`) and is invoked from
  `sessions/approvals.rs`, but only under `auto` (held shell, ungranted fetch)
  and `supervised`. Under `auto` its `Deny` escalates to the human; under
  `supervised` a `Deny` is final. The reviewer and the human share one 300 s
  deadline (`DEFAULT_APPROVAL_TIMEOUT`), which is not plumbed from config.
- ADR-0030: Jev review and routing are explicit, default off, and a stored key
  enables nothing. "Jev never authorizes side effects." DA5 is the slice that
  supersedes that sentence, for the approval lane only, via ADR-0041.
- Run-reliability RR9 already owns the interactive-deadline half (audit R07).
  DA2 consumes it and adds the delegate clock. It must not race RR9 on
  `sessions/approvals.rs`.
- ADR-0020's configurable rule table stays deferred. Exact session grants
  cover the repeated-prompt case without a DSL.
- ADR-0004 stands: Jev is a second `ApprovalReviewer`, selected at the
  composition root, not a new trait and not a plugin lane.

Shipped: none. In progress: none. Blocked: none.

### 2026-09-22 — grant storage reconciled with the design

The approval command no longer fails when a grant cannot be stored. A session
or workspace choice whose value is empty, longer than 256 bytes, or past the
session's 256-grant cap approves the call once, records nothing, and promotes
nothing. `SessionRuntimeError::InvalidApprovalGrant` is removed, so the string
"approval grant is empty or exceeds the session limit" cannot be produced.
That landed on `fix/eng-862-approval-grants` ([#125](https://github.com/retsu-AI/qq/pull/125)),
not in a DA slice: it is the storage rule DA4 must keep.

The target contract and DA4's acceptance now say the same thing. A delegate
exact-command grant is subject to the 256-byte value cap and the 256-grant
session cap; a value that does not fit approves the call once and never fails
the command. The per-run cap of 64 is an additional bound on delegate-recorded
grants, not a replacement for the session cap.

ADR-0041 stays reserved. DA1–DA6 stay `Planned`.

### 2026-09-22 — DA1 in review: a reviewer denial settles the call

Branch `feat/eng-862-da1-reviewer-deny-final`, stacked on #125 (the grant
fix) so the approval path is tested as it will merge. Owned paths only:
`crates/qq-core/src/sessions/approvals.rs`, the reviewer prompt in
`src/runtime.rs`, plus the ledger and the design paragraph the slice names.

What changed:

- `ReviewDecision::Deny` under `auto` now settles the call as
  `denied_by_reviewer`, the same durable path `supervised` already used. No
  human prompt follows; the model receives the reviewer's bounded reason as a
  tool error and the run continues. The client-wins race is unchanged: a
  client resolution that committed first still stands.
- The human wait starts at the escalation. Before, the reviewer and the human
  shared one deadline that started when the reviewer was consulted, so a slow
  reviewer ate the human's time. Now `Escalate` restarts the deadline. A
  reviewer that never answers is still bounded by the approval wait, so a
  broken delegate cannot hold a run open.
- The denial text is mode-aware. `supervised` keeps "for the supervised
  sub-agent"; `auto` says "The approval reviewer denied this tool call:" and
  never claims a sub-agent.
- `REVIEWER_SYSTEM_PROMPT` no longer tells the model a root deny only
  escalates. It states that deny is final under every mode the reviewer is
  consulted for, and the request names the mode with what that mode holds.
- `tools.md` § Approval Policy and `protocol.md`'s resolution paragraph
  describe the as-built behavior. `ApprovalResolution::DeniedByReviewer`'s doc
  comment no longer says `auto` escalates.

What did not change: `ask` never consults the reviewer; `read-only` and `full`
never reach the hold; headless `auto` still supplies a deferred unattended
deny for an escalation (`REVIEWER_DENY_GRACE`), which a reviewer resolution
that landed first makes a no-op. No protocol bump: the resolution vocabulary
is unchanged, only when `denied_by_reviewer` can occur.

Tests added in `sessions/tests/approvals.rs`:
`a_reviewer_denial_is_final_under_auto_and_no_human_is_asked` (replaces
`reviewer_denial_still_lets_the_client_decide`, which asserted the old
behavior), `a_reviewer_escalation_starts_the_human_wait_at_the_escalation`,
`a_reviewer_that_never_answers_is_bounded_by_the_approval_wait`. The existing
supervised denial test still passes with the supervised wording.

Gates: `sessions::tests::*` (292 passed), `qq` binary reviewer and headless
tests, `qq-protocol` goldens, `cargo fmt --check`, clippy `-D warnings` on
`qq-core`, `qq-protocol`, `qq`. No named performance gate: the reviewer is
off the default path and the change adds one `Instant::now()` on escalation.

DA2 note: the escalation-restarts-the-wait half of DA2's acceptance is now
done here, because it fell out of making `Deny` final on the same select
arms. DA2 keeps the rest: the delegate's own short deadline, no server
deadline while a client is attached, headless immediate denial, and the
config option. RR9 still owns the deadline policy; this slice did not touch
`DEFAULT_APPROVAL_TIMEOUT` or its plumbing.

### 2026-09-22 — DA4 in review: exact delegate grants, no durable widening

Branch `feat/eng-862-da4-delegate-grants`, stacked on DA1 so a reviewer
`Approve` is tested against the final-deny gate it will merge with. Owned
paths: `crates/qq-core/src/sessions/approvals.rs`,
`crates/qq-core/src/approval.rs`, plus the store code those two need
(`sessions/tool_calls.rs`, `sessions/store.rs`, `store/schema.rs`) and
`tools.md` § Grant Lifetimes.

What changed:

- **Provenance on every grant row.** `session_grants` gains `source`
  (`human` | `delegate`, default `human`) and `run_id` (NULL for a human).
  Store schema 34 → 35; a pre-35 store's rows all become `human`, which is
  what they were. The migration refuses a `source` column of the wrong shape
  like every other step.
- **A reviewer `Approve` records its own grant** in the same transaction as
  the approval: the exact command string for shell, the exact host for
  `fetch`, nothing for other classes. `SessionGrants` gains a `delegate`
  set (`DelegateGrants { commands, hosts }`) that the evaluator matches
  byte-for-byte only. The human's `tools`, `shell_prefixes`, and `hosts`
  keep their wide shapes.
- **The floor holds.** `quotes_exactly` reads only the human prefixes, so a
  delegate row with the exact text of a `Forbidden` command changes nothing.
  Tested under all five modes, alongside the human exact-string case that
  still lifts it (ADR-0020).
- **Bounded.** Per-run cap `MAX_DELEGATE_GRANTS_PER_RUN = 64` by `run_id`,
  in addition to the session-wide 256. The #125 storage rule applies: a
  value that does not fit approves the call once and records nothing; the
  approval never fails. A human grant with the same `(kind, value)` wins
  and the delegate row is not written.
- **No promotion.** The delegate path never touches
  `pending_workspace_grant_promotions`; only a human `ApproveForWorkspace`
  does. Asserted directly.

What did not change: human once/session/workspace choices, including prefix
grants and promotion; `insert_seed_grants` (config seeds are `human`); the
resolution vocabulary (`approved_by_reviewer` is unchanged, so no protocol
bump); the reviewer request shape.

Tests: `approval::tests::delegate_grants_match_exactly_and_never_lift_forbidden`
(evaluator, all modes); in `sessions/tests/approvals.rs`:
`a_delegate_approval_records_an_exact_command_grant_and_nothing_wider`
(`git commit -m x` twice + `-m y`: two holds, two reviewer consultations,
no promotion row),
`a_delegate_host_grant_covers_the_exact_host_only`,
`a_delegate_grant_that_cannot_be_stored_still_approves_the_call_once`
(byte cap and per-run cap),
`a_delegate_grant_never_lifts_forbidden_and_a_forbidden_call_never_reaches_the_reviewer`
(plan acceptance 2, `reviewer_model` half; the `jev_approval` half lands
with DA5). Migration:
`version_thirty_four_gains_grant_provenance_and_keeps_old_rows_human`.
Existing migration tests updated from `"34"` to `"35"` (26 sites).

Gates: `cargo test -p qq-core --lib` (630 passed, compaction/streaming
skipped locally for time; the approvals, delegation, and migrations modules
ran in full), `mcp_session` integration, the binary's reviewer and headless
allowlist tests, `cargo fmt --check`, `cargo clippy --workspace -D warnings`.
No named performance gate: the delegate path adds two indexed lookups and one
insert inside a transaction that already exists, only when a reviewer
approves; `load_approval_policy` reads one more column per row.

DA5 note: the `delegate: jev` vs `delegate: reviewer` distinction the design
asks for is not in this slice. Both would write `source = 'delegate'` today;
DA5 should widen `source` (or add a column) when Jev becomes a second
writer, and the `approved_by_reviewer` event field question stays with DA6.
