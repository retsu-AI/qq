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
configuration changes affect subsequent plan loads.

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
