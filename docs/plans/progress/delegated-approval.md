# Ledger — delegated approval

**Closed 2026-09-24.** The plan (`docs/plans/delegated-approval.md`,
ENG-862) shipped in full and was deleted, as it required; this ledger is the
receipt. The durable text is [`../../design/tools.md`](../../design/tools.md)
§ Approval Policy, [`../../design/protocol.md`](../../design/protocol.md)
(version 28), [ADR-0041](../../adr/0041-jev-delegated-approval.md), and the
operator guide [`../../guide/permissions.md`](../../guide/permissions.md)
§ "Who decides a held call".

What the plan set out to do: an operator who opts in stops babysitting
ordinary side effects. A configured delegate — Jev when `jev_approval: on`
and a key is stored, otherwise `reviewer_model`, otherwise the human —
settles the calls the approval mode already holds, inside that mode's
ceiling. `Forbidden`, blocked hosts, managed `deny_*`, and `ask_user` never
reach a delegate. An attached interactive client is not denied by a server
timer. The five modes are unchanged; `full` is not widened.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| — | Grant storage rule: a grant that cannot be stored approves once and never fails the command | Merged | [#125](https://github.com/retsu-AI/qq/pull/125) | Removes `InvalidApprovalGrant`; the 400 string cannot be produced |
| DA1 | Reviewer `Deny` is final under `auto`; escalation restarts the human wait | Merged | [#133](https://github.com/retsu-AI/qq/pull/133) | Independent review done (touches `sessions/`) |
| DA2 | Delegate clock separate from the human wait; no server deadline for an interactive hold; `approval_timeout_seconds` | Merged | [#150](https://github.com/retsu-AI/qq/pull/150) | Ships RR9's minimum; RR9 (ENG-871) row updated in the same PR |
| DA3 | `approval_delegate: by-mode\|on\|off` says who settles a held call | Merged | [#143](https://github.com/retsu-AI/qq/pull/143) | No protocol bump; not part of the plan digest |
| DA4 | Delegate grants are exact-command or exact-host, session-scoped, never written to config | Merged | [#135](https://github.com/retsu-AI/qq/pull/135) | Store schema 34 → 35 |
| DA5 | `jev_approval` typed approve/deny/abstain; ADR-0041 | Merged | [#144](https://github.com/retsu-AI/qq/pull/144) | ADR-0041 accepted. `source = 'jev'` on delegate grant rows |
| DA6 | Surfaces: `tool_approval_resolved.delegate`, `tool_approval_escalated`, `set_approval_delegate` (`/delegate`), TUI "who decided", headless `approved by jev` | Merged | [#152](https://github.com/retsu-AI/qq/pull/152) | `PROTOCOL_VERSION` 27 → 28; store schema 35 → 36. Target contract deleted |
| docs | Plan, target contract, ledger | Merged | [#123](https://github.com/retsu-AI/qq/pull/123) | |
| close | Blocked-host regression test with a delegate wired; plan deleted; indexes | Merged | `chore/eng-862-close-out` | Closes plan acceptance 2 and 5 |

### Plan acceptance

1. Every slice's tests and the workspace gates green — **met** (no slice
   touched `qq-provider`).
2. `Forbidden` and blocked-host refusals have a regression test with a
   delegate configured that asserts no delegate call — **met**:
   `a_delegate_grant_never_lifts_forbidden_and_a_forbidden_call_never_reaches_the_reviewer`
   (DA4) and `a_blocked_host_never_reaches_the_delegate_even_when_one_would_approve`
   (close-out; `auto`, `approval_delegate: on`, a Jev-attributed reviewer
   that would approve). Jev-in-front is `jev_http_contract_decides_and_never_reaches_the_reviewer_on_a_confident_answer`
   (DA5).
3. One week of real use after DA4: no `denied_timeout` on a session that had
   a client attached, and a human answers fewer approval prompts than a
   delegate settles — **open until 2026-09-30** (DA2 removed the default
   deadline on the 23rd). Record here as a dated entry with these two counts
   over the live store:

   ```sql
   -- holds settled by the server clock; the store does not record client
   -- attachment, and since DA2 this can only be non-zero where
   -- approval_timeout_seconds is configured, so the count stands in
   SELECT count(*) FROM tool_calls WHERE approval_resolution = 'denied_timeout';
   -- who settled the holds: human (approved_once, approved_for_*, denied)
   -- against delegate (approved_by_reviewer, denied_by_reviewer)
   SELECT approval_resolution, count(*) FROM tool_calls
     WHERE approval_resolution IS NOT NULL
     GROUP BY approval_resolution;
   ```

   No regression in `Forbidden` refusals: `tool_calls.result LIKE
   'forbidden:%'` still denies with no `tool_approval_requested`.
4. ADR-0041 accepted; `tools.md` and `architecture.md` amended;
   `design/delegated-approval.md` deleted — **met** (DA5, DA6).
5. The plan is deleted; the ledger remains — **met** (close-out).

## Entries

### 2026-09-21 — plan opened

Tracked by [ENG-862](https://linear.app/retsu-ai/issue/ENG-862/docsplans-delegated-approval-jev-then-a-reviewer-then-the-human).
No code. The operator asked for an opt-in where Jev decides held approvals and
a model decides them when Jev is not configured, with stricter modes for
operators who want to be asked. Research recorded in the plan and in the
target contract `docs/design/delegated-approval.md` (both since deleted; the
as-built text is `docs/design/tools.md` § Approval Policy).

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

### 2026-09-23 — #125, DA1, DA4 merged

[#125](https://github.com/retsu-AI/qq/pull/125), [#133](https://github.com/retsu-AI/qq/pull/133),
and [#135](https://github.com/retsu-AI/qq/pull/135) are on `main` in that
order, each retargeted to `main` as the one below it merged. No conflicts
beyond the ledger.

### 2026-09-23 — DA3 merged: `approval_delegate` says who settles a held call

Branch `feat/eng-862-da3-delegate-opt-in`, [#143](https://github.com/retsu-AI/qq/pull/143).
The plan's task index says "a `DelegatedApproval` mode on `ApprovalMode`";
the slice did not do that, because the mode enum is on the wire and the
non-goals say modes stay the ceiling. It is a separate knob:

- `qq_core::ApprovalDelegate { ByMode, On, Off }`, default `ByMode`.
  `consults_reviewer(mode)`: `ByMode` asks the reviewer under `auto` and
  `supervised` and the human under `ask` (prior behavior); `On` extends the
  reviewer to `ask`; `Off` withdraws it everywhere. `read-only` and `full`
  never consult. `deny_is_final(mode)`: `auto` and `supervised` only, so
  under `ask` with `On` a reviewer `Deny` escalates with its reason and the
  human wait starts at the denial.
- Plumbed `Runtime.approval_delegate` → `AgentProfile` →
  `CompiledAgentPlan::approval_delegate()` → `SessionToolGate`. Not part of
  the plan digest: it changes who is asked, never what the model may do.
- Config: `approval_delegate: on|off` top-level and per profile,
  `QQ_APPROVAL_DELEGATE`, `ConfigKey::ApprovalDelegate` (trust-gated,
  sensitive). The root translates `Option<ApprovalDelegateSetting>` to
  `ApprovalDelegate` in `src/runtime.rs`; the request override wins over the
  profile.
- The reviewer prompt names the mode with an `Ask` arm and says deny is
  final under `auto`/`supervised` and escalates under `ask`.

The plan's "default `auto` profile turns it on only when a delegate is
configured" is satisfied by construction: without `reviewer_model` or
`jev_approval` there is no reviewer to consult, and `ByMode` under `auto`
asks the human as before.

Tests: five DA3 cases in `sessions/tests/approvals.rs` (`Off` withdraws the
reviewer under `auto`; `On` lets the reviewer approve under `ask`; `On`
escalates a reviewer denial under `ask`; `ByMode` is the prior behavior;
`read-only`/`full` never consult), two `qq-config` tests, and the root test
`approval_delegate_reaches_the_plan_from_config_profile_and_override`.
Gates: `cargo test --workspace`, fmt, clippy `-D warnings`. No protocol or
schema change.

### 2026-09-23 — DA5 merged: Jev decides when `jev_approval` is on

Branch `feat/eng-862-da5-jev-approval`, [#144](https://github.com/retsu-AI/qq/pull/144).
ADR-0041 accepted (`docs/adr/0041-jev-delegated-approval.md`); it
supersedes ADR-0030's "never authorizes side effects" for this lane only.

- `src/runtime/approval.rs`: `JevApprovalReviewer` wraps
  `ModelApprovalReviewer` unconditionally at the composition root. Whether
  Jev is consulted is read per hold from the held call's workspace config
  (`jev_approval`), cached per credential epoch; off means the fallback is
  asked directly and the key is never read (ADR-0030 holds). No key with it
  on falls through and records `no TypeSafe key` in the escalation reason.
- One `choice` question with labels `approve`/`deny`/`abstain` over the
  masked preview (8 KiB per section, 64 KiB per request; over-bound
  abstains). Bounded at 5 s. The reply must pin `jev-1.13.0`, carry usage,
  and have the chosen label at max with confidence and probability ≥ 0.7;
  anything else is `JevAbstain::{NoKey, OverBound, Transport, Malformed,
  LowConfidence, Abstained}` and falls through to `reviewer_model`, then the
  human, with the reason prepended. Never fails open. Spend is summed onto
  the fallback's.
- `qq_core::DelegateIdentity { Reviewer, Jev }` on `ReviewVerdict`; the
  delegate grant row writes `source = 'jev'` or `'delegate'`, so an audit can
  tell them apart (the DA4 note is closed). No schema bump: the column has
  existed since 35; the per-run cap counts both.
- Config: `jev_approval: bool` top-level and per profile, `QQ_JEV_APPROVAL`,
  `ConfigKey::JevApproval` (trust-gated). `qq --tui-qa-root` rejects it like
  the other Jev capabilities.

Tests: six in `runtime::approval` (contract over a local HTTP server, off
means no request, transport failure escalates with the reason, over-bound
abstains, low confidence abstains, deny is final under `auto`),
`a_jev_approval_records_a_grant_row_that_names_jev_and_still_matches_exactly`,
config independence and trust tests. Gates: `cargo test --workspace`, fmt,
clippy `-D warnings`.

The plan's acceptance 2 (`Forbidden` never reaches a delegate with both
configured) is covered by
`a_delegate_grant_never_lifts_forbidden_and_a_forbidden_call_never_reaches_the_reviewer`
(DA4) plus the DA5 wrapper being in front of that same seam; a `Forbidden`
call never reaches the gate's hold, so neither delegate is constructed a
request.

### 2026-09-23 — DA2 in review: two clocks

Branch `feat/eng-862-da2-two-clocks`, [#150](https://github.com/retsu-AI/qq/pull/150),
against `main`. RR9 (ENG-871) was still `Planned`, so this slice ships RR9's
minimum and the RR9 row in `run-reliability.md` says so.

- `DEFAULT_APPROVAL_TIMEOUT: Option<Duration> = None`. An interactive hold
  has **no server deadline**: it ends when the client answers, the run's own
  deadline cancels, or the run is cancelled. `denied_timeout` only occurs when
  `approval_timeout_seconds` is set.
- `DEFAULT_DELEGATE_TIMEOUT = 20 s`, a backstop for a delegate that breaks
  its own bound (Jev 5 s, reviewer 10 s). When it fires the pending review is
  dropped and the human is asked; the human clock, when there is one, starts
  then, at an `Escalate`, or at a non-final `Deny`. The plan said 10 s; the
  slice chose 20 s so a slow reviewer model on a cold provider is not cut off
  before its own 10 s timeout plus transport.
- `SessionRuntimeOptions { approval_timeout: Option<Duration>,
  delegate_timeout }`; `RuntimeHandler::open_with(factory, approval_timeout)`
  threads the configured wait through `qq run`, the TUI, and `qq serve`.
- Config: `approval_timeout_seconds` 1..=86400, validated in `finish`
  (`ConfigError::InvalidApprovalTimeout`), not trust-gated because it only
  shortens a wait. `qq explain approval_timeout`; `qq show` prints it.
- Headless keeps `REVIEWER_DENY_GRACE` (20 s) for the unattended deny after
  an escalation; without a delegate the deny is immediate.

Tests: `a_reviewer_that_never_answers_is_cut_off_by_its_own_clock_not_the_humans`,
`a_stuck_reviewer_and_an_absent_human_settle_on_the_human_clock_after_the_delegates`,
`an_interactive_hold_has_no_server_deadline_by_default`,
`approval_timeout_is_absent_by_default_and_bounded_when_set`. The DA1 test
`a_reviewer_that_never_answers_is_bounded_by_the_approval_wait` is replaced
by the first of these. Gates: `cargo test --workspace`, fmt, clippy
`-D warnings`, on `main` after OB5/OB9.

Shipped: #125, DA1, DA3, DA4, DA5. In review: DA2 (#150), docs (#123, stacked
on DA2). Open: DA6.

### 2026-09-23 — DA2 and docs merged

[#150](https://github.com/retsu-AI/qq/pull/150) and [#123](https://github.com/retsu-AI/qq/pull/123)
are on `main`, in that order.

### 2026-09-24 — DA6 in review: surfaces and the off switch

Branch `feat/eng-862-da6-delegate-surfaces`, against `main`. The plan asked
DA6 to decide the `approved_by_reviewer` field question. Decision: **yes, a
protocol change**, because a supervisor reading a headless stream cannot
otherwise tell Jev's decision from `reviewer_model`'s, and the plan's own
acceptance ("headless output names the delegate on the approval event")
requires it. `PROTOCOL_VERSION` 27 → 28; the root ledger row records the
shared-file change. Store schema 35 → 36.

- **Wire.** `qq_protocol::ApprovalDelegate { ByMode, On, Off }` and
  `DelegateIdentity { Reviewer, Jev }` are now protocol types (`by_mode` /
  `on` / `off`, `reviewer` / `jev`); `qq-core` re-exports them instead of
  owning duplicates. `tool_approval_resolved` gains optional
  `delegate` beside `approved_by_reviewer` / `denied_by_reviewer`, absent
  for human, timeout, and answer resolutions. New advisory event
  `tool_approval_escalated { tool_call_id, delegate?, reason }` when a
  delegate passes: an `escalate`, a non-final `deny` under `ask` (reason
  prefixed `the delegate would deny:`), or the delegate clock running out
  (`delegate` absent, reason `the delegate did not answer within its
  window`). Written only while the call is still awaiting approval, so a
  lost race publishes nothing. New command `set_approval_delegate
  { session_id, delegate? }` on `/v1/sessions/approval-delegate` with
  outcome `approval_delegate_set`; new optional
  `SessionSummary.approval_delegate`. `/delegate` reserved as a client slash
  command. Goldens under `v28/` (three new: the command, the receipt, the
  Jev-attributed resolution, the escalation) and `headless/v28/`; harbor
  trace fixtures bumped to 28.
- **Store.** Schema 36 adds nullable `sessions.approval_delegate`.
  `load_approval_policy` returns it beside the mode and grants; the gate
  applies `session_override.unwrap_or(configured)` at each hold, so the
  switch takes effect at the next held call of a running session with no
  restart and no config write. Spawned children (both `CreateSession` with a
  parent and the model's `spawn_agent`) inherit the parent's override.
  `set_approval_delegate` has no authority check: the mode is the ceiling
  and every value asks at least as much of a human as the configured choice.
- **TUI.** `/delegate` picker (rows `configured`, `by_mode`, `on`, `off`;
  needs a focused session; running sessions allowed). Status badge reads
  `MODE · delegate off` while an override is set. The client keeps who
  settled each call beside its timing (`ApprovalSettlement`, evicted with the
  body); the expanded call detail shows `approved by jev` /
  `approved for session` / `denied by you`, and a delegate denial's row
  state reads `denied by jev` or `denied by reviewer`. The approval block
  shows the escalation reason (`reviewer passed to you: …` or `delegate
  timed out: …`). Receipt notices name the delegate.
- **Headless.** JSONL carries the fields through the envelope. Text mode
  prints `[tool] NAME approved by jev` and `[tool] reviewer passed to the
  human: …`.
- **Docs.** `tools.md` § Approval Policy (session override paragraph, wire
  identity), `protocol.md` (version 28, route, command section, resolution
  text), `guide/tui.md`, `guide/permissions.md` ("Stop delegating for this
  session", "Seeing who decided"), ADR-0041 § 6.
  `docs/design/delegated-approval.md` deleted: every section is now as-built
  in `tools.md` / `protocol.md` / ADR-0041, as the plan required.

Tests: core `the_session_off_switch_withdraws_the_reviewer_at_the_next_hold_without_a_restart`,
`a_session_delegate_override_reaches_the_reviewer_under_ask_and_children_inherit_it`,
`setting_the_delegate_on_an_unknown_session_is_refused`,
`a_reviewer_escalation_is_published_with_its_reason_before_the_human_is_asked`,
`an_advisory_denial_under_ask_is_published_as_an_escalation_naming_the_delegate`,
and the delegate-clock test now asserts the cut-off escalation; protocol
round-trips and goldens; client `a_reviewer_resolution_records_who_settled_the_call_and_evicts_with_it`,
`an_escalation_lands_on_the_pending_holds_preview_and_nowhere_else`; TUI
`delegate_picker_switches_the_focused_session_off_and_reports_the_receipt`,
`delegate_picker_needs_a_focused_session`; headless
`a_delegated_approval_names_the_delegate_in_jsonl_and_text`; migrations
re-pinned to 36. Gates: `cargo test --workspace`, fmt, clippy `-D warnings`.

Plan acceptance 3 (one week of real use after DA4: no `denied_timeout` with
a client attached; humans answer fewer prompts than delegates settle) is
not measurable yet; DA2 removed the default deadline four days ago. Record
the counts here when the week is up. Everything else in "Acceptance for the
plan" is met with this slice.

### 2026-09-24 — DA6 merged; plan closed

[#152](https://github.com/retsu-AI/qq/pull/152) is on `main`. Close-out on
`chore/eng-862-close-out`:

- Plan acceptance 2 had only the `Forbidden` half under test with a delegate
  wired. Added `a_blocked_host_never_reaches_the_delegate_even_when_one_would_approve`:
  `auto`, `approval_delegate: on`, a reviewer that would approve and names
  Jev; a link-local `fetch` is denied with no `tool_approval_requested`,
  `_resolved`, or `_escalated`, the reviewer is never consulted, and no
  delegate host grant is recorded.
- `docs/plans/delegated-approval.md` deleted per its own acceptance 5; the
  goal, slice table, and acceptance checklist moved to the head of this
  ledger so it stands alone. Index rows in `docs/plans/README.md` and
  `docs/plans/progress/README.md` updated; `docs/README.md` already points at
  the as-built text.
- The plan's DA6 row asked for `docs/runbooks/delegated-approval.md`. Not
  written: the operator procedure is `guide/permissions.md` § "Who decides a
  held call" (setting, session switch, seeing who decided) and
  `runbooks/jev.md` § "Jev as the approval delegate" covers the Jev lane.

Open: acceptance 3 only, measurable from 2026-09-30. Nothing to build.
