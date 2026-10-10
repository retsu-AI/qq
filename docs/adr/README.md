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
| 0003 | [Persist before publish; observers read committed events only](0003-persist-before-publish.md) | Accepted; superseded in part by ADR-0028 |
| 0004 | [Compile an immutable agent plan; no universal plugin trait](0004-compiled-agent-plan.md) | Accepted |
| 0005 | [The provider is the single retry owner](0005-provider-owns-retry.md) | Accepted; superseded in part by ADR-0040 |
| 0006 | [Serve live and warm replay from a sequence-indexed feed ring](0006-feed-ring.md) | Accepted; supersedes the H15 broadcast design |
| 0007 | [Classify tool approval from the catalog effect class, not the name](0007-effect-classified-approval.md) | Accepted |
| 0008 | [Feature-gate the Bedrock family inside `qq-provider`; no provider-per-crate split](0008-feature-gated-bedrock.md) | Accepted |
| 0009 | [QQ ends at the headless contract; supervisors own hosting](0009-headless-hosting-boundary.md) | Accepted |
| 0010 | [Ship a stripped, thin-LTO release profile and tighten size budgets](0010-release-profile.md) | Accepted |
| 0011 | [One commit discipline for both store lanes; wakeups do not close groups](0011-shared-commit-across-lanes.md) | Accepted |
| 0012 | [One settlement path with a pre-read guard; teardown is a typed prerequisite for a started run's terminal event](0012-structural-settlement.md) | Accepted |
| 0013 | [Context sources are part of plan identity; excess sources fail compilation](0013-context-sources-in-descriptor.md) | Accepted |
| 0014 | [Typed final output: a per-run contract compiled at admission, judged at the completion boundary, repaired within a bounded allowance](0014-typed-final-output.md) | Accepted |
| 0015 | [Enroll remote clients with a pairing code and issue per-client credentials](0015-pairing-code-client-enrollment.md) | Proposed (multi-surface S2) |
| 0019 | [Spill handles are durable session state: cut tool outputs are stored with their result, cited by a content-addressed handle, masked inline and exact on explicit read](0019-spill-handles.md) | Accepted |
| 0020 | [Shell `Forbidden` is a policy decision above every approval mode, produced by a CST classifier whose rules are self-tested data](0020-shell-forbidden-classifier.md) | Accepted |
| 0021 | [`Interactive` and `Network` effect classes: a question is a hold, not a permission; a fetch is authority over the outside, not the workspace](0021-interactive-and-network-effect-classes.md) | Accepted (`Interactive` merged in #49, `Network` in #50) |
| 0022 | [One owner per session store: an advisory lock is taken before the database is opened and before recovery runs](0022-single-store-owner.md) | Accepted |
| 0023 | [Headless JSONL records are protocol types, pinned by golden streams per `PROTOCOL_VERSION`](0023-headless-records-as-protocol.md) | Accepted |
| 0024 | [Shared transcript, raw tool JSON, and a precompiled prompt prefix on the request path](0024-shared-transcript-raw-json-prompt-prefix.md) | Accepted |
| 0025 | [SSE bodies are framed per chunk with one allocation per event; adapters parse once](0025-sse-chunk-framing.md) | Accepted |
| 0026 | [Run cancellation is a token that wakes waiters, not a flag that is polled](0026-run-cancellation-token.md) | Accepted |
| 0027 | [`qq-core` is a public embedding API; its exports are a contract, not leakage](0027-qq-core-public-embedding-api.md) | Accepted |
| 0028 | [Mandatory typed JEV checkpoints after tool results and final candidates](0028-mandatory-typed-jev-checkpoints.md) | Accepted; narrowly supersedes ADR-0003's synchronous-decision list |
| 0030 | [Explicit, independent Jev capabilities](0030-optional-jev-decisions.md) | Accepted; supersedes ADR-0028 activation |
| 0031 | [Explicit reasoning effort belongs to the compiled plan](0031-explicit-reasoning-effort.md) | Accepted |
| 0032 | [Routing spend belongs to the reserved run](0032-durable-task-routing.md) | Accepted |
| 0033 | [Keep model pins distinct from configured fallbacks](0033-model-choice-provenance.md) | Accepted |
| 0034 | [Route only among declared, authorized choices](0034-concrete-jev-routing.md) | Accepted |
| 0035 | [Global configuration may be a leaf symlink to a regular file](0035-global-leaf-config-symlinks.md) | Accepted |
| 0036 | [Designed truecolor default theme `ink` with `terminal` ANSI fallback](0036-truecolor-default-theme.md) | Accepted (ENG-851) |
| 0038 | [Session retention: archive by session, never by row; receipts and cursors outlive their sessions](0038-session-retention.md) | Proposed (ENG-803) |
| 0039 | [A run compacts its own turns at a safe boundary with a run-scoped marker](0039-in-run-compaction.md) | Accepted (ENG-793, #92) |
| 0040 | [Two-phase retry ownership: the provider owns sends, the run owns turns](0040-two-phase-retry-ownership.md) | Accepted (run-reliability RR4); supersedes ADR-0005 in part |
| 0041 | [Jev as an approval delegate, for held calls only](0041-jev-delegated-approval.md) | Accepted (ENG-862); supersedes ADR-0030's "never authorizes side effects" for the `jev_approval` lane only; decision 5's activation superseded by ADR-0052 |
| 0042 | [The trust prompt is a client-side hold fed by the composition root; no protocol change](0042-in-tui-trust-prompt.md) | Accepted (onboarding OB7, ENG-881) |
| 0043 | [Optimize verified task efficiency, not individual turn size](0043-verified-task-efficiency.md) | Proposed (token-efficiency TE0) |
| 0046 | [Pin an MCP server's advertised tool set in configuration and plan identity; enforce the pin at dispatch, not only at discovery](0046-mcp-tool-set-pinning.md) | Accepted (ENG-939); `DESCRIPTOR_VERSION` 9 → 10 |
| 0047 | [Explicit Jev consent and a durable held-approval lifecycle](0047-jev-approval-hold-lifecycle.md) | Proposed (Jev JV1–JV6) |
| 0048 | [A run's bounds reset at its seams, and a stopped unattended run continues itself](0048-run-bounds-reset-at-seams-and-continuation.md) | Proposed (autonomous-core AC2–AC6); extends ADR-0039/0040 |
| 0049 | [A session goal is pursued by the runtime across runs, restarts and days; completion is checked, budgets are enforced, loops are guarded](0049-durable-run-goal-and-loop-guard.md) | Proposed (goals G0–G5; autonomous-core AC4) |
| 0050 | [Composition moves from the binary into a `qq-harness` library; `qq-core` gets tool features and a tested embedding example](0050-qq-harness-composition-library.md) | Proposed (autonomous-core AC10–AC13); refines ADR-0027 |
| 0051 | [0ver product versioning; compatibility is carried by contract versions](0051-zerover-product-versioning.md) | Accepted (ENG-969) |
| 0052 | [Jev approval activation comes from the run's compiled plan](0052-jev-approval-activation-from-plan.md) | Accepted (ENG-971); supersedes ADR-0041 decision 5's per-hold configuration read |
| 0054 | [Progress means output: a run reports when it stops changing things, a sub-agent answers its brief, and delegation does not block the parent](0054-progress-means-output.md) | Proposed (autonomous-core AP1–AP5); takes ADR-0048 § 2's empty-checkpoint fault |
| 0056 | [A compaction summary is a short model narrative plus an exact record that QQ renders from the store](0056-compaction-narrative-and-record.md) | Proposed (compaction CX0–CX5); amends how ADR-0039 § 3's summarizer request is built |

Numbers without a file are reserved in
[`../plans/progress/root.md`](../plans/progress/root.md) for a decision not yet
written, or were released unused:

| ADR | State |
| --- | --- |
| 0016 | Reserved: remote exposure (multi-surface S4) |
| 0017 | Reserved: client UI stack (multi-surface W1/U1) |
| 0018 | Reserved: `apps/` as a separate Cargo workspace (multi-surface U1) |
| 0029 | Unused: native Jev routing was decided in ADR-0031–0034 |
| 0037 | Reserved: responsive TUI layout tiers (tui-redesign U8) |
| 0044, 0045 | Unused: reserved for session mode and strict verification (#187, closed unmerged) |
| 0053 | Unused: reserved for #231's ADR, which was renumbered 0055 |
| 0055 | Reserved: decision-model seam (open PR #249) |

ADR-0030–0034 predate the template and use a short form (status, date,
prose); later ADRs follow [the template](0000-template.md).

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
