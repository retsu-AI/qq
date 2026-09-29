# Jev operator runbook

How to turn QQ's optional TypeSafe Jev capabilities on, see what they do,
and turn them off. Design and known limitations:
[`../design/jev.md`](../design/jev.md). Planned changes:
[`../plans/jev.md`](../plans/jev.md). This page describes only what ships
today.

## Before you enable anything

QQ runs without Jev by default, including when a TypeSafe credential is
stored. The four capabilities are enabled separately, and none enables
another.

```sh
qq jev setup                    # store an endpoint-bound credential; enables nothing
qq auth status typesafe-jev     # credential metadata
qq auth logout typesafe-jev     # remove it (not needed to turn Jev off)
```

Jev receives masked task text and bounded evidence or previews. Enable it
only for work whose contents you allow TypeSafe to process. Masking is
defense in depth, not a privacy boundary.

Workspace and profile activation require current workspace trust. The
`--tui-qa-root` fixture rejects every enabled Jev capability.

```ron
(
    version: 1,
    jev_review: off,
    jev_routing: false,
    jev_approval: false,
)
```

Inspect values with `qq config show` and sources with
`qq config explain <key>`.

## Checkpoint review — `jev_review`

```sh
QQ_JEV_CHECKPOINTS=final qq     # assess final candidates; tool batching unchanged
QQ_JEV_CHECKPOINTS=enforce qq   # assess each tool result and the final candidate
QQ_JEV_CHECKPOINTS=off qq       # override configured review without deleting the key
```

The same settings apply to `qq ask`, `qq run` and `qq serve`.

**Precedence.** Environment and runtime overrides beat profiles, and
profiles beat top-level values. For a remote client, the server resolves its
own configuration. A client's environment is not forwarded. Unknown values
fail configuration rather than silently enabling or disabling review.

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

**Active runs and children.** An active run keeps its compiled profile, and
spawned work inherits that review choice. A later user prompt, including one
in a child session, resolves the current configuration.

**Limits and failures.** Review is bounded to 32 requests and 2 corrections
per run, 5 s per request, and 64 KiB per response. It shares the run's token
and cost allowance. A RED verdict after corrections, or an unavailable
reviewer during the run, is recorded as evidence, and the run completes. A
missing TypeSafe key is different: the plan does not compile and the run
does not start (`JevKeyRequired`). A positive verdict
is support, not proof.

**`enforce` is slow.** It admits one executable tool call per model turn,
adds serial inference latency, and can't undo side effects. Use `final`
unless you need per-result review.

## Model and effort routing — `jev_routing`

Enable routing with any of `QQ_JEV_ROUTING=on`, `jev_routing: true`, or
`Profile(jev_routing: true)`. `QQ_JEV_ROUTING=off` overrides configured
activation. A missing key fails configuration.

**How it chooses.** Routing picks once, before run preparation, from the
configured fallback plus at most seven other authorized models. Explicit
model and effort pins stay fixed. When only one choice is available, routing
skips inference. Low confidence, a timeout, an invalid reply or an
unavailable selection keeps the configured choice, visibly.

**Automatic effort needs a declaration.** Declare only the efforts the
remote model actually supports:

```ron
models: { "my-model": (reasoning_efforts: [low, medium, high]) }
```

**Receipts.** Decisions and spend are durable in sessions and count against
run budgets. Direct `ask` reports them on stderr. Owned children inherit the
parent's routing activation.

**Pinned effort without Jev.** You can pin effort in configuration
(`reasoning_effort: high`, `Profile(reasoning_effort: Some(low))`) or with
`/effort` in the TUI. Values are `none`, `minimal`, `low`, `medium`, `high`
and `xhigh`. This is a fixed choice, not routing.

## Approval delegate — `jev_approval`

Enable it with `jev_approval: true` or `QQ_JEV_APPROVAL=on`. Jev then
decides calls the approval mode already holds, before `reviewer_model` and
before you (ADR-0041). Whether a held call reaches the delegates at all is the
separate `approval_delegate` setting (or `/delegate`): the default `by_mode`
consults them under `auto` and `supervised` but sends `ask` straight to you;
`on` consults them under `ask` too; `off` never does. The mode stays the ceiling. `Forbidden` shell shapes,
blocked hosts, managed denies and `ask_user` never reach Jev.

The setting is resolved with the run's profile, like `jev_review` and
`jev_routing`: a profile's `jev_approval: false` turns Jev off for runs of
that profile even when the top level turns it on, a profile-only `true`
enables it, and an edit takes effect on the next run without restarting the
server (a run in progress keeps the plan it started with). Inspect the
setting with `qq config show` and `qq config explain jev_approval`.

**What Jev sees.** Only the approval preview. The task brief (child sessions
only), shell command, edit diff, and other tool arguments are each limited to
8 KiB and secret-masked. These are sent **verbatim, without masking**: the
workspace path, the shell working directory, the edit path, recent action
names with their paths (each path cut to 120 bytes), the granted tool names,
and the granted shell prefixes. The whole request is limited to 64 KiB; a
larger one is not sent and the call falls through.

**How it decides.** An answer counts only when confidence and the winning
probability are both at least 0.7. Otherwise the call falls through to
`reviewer_model`, then to you, with the reason attached. Fall-through
triggers:
- abstain or low confidence;
- a malformed reply;
- a transport failure;
- a 5 s timeout;
- a missing key.

Jev never approves on failure.

**What a verdict does.** A deny is final under `auto` and `supervised`, and
advice under `ask`. An approve may record the exact command or host for the
session, nothing wider. Spend counts as reviewer spend only for a returned
verdict; a request that a human, cancellation or deadline overtakes may be
billed without being counted (design finding 7, fixed by JV6).

`/delegate off` withdraws both approval delegates for the session. It does
not disable review or routing, and it does not revoke earlier grants.

### Known limitations (tracked in the plan)

- **You may be prompted before Jev answers.** The TUI shows "approval
  needed" as soon as a call is held. Jev may settle it moments later, and
  answering first drops Jev's decision. Fixed by JV5.
- **Root sessions send no task brief,** so Jev and the fallback often
  abstain or deny for lack of a stated need. Fixed by JV4.
- **Headless `qq run` with Jev but no `reviewer_model`** denies held calls
  immediately. Configure a `reviewer_model` as well. Fixed by JV2.
- **Rounded Jev replies can be rejected as malformed** and fall through.
  Fixed by JV3.

## Passive advisory observer — `qq jev observe`

```sh
qq jev observe --workspace-id WORKSPACE_ID --session-id SESSION_ID \
  --receipts ./jev-advisory.jsonl --max-cost-usd 0.10
```

**What it does.** Observation is enabled only while the command runs. It
never enables review or routing, changes run outcomes, or delays the next
run. Omit `--session-id` to observe all task sessions in the workspace.

**Limits.** Defaults are 300 s and 32 requests. `--duration-seconds`,
`--max-requests` and `--max-total-tokens` reduce the allowance.

**What it reads.** Masked, bounded evidence from the server's recent
snapshot window. Missing task or final-answer evidence produces an
unavailable receipt. It does not read workspace files.

**The receipt journal.** The JSONL file is locked and synced before dispatch
and settlement. Resume with the same file, scope and budget flags. A pending
request left by an interruption has unknown spend, blocks further dispatch
from that journal, and is never retried. Receipts separate `recorded_run`
from `external_advisory` spend, and a combined total is unknown when either
part is. Store receipts with the same care as session history.

## Turning everything off

1. **Configuration.** On the server, set `QQ_JEV_CHECKPOINTS=off`,
   `QQ_JEV_ROUTING=off` and `QQ_JEV_APPROVAL=off`. These beat every profile.
   Top-level `jev_review: off` / `jev_routing: false` / `jev_approval: false`
   are not enough on their own: a selected profile that sets `jev_review` or
   `jev_routing` still wins (see Precedence), so clear those profile values
   too if you use configuration instead of the overrides.
2. **Next run.** Approval activation is resolved for each new run; a run
   already in progress keeps the plan it started with.
3. **Stop observers.** Stop any `qq jev observe` processes.
4. **Optional.** `qq auth logout typesafe-jev`. Removing the key doesn't
   undo earlier grants or side effects.
