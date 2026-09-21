# Architecture decisions

Use [the template](0000-template.md) for new decisions. An Accepted decision
changes only through a superseding ADR; implementation evidence and
clarifications may be appended. Allocate numbers through
[`../plans/progress/root.md`](../plans/progress/root.md) while more than one
agent is writing, and reserve the number there before opening the PR.

ADR-0001 through ADR-0010 were backfilled on 2026-09-08 from decisions already
in force; their dates record when the decision was made, not when the ADR was
written.

| ADR | Decision | Status |
| --- | --- | --- |
| 0001 | [One `SessionRuntime` behind every execution surface](0001-single-session-runtime.md) | Accepted |
| 0002 | [SQLite is the authoritative store, written by one worker with `synchronous=FULL`](0002-sqlite-authoritative-store.md) | Accepted |
| 0003 | [Persist before publish; observers read committed events only](0003-persist-before-publish.md) | Accepted |
| 0004 | [Compile an immutable agent plan; no universal plugin trait](0004-compiled-agent-plan.md) | Accepted |
| 0005 | [The provider is the single retry owner](0005-provider-owns-retry.md) | Accepted |
| 0006 | [Serve live and warm replay from a sequence-indexed feed ring](0006-feed-ring.md) | Accepted; supersedes the H15 broadcast design |
| 0007 | [Classify tool approval from the catalog effect class, not the name](0007-effect-classified-approval.md) | Accepted |
| 0008 | [Feature-gate the Bedrock family inside `qq-provider`](0008-feature-gated-bedrock.md) | Accepted |
| 0009 | [QQ ends at the headless contract; supervisors own hosting](0009-headless-hosting-boundary.md) | Accepted |
| 0010 | [Ship a stripped, thin-LTO release profile and tighten size budgets](0010-release-profile.md) | Accepted |
| 0011 | [One commit discipline for both store lanes; wakeups do not close groups](0011-shared-commit-across-lanes.md) | Accepted |
| 0012 | [One settlement path with a pre-read guard; teardown is a typed prerequisite for a started run's terminal event](0012-structural-settlement.md) | Accepted |
| 0013 | [Context sources are part of plan identity; excess sources fail compilation](0013-context-sources-in-descriptor.md) | Accepted |
| 0014 | [Typed final output: a per-run contract compiled at admission, judged at the completion boundary, repaired within a bounded allowance](0014-typed-final-output.md) | Accepted |
| 0015 | [Enroll remote clients with a pairing code and issue per-client credentials](0015-pairing-code-client-enrollment.md) | Proposed (multi-surface S2) |
| 0019 | [Spill handles are durable session state: cut tool outputs stored with their result, cited by a content-addressed handle, masked inline and exact on explicit read](0019-spill-handles.md) | Accepted |
| 0020 | [Shell `Forbidden` is a policy decision above every approval mode, produced by a CST classifier whose rules are self-tested data](0020-shell-forbidden-classifier.md) | Accepted |
| 0021 | [`Interactive` and `Network` effect classes: a question is a hold, not a permission; a fetch is authority over the outside, not the workspace](0021-interactive-and-network-effect-classes.md) | Accepted |
| 0022 | [One owner per session store: an advisory lock precedes open and recovery](0022-single-store-owner.md) | Accepted |
| 0023 | [Headless JSONL records are protocol types, pinned by golden streams per `PROTOCOL_VERSION`](0023-headless-records-as-protocol.md) | Accepted |
| 0024 | [Shared transcript, raw tool JSON, and a precompiled prompt prefix on the request path](0024-shared-transcript-raw-json-prompt-prefix.md) | Accepted |
| 0025 | [SSE bodies are framed per chunk with one allocation per event; adapters parse once](0025-sse-chunk-framing.md) | Accepted |
| 0026 | [Run cancellation is a token that wakes waiters, not a flag that is polled](0026-run-cancellation-token.md) | Accepted |
| 0027 | [`qq-core` is a public embedding API; its exports are a contract, not leakage](0027-qq-core-public-embedding-api.md) | Accepted |
| 0028 | [Mandatory typed JEV checkpoints after tool results and final candidates](0028-mandatory-typed-jev-checkpoints.md) | Accepted; narrowly supersedes ADR-0003's synchronous-decision list |

| 0030 | [Explicit, independent Jev capabilities](0030-optional-jev-decisions.md) | Accepted; supersedes ADR-0028 activation |

- [ADR-0031: explicit reasoning effort in compiled plans](0031-explicit-reasoning-effort.md) — Accepted.
- [ADR-0032: durable optional task routing](0032-durable-task-routing.md) — Accepted.
- [ADR-0033: model-choice provenance](0033-model-choice-provenance.md) — Accepted.
- [ADR-0034: concrete Jev routing](0034-concrete-jev-routing.md) — Accepted.
- [ADR-0035: global configuration leaf symlinks](0035-global-leaf-config-symlinks.md) — Accepted.
- [ADR-0036: designed truecolor default theme `ink` with `terminal` ANSI fallback](0036-truecolor-default-theme.md) — Accepted (ENG-851).
- [ADR-0038: session retention — archive by session, never by row; receipts and cursors outlive their sessions](0038-session-retention.md) — Proposed (ENG-803).
- [ADR-0039: a run compacts its own turns at a safe boundary with a run-scoped marker](0039-in-run-compaction.md) — Accepted (ENG-793, #92).

## When to write one

Write an ADR when a change:

- alters a system boundary, dependency direction, or crate ownership;
- changes a durability, ordering, retry, approval, or bounding invariant;
- bumps `PROTOCOL_VERSION`, `DESCRIPTOR_VERSION`, or the store schema with a
  behavioral meaning change;
- adds or removes a Cargo feature, build profile, or budget;
- rejects a design the plan had accepted (supersede it explicitly); or
- is something a future agent would otherwise re-litigate.

Do not write one for a bug fix, a refactor with no behavior change, or a
measurement receipt; those go in the plan ledger.
