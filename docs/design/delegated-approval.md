# Delegated approval

A session's approval mode is the ceiling on what may run. A delegate decides
the calls that ceiling still holds, so an operator who opted in is not asked
for every ordinary side effect. This document is the contract
[`../plans/delegated-approval.md`](../plans/delegated-approval.md) builds.
The as-built behavior is [`tools.md`](tools.md) § Approval Policy, which has
absorbed DA1, DA3, DA4, DA5, and DA2; what remains here beyond that section
is DA6 (§ What the operator sees). This file is deleted when DA6 ships.

## Ceiling and delegate

The five modes are unchanged ([`tools.md`](tools.md) § Approval Policy,
ADR-0007, ADR-0021):

| Mode | Ceiling | Delegate |
| --- | --- | --- |
| `read-only` | deny mutations | none; nothing is delegated |
| `ask` | hold every ungranted mutation | the human, unless `approval_delegate: on` |
| `auto` | edits and allow-listed shell execute; `Prompt` shell and ungranted hosts hold | the configured delegate when one exists, unless `approval_delegate: off` |
| `supervised` | every non-read call of a write child is held | the configured delegate; a denial is final |
| `full` | execute, except shell `Forbidden` | none |

`approval_delegate` defaults to `by-mode`: the reviewer under `auto` and
`supervised`, the human under `ask`. `on` extends the delegate to `ask`;
`off` withdraws it everywhere. With neither `jev_approval` nor
`reviewer_model` set there is no delegate and every mode behaves as it did.

`Forbidden` shell shapes (ADR-0020), blocked hosts, and managed `deny_tools`,
`deny_shell_prefixes`, and `deny_hosts` are settled by the classifier before
the mode and before the delegate. No delegate call is made, and no delegate
configuration can move a rule out of that tier.

## Who is asked

A hold resolves through one chain. The first configured delegate answers; the
human is the residue.

```text
classify
  ├─ Forbidden, blocked host, managed deny → refuse; no delegate
  ├─ mode and grants say Execute → execute
  └─ hold
       ├─ jev_approval on and a key stored → Jev
       ├─ else reviewer_model set → that model
       ├─ else → the human
       ├─ Approve → execute, within the mode ceiling
       ├─ Deny → final under auto and supervised; elsewhere escalate
       └─ Escalate, timeout, outage, abstain
            ├─ a client is attached → the human
            └─ headless, no client → tool error, immediately
```

Both delegates speak `ReviewDecision::{Approve, Deny, Escalate}`. The
composition root selects the implementation; `qq-core` does not learn what
Jev or a provider is. A stored TypeSafe key enables nothing by itself
(ADR-0030). `jev_approval` is a third explicit capability beside
`jev_review` and `jev_routing`, default off, and it does not change what
those two do.

`ask_user` stays a hold for a human under every mode, including `full`. A
delegate is not asked. A question is not a permission (ADR-0021).

## What a delegate may decide

A delegate sees the approval preview: the command or the edit diff, the host
for a fetch, the task brief, and the names of recent actions. It does not see
the transcript. The preview is bounded the way a checkpoint payload is
bounded, and an over-bound preview escalates rather than being truncated into
a confident answer.

A delegate `Approve` executes the call and may record a session grant for the
exact command string or the exact host, when that value fits a session grant:
non-empty and at most 256 bytes, and the session is under its 256-grant cap
([`tools.md`](tools.md) § Grant Lifetimes). A value that does not fit still
approves the call, once, and records nothing. A grant the table cannot store
never fails the approval. It may not record a prefix grant, and it may not
promote a grant into `.qq/config.ron`. A human approval keeps the once,
session, and workspace choices, including prefix grants and workspace
promotion, under the same storage rule: a choice whose grant cannot be stored
approves the call once. Delegate-recorded grants are also capped per run.

A delegate `Deny` is final under `auto` and `supervised`: the call settles
`denied`, no human prompt is published, and the tool result names the
delegate. Under `ask` with delegation on, a `Deny` escalates, because the
operator chose the mode that asks. `Escalate`, a timeout, an outage, an
abstain, and a low-confidence Jev verdict all escalate. Escalation starts the
human wait; it does not inherit time the delegate already spent.

## Two clocks

The delegate has its own deadline: Jev bounds itself at 5 s and
`reviewer_model` at 10 s, and the gate cuts off a delegate that breaks its
own bound at `delegate_timeout` (20 s). None of that consumes the human wait.

An interactive session has no server approval deadline: `approval_timeout`
is `None` by default. The run deadline still cancels the run. A headless run
with no client is denied without a human wait, with a tool result that names
the policy, the same way an unanswered `ask_user` exits `needs_input`. A
supervisor that wants a bound sets `approval_timeout_seconds`; absent means
this policy, not a hidden 300 s.

Run-reliability RR9 owned this split; delegated approval DA2 shipped RR9's
minimum together with the delegate clock ([#150](https://github.com/retsu-AI/qq/pull/150)).

## Jev as a delegate

`jev_approval: on` asks Jev for a typed yes, no, or abstain over the preview.
The call is bounded at 5 s, its spend counts against the run's reviewer
budget, and a missing key, an over-budget run, a malformed reply, or a
transport failure falls through to `reviewer_model` and then the human. Jev
is never failed open to approve.

This lane is the one exception to ADR-0030's "Jev never authorizes side
effects," and only for calls the mode would already hold. ADR-0041 records
it. Review still judges evidence after a tool ran; routing still selects a
model. Neither gains the ability to approve a side effect.

`qq --tui-qa-root` rejects `jev_approval: on` with the other Jev
capabilities. The fixture stays credential-free.

## What is recorded

A delegated decision is persisted before the tool starts and before any
client is told it executed. The record carries the verdict and the delegate
(`jev` or `reviewer`, as `source` on the grant row it writes). A human
decision is unchanged. A failed write is not an approval and does not
dispatch.

The resolution vocabulary stays `approved`, `approved_for_session`,
`approved_for_workspace`, `approved_by_reviewer`, `denied`,
`denied_timeout`. Whether `approved_by_reviewer` also gains the delegate
identity on the wire, so a supervisor can tell Jev from `reviewer_model`
without the store, is DA6's decision: it is a protocol change only if a
supervisor must reconstruct the decision from the stream; the slice that adds
it says which and updates the headless goldens.

## What the operator sees

The TUI shows who settled a call and the preview digest, and an escalation
says why the delegate abstained. One session command disables the delegate
for the rest of that session without rewriting configuration and without
restarting the server. Headless output names the delegate on the approval
event.

## Presets

Stricter and looser operation are profiles, not new modes.

| Profile | Mode | `approval_delegate` | Who is asked |
| --- | --- | --- | --- |
| strict | `ask` | `off` | the human, for every ungranted mutation |
| default | `auto` | `by-mode` | the delegate when one is configured; the human on abstain |
| hands-off | `ask` | `on` | the delegate; its deny still comes to the human |
| unattended | `auto` | `by-mode` | the delegate; a headless abstain is an immediate tool error |
| open | `full` | — | nobody, except the `Forbidden` floor |

`supervised` is not a profile. It remains the mode a spawned write child runs
under, and it uses the same delegate selection as `auto`.
