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

`enforce` is an advanced mode: it currently admits one executable tool call per
model turn and fails if an assessment is unavailable. It adds inference latency
and does not reverse tool side effects. Approval and sandbox policy still own
execution authorization. No Jev speed or quality improvement is claimed without
a paired task evaluation.

Implementation/qualification progress for the stacked work is in
[`../plans/progress/jev-opt-in.md`](../plans/progress/jev-opt-in.md).

Review is bounded to 32 requests and two corrections per run, five seconds per
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

Explicit effort can be pinned independently of Jev in trusted configuration:

```ron
(
    version: 1,
    reasoning_effort: high,
    profiles: { "quick": Profile(reasoning_effort: Some(low)) },
)
```

Values are `none`, `minimal`, `low`, `medium`, `high`, and `xhigh`. Omission
preserves provider defaults; top-level `Clear` removes an inherited setting.
Profile values override top-level settings; explicit runtime overrides win.
`qq config show` and `qq config explain reasoning_effort` expose the value and
source. Unsupported adapter families reject the choice before credential lookup.
Remote model restrictions still apply. This is a pinned choice, not automatic
routing; it makes no speed or quality promise.
