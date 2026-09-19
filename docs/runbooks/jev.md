# Optional Jev review

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

Automatic model/effort routing is a separate planned slice. Setting
`jev_routing: true` currently returns a configuration error; it is never silently
treated as enabled. `QQ_JEV_ROUTING=off` overrides that reserved setting.

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
