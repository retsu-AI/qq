# Optional Jev review and routing

## Passive advisory observation

Run an independent observer against an already running local QQ server:

```sh
qq jev observe --workspace-id WORKSPACE_ID --session-id SESSION_ID \
  --receipts ./jev-advisory.jsonl --max-cost-usd 0.10
```

This explicit command enables observation only for its lifetime. It does not
enable runtime review or routing, change run outcomes, request repairs, or delay
the server's next run. Omit `--session-id` to observe task sessions throughout
the selected workspace. The default duration is 300 seconds and the request
limit is 32; use `--duration-seconds`, `--max-requests`, and
`--max-total-tokens` to reduce the finite allowance.

The observer assesses newly completed runs using masked, bounded evidence from
the server's recent snapshot window. Missing original task or final-answer
evidence produces an unavailable receipt without inference. It does not read
workspace files or retrieve omitted history. Selected evidence cannot establish
the correctness of an entire run.

The JSONL receipt file is exclusively locked and synced before dispatch and
settlement. Resume with the same file, scope and budget flags to retain its
cursor and spending limits. A pending request after interruption has unknown
spend and prevents further dispatch from that journal; it is never automatically
retried. Admission reserves a worst-case request before spending, so observation
can stop before the nominal allowance is fully consumed.

Receipts distinguish `recorded_run` from `external_advisory` spend. Combined
totals remain unknown when either component is unknown. Advisory spending uses
its own explicit allowance and does not modify the completed run's accounting.
Receipts are also printed to stdout after durable recording. Store them with the
same care as session history; masking does not guarantee removal of all secrets.

## Runtime review and routing

QQ runs without Jev by default, including when a TypeSafe credential is stored.
`qq jev setup` stores an endpoint-bound credential; it does not enable reviews.
Jev receives task text and selected evidence, so enable it only for work whose
contents you allow TypeSafe to process. Masking is not a privacy boundary.

Choose the review boundary explicitly:

```sh
QQ_JEV_CHECKPOINTS=final qq     # assess final candidates; keep tool batching
QQ_JEV_CHECKPOINTS=enforce qq  # assess each tool result and final candidate
QQ_JEV_CHECKPOINTS=off qq      # override configured review without deleting a key
```

The same settings apply to `qq ask`, `qq run` and `qq serve`. For a remote
client, the server resolves its own configuration/environment. These are
server-side controls, not environment variables a client forwards implicitly.
Unknown environment values fail configuration instead of silently enabling or
disabling review.

Persistent configuration and named profiles use existing QQ layering and trust:

```ron
(
    version: 1,
    jev_review: off,
    profiles: {
        "review": Profile(jev_review: final),
        "plain": Profile(jev_review: off),
    },
)
```

Choose a profile using the existing `--profile` option or session profile
selection. Explicit environment/runtime overrides win over profiles, which win
over top-level settings. Workspace activation and changes to profile activation
require current workspace trust. Active runs keep their compiled profile;
spawned work inherits that fixed review choice and profile. A later user-submitted
prompt, including one in a child session, resolves its current configuration.
Legacy parent-owned work without a recorded reviewer stays off.

Inspect configured values with `qq config show`, provenance with
`qq config explain jev_review`, and credential metadata with
`qq auth status typesafe-jev`. Remove credentials with
`qq auth logout typesafe-jev` when desired; removal is not required to turn off.

`enforce` is an advanced review mode: it admits one executable tool call per
model turn and reviews each tool result and final candidate. The name selects
the review boundary; it does not require a supported verdict to complete a run.
Both `final` and `enforce` use the following outcome policy:

| Review result | Run behavior |
| --- | --- |
| Supported | Continue, subject to the run's normal limits. |
| Red verdict with a correction remaining | Feed the verdict back and request a correction. Tool and final reviews share two corrective redirects per run. |
| Red verdict after both corrections | Retain the verdict and continue; a final candidate can complete with a red verdict on record. |
| Unavailable (including timeout, malformed reply, or oversized task/evidence) | Record an unavailable outcome and continue without claiming a successful assessment. |

This is the behavior introduced by [RR3 / #117](https://github.com/retsu-AI/qq/pull/117)
and included in v0.1.4. Neither mode is a fail-closed verification gate.
The 32-assessment request limit, cost admission, run budgets and durable event
settlement still apply. In particular, a nominal completion with a pending
checkpoint that was never durably settled fails; that differs from a durably
recorded unavailable assessment.

Review adds inference latency and does not reverse tool side effects. Approval
and sandbox policy own execution authorization, including the separately enabled
Jev approval delegate described below. A completed run is not evidence of a
supported Jev verdict. No Jev speed or quality improvement is claimed without a
paired task evaluation.

Implementation/qualification progress for the stacked work is in
[`../plans/progress/jev-opt-in.md`](../plans/progress/jev-opt-in.md).

Legacy `final`/`enforce` review is bounded to 32 requests and two corrections per run, five seconds per
request, and 64 KiB per response. It consumes the same run token/cost allowance.
Pending review and its final criterion outcomes are visible in event streams;
known reviewer usage and estimated cost are recorded with the verdict. Interrupted
requests have unknown spend. A positive verdict is evidence support, not proof.
For current pinned pricing and uncertainty policy, see the architecture document.

Model/effort routing is independently opt-in: use `QQ_JEV_ROUTING=on`, trusted
`jev_routing: true`, or `Profile(jev_routing: true)`. `QQ_JEV_ROUTING=off` overrides
configured activation. Routing does not enable review. It chooses once before
ordinary provider preparation from at most eight authorized configured models;
explicit model choices stay fixed. Missing credentials fail configuration.

Automatic effort requires a model declaration, for example:

```ron
models: { "my-model": (reasoning_efforts: [low, medium, high]) }
```

Declare only values the remote model supports. Adapter transport support alone
is insufficient; unknown model support preserves omitted effort. Pinned effort
remains fixed, including explicit `none`. One available choice skips inference.
Requests contain masked task text and bounded model metadata. Low confidence,
timeout, invalid responses or unavailable selected routes visibly retain the
configured choice. Session decisions and spend are durable; direct `ask` reports
them on stderr and remains ephemeral. Routing spends count against session run
budgets. Owned children inherit the parent's routing activation; later user
prompts resolve current configuration.

## Strict completion verification

Select Strict through trusted configuration/profile `jev_review: strict` or the
explicit environment override. A stored key alone never enables it:

```sh
QQ_JEV_CHECKPOINTS=strict qq run --timeout-seconds 300 --max-turns 40 -- "Inspect and verify the change"
```

At least one explicit finite duration, turn, tool, token or cost run bound is
required before provider work. A session mode pin retains its existing precedence;
clear it to use the configured Strict profile. The Low–Ultrajev ladder is unchanged.
Owned children inherit Strict and must also have a finite bound; remaining duration,
token and cost bounds propagate, while parent-only turn/tool counts do not grant
children a fresh allowance.

Strict reviews each retained tool result and the final candidate. A failed tool
can be supported evidence of failure. A semantic rejection permits repair under
the original permissions and budgets; it cannot complete until a fresh tool
observation receives support. Rewording the final answer or repeating unchanged
rejected tool evidence cannot request another score. There is no automatic two-
repair or 32-review ceiling in Strict. Reviewer usage/cost consume the original
run allowance; each wait is at most five seconds and the remaining run duration.

Only final `supported` with no pending/open obligation yields `Completed` and a
`verified` record atomically. `verification_unresolved` means semantic obligations
remain; `verification_unavailable` means assessment could not be obtained (including
malformed replies, timeouts and exact-evidence overflow). Unavailable is not a red
verdict and does not initiate repair. Cancellation, interruption and budget
exhaustion preserve their own terminal outcomes and a non-verified record.

Snapshots, checkpoint/terminal events and headless outcomes expose `verification`
separately from answer text, advisory `audit` and typed `final_output`. The record
contains the policy identity, masked request digest, evidence generation, review
count and open correction. Historical/non-strict runs omit it. Headless unresolved
and unavailable outcomes exit unsuccessfully. Pending requests recovered after a
crash retain unknown spend and settle unavailable; they are never replayed.

## Jev as the approval delegate

`jev_approval: true` (or `QQ_JEV_APPROVAL=on`) makes Jev the first delegate
for tool calls the session's approval mode holds, ahead of `reviewer_model`
and the human (ADR-0041). It is independent of review and routing and does
not enable them; a stored key with it off is never read. Like the other Jev
capabilities it is trust-gated in project files and profiles and refused by
the credential-free `--tui-qa-root` fixture.

Jev sees the approval preview only: the command or the diff, the host, the
task brief, recent action names, the session's grants, and the mode. Each
section is bounded to 8 KiB and secret-masked; the transcript is not sent. It
answers one `choice` question (`approve`, `deny`, `abstain`) and the answer
counts only when confidence and the winning probability both reach 0.7 under
the pinned `jev-1.13.0` contract. `abstain`, low confidence, a malformed
reply, a transport failure, a 5 s timeout, or a missing key falls through to
`reviewer_model`, then to you, with the reason attached to the prompt. Jev is
never failed open to approve. A Jev deny is final under `auto` and
`supervised` and advice under `ask`, exactly like a reviewer-model deny; a
Jev approve may record the exact command or host for the session and nothing
wider. `Forbidden` shell shapes, blocked hosts, managed denies, and
`ask_user` never reach it.

Spend counts against the run's budget as reviewer spend. Inspect the setting
with `qq config show` and `qq config explain jev_approval`.

## The Jev mode ladder

A session can pin one rung of a five-step ladder over the three Jev roles
above instead of toggling them one by one (ADR-0044, protocol 31):
`set_jev_mode` on `POST /v1/sessions/jev-mode`, with `mode` set to `low`,
`medium`, `high`, `max`, or `ultrajev`, or omitted to clear the pin. The
summary field `jev_mode` carries the pin on every `session_updated` and
snapshot, so each surface renders the same state from the reducer. In the
TUI, `/jev` opens the same ladder for the focused session (`configured`
clears the pin). A picker row marked `selected` is the saved choice for the
next run, and the top row shows `jev max` while that pin is set. Neither
label means an active run has changed policy.

| mode | routing | review | approval delegate |
| --- | --- | --- | --- |
| `low` | on | off | off |
| `medium` | on | final | off |
| `high` | on | enforce | off |
| `max` | on | enforce | by_mode |
| `ultrajev` | on | enforce | on |

The `enforce` value asks for review after tools and at the final answer;
it retains the RR3 completion behavior described above. `max` and `ultrajev`
set delegation defaults for the configured reviewer. Jev handles approvals
only when the workspace separately enables `jev_approval`; a session
`/delegate` override still wins.

The rung is read when the next run is claimed and never rewrites an active
run's plan. It sits between configuration and explicit selections: it
overrides the workspace's `jev_routing` / `jev_review` / `approval_delegate`
values, while a spawned child's resolved routing and checkpoint policy still
win over the rung it inherits from its parent. A rung never widens Jev
consent (the credential and trust gates above still apply) or the approval
mode's ceiling. When a rung's roles need Jev and no `typesafe-jev` credential
is stored, the next run fails closed with a configuration error, exactly as
`jev_routing: true` would.

Explicit effort can be pinned independently of Jev in trusted configuration:

```ron
(
    version: 1,
    reasoning_effort: high,
    profiles: { "quick": Profile(reasoning_effort: Some(low)) },
)
```

Values are `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, and `max`,
subject to the selected model. `default` explicitly lets the provider choose. Omission
preserves provider defaults; top-level `Clear` removes an inherited setting.
Profile values override top-level settings; explicit runtime overrides win.
In the TUI, `/effort` pins the focused session (or the default for new sessions);
`configured` restores configured/profile inheritance; `default` overrides it
with the provider default. `qq config show` and
`qq config explain reasoning_effort` expose the configured value and source. Unsupported adapter families reject the choice before credential lookup.
Remote model restrictions still apply. This is a pinned choice, not automatic
routing; it makes no speed or quality promise.
