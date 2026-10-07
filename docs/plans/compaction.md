# Compaction: fast, cache-aligned, high-fidelity

## Status

| | |
| --- | --- |
| Now | CX0–CX2 merged (#239, #250). CX4 in review; CX3 stacked on it. ADR-0056 Proposed |
| Decision | [ADR-0056](../adr/0056-compaction-narrative-and-record.md) (amends how ADR-0039 § 3's summarizer request is built) |
| Ledger | [`progress/compaction.md`](./progress/compaction.md) |
| Linear | [ENG-992](https://linear.app/retsu-ai/issue/ENG-992) (plan); CX0 [ENG-993](https://linear.app/retsu-ai/issue/ENG-993) … CX5 [ENG-998](https://linear.app/retsu-ai/issue/ENG-998) |
| Related | autonomous-core AC2/AC3 (same summarizer files), AC14 (CX4 takes its compaction-activity item); token-efficiency TE6 ([ENG-893](https://linear.app/retsu-ai/issue/ENG-893)) and summary-quality eval [ENG-807](https://linear.app/retsu-ai/issue/ENG-807); cache determinism [ENG-833](https://linear.app/retsu-ai/issue/ENG-833) |

## Goal

A compaction takes seconds, not minutes, and loses nothing exact:

1. **Fast.** A compaction finishes in under 30 s at p50 on the Anthropic and
   Codex routes in the CX0 baseline, where it takes 110–390 s today. The
   summarizer writes a short narrative, about 1 200 words, instead of
   retyping stored data. It also needs one provider turn instead of a cap
   hit followed by a continuation.
2. **Exact.** Every user message, the last reply, the files read and
   modified, and recent tool failures survive any number of compactions
   byte for byte, or are cited for `search_history` when the bounded record
   is full. Old-format summaries fold into the new format without loss.
3. **Cache-aligned.** A summarizer request reuses the session's system prompt,
   tool declarations and message prefix, so it reads the provider cache that
   the prompt runs wrote. A run's first request extends the previous run's
   last request byte for byte.
4. **Visible.** Clients show that a run is compacting while it compacts.

## Non-goals

- A separate, cheaper compaction model. Rejected in ADR-0056: it cannot read
  the session's cache, and the narrative is short enough that the session
  model is fast.
- Changing reasoning effort for compaction. The summarizer inherits the
  session's effort so the request stays cache-compatible.
- Summary-quality scoring. That is ENG-807's evaluation; this plan gives it
  an exact record to check retention against.
- Bound changes owned by autonomous-core AC2/AC3: step budgets, reservation
  re-base, and summarizer turn recovery.

## Task index

Slices are stacked PRs in this order. Owned paths follow autonomous-core's
rule: a slice also owns every consumer that its type or wire change forces
to move in the same PR.

### CX0 — Plan, ADR-0056, ledger and live baseline
**Inputs:** none
**Owned paths:** `docs/plans/compaction.md`, `docs/plans/progress/compaction.md`, `docs/adr/0055-*.md`, index rows in `docs/plans/README.md`, `docs/plans/progress/README.md`, `docs/adr/README.md`, the ADR reservation in `docs/plans/progress/root.md`
**Gates:** none (docs)
**Acceptance:** the baseline query reproduces the ledger table read-only on the lead's store
**Docs:** this plan, ADR-0056

### CX1 — Narrative summary plus a mechanically rendered record; resolved output cap
**Inputs:** CX0
**Owned paths:** `crates/qq-core/src/sessions{.rs,/compaction.rs,/in_run_compaction.rs,/claim.rs,/execution.rs,/context.rs,/store.rs}`, `crates/qq-core/src/sessions/tests{.rs,/compaction.rs}`, the summary fixture in `crates/qq-core/tests/support/soak.rs`
**Gates:** `context_assembly` within noise for typical summaries, recorded in the ledger; the record renders only at the compaction commit, never during assembly
**Acceptance:**
- The summarizer is told to write five sections (Intent; Decisions and
  constraints; Work state; Open problems; Next step) and not to restate the
  record. Validation requires those five sections and checks only the
  model's narrative.
- At commit, QQ renders the record from stored rows and stores it after the
  narrative in the same `session_compactions.summary` row. No schema change.
  Between runs, the record holds every user prompt and applied steering
  message at or before the cutoff (newest kept first, at least about
  39 KiB), the last assistant reply (8 KiB), files modified and read
  (12 KiB), and failed tool calls (4 KiB, newest first). The whole record is
  at most 64 KiB, and at most an eighth of a declared window.
  In-run, it holds the steering, files and failures of the replaced turns.
- Omitted user prompts are listed by ordinal for `search_history`;
  omitted steering is counted.
- Five successive compactions keep every user message verbatim through the
  record alone, with a summarizer that never retypes one.
- An old-format summary folds into the new format, including the case where
  the record makes the assembly larger than the old summary did: between
  runs, shrinkage is required of the narrative, and the record is a fixed,
  bounded cost. In-run steps count the record toward shrinkage.
- The summarizer requests the run's resolved output cap, held to an eighth
  of a declared window (never below 8 192); the fixed 8 192 clamp is gone.
  The live in-run splice and replay render the same bytes.
**Docs:** `design/tools.md` § Context Budget, `design/protocol.md` § compaction, ADR-0056

### CX2 — Cache-aligned summarizer requests
**Inputs:** CX1
**Owned paths:** `crates/qq-core/src/lib.rs` (`RunCapabilities`, `Runtime::summarize`, the in-run call site), `crates/qq-core/src/sessions/{execution,in_run_compaction}.rs`, `crates/qq-core/src/runtime/compaction.rs`, the soak fixture's summarizer detection
**Gates:** `context_assembly`, `turn_overhead` within noise
**Acceptance:**
- Between-run and in-run summarizer requests carry the prompt run's
  `PromptPrefixKey`, system prompt and tool declarations. A golden test
  shows that a summarizer request extends the preceding prompt request
  byte for byte, up to the instruction.
- Declared tools are denied. At most one rejected-call turn is allowed
  before the step fails closed.
- Context sources and the output-contract notice are not re-sent.
**Docs:** ADR-0056 § Consequences (as built), `design/architecture.md` run loop step 3

### CX3 — Durable prune watermark
**Inputs:** CX2
**Owned paths:** `crates/qq-core/src/sessions/{transcript,context}.rs`, schema (41 → 42), `crates/qq-core/src/lib.rs` (live overflow prune)
**Gates:** `context_assembly` within noise
**Acceptance:**
- Assembly stubs stale read-only results only up to a durable watermark.
  The watermark advances only at seams: the live overflow prune and the
  proactive threshold.
- A golden test shows that each run's first request extends the previous
  run's last request byte for byte when no seam fell between them.
- The schema upgrade test runs from 41.
**Docs:** `design/tools.md` § Context Budget; coordinate the schema number with open PRs that touch `store/schema.rs` (#166) through `progress/root.md`

### CX4 — `RunActivity::Compacting`
**Inputs:** CX1 (independent of CX2/CX3)
**Owned paths:** `crates/qq-protocol` (`PROTOCOL_VERSION` 30 → 31), `crates/qq-core/src/sessions/{execution,in_run_compaction}.rs`, `crates/qq-client/src/state.rs`, `crates/qq-tui/src/{app.rs,view/chrome.rs}`, `src/headless.rs`
**Gates:** none beyond workspace tests
**Acceptance:**
- Between-run and in-run compaction publish `Compacting` and clear it.
- Reducer tests, headless goldens, and one TUI snapshot are included.
- This slice takes the "in-run compaction activity" item of
  autonomous-core AC14, recorded as a root request.
**Docs:** `design/protocol.md`, `design/transcript.md`; coordinate the protocol bump with open PRs #166/#170 through `progress/root.md`

### CX5 — Live qualification
**Inputs:** CX1–CX3
**Owned paths:** ledger only
**Acceptance:** the CX0 query, re-run on the lead's store after 7 days of
normal use, records p50/p95 wall time, output tokens, turn count and
cache-read share per route against the baseline. Goal 1 is met or the gap
is recorded with its cause.

## Order

```text
CX0+CX1 ─ CX2 ─ CX3 ─ CX5
     └─── CX4
```

CX1 lands first because it removes most of the output, and output is most
of the latency. CX2 and CX3 are the cache work; CX4 can go in parallel with
them.

## Coordination

- autonomous-core AC2/AC3 edit the same summarizer path
  (`sessions/{claim,compaction,in_run_compaction,store,execution}.rs`).
  Both are Planned. Whichever lands second rebases; neither changes the
  other's contract.
- goals G0 renders the goal verbatim after every compaction. The record
  does not include the goal: G0 owns that rendering.
- TE6 experiments on obsolete-evidence projection in the same assembly
  code as CX3. CX3 ships the watermark; TE6 measures against it.
