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
| 0012 | Structural settlement (`RunSettlement`, teardown before terminal publication) | Reserved: H21 |
| 0013 | [Context sources are part of plan identity; excess sources fail compilation](0013-context-sources-in-descriptor.md) | Accepted |
| 0014 | Typed final output contract | Reserved: HC3 |
| 0015 | [Enroll remote clients with a pairing code and issue per-client credentials](0015-pairing-code-client-enrollment.md) | Proposed (multi-surface S2) |

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
