# Speed-First Extensible Agent Harness Backend

## Status

| | |
| --- | --- |
| Now | No implementation phase is active. Phases 0–6 are closed (see Completed Phases). Phase 7 (H10) waits on R6 in `terminal-bench-readiness.md` and a platform threat model; Phase 8 (H11) waits on a real client; Phase 9 (H12) waits on both |
| Next | Quiet-host recordings, not code: the Phase 5a H0 tail comparison and the H20 eight-stream p95 (then tighten the executable budget 50→20 ms). The seven H22 deferrals in § H22 deferrals are the only unscheduled code items from this plan |
| Open gates carried | Eight-stream output service gap ≤20 ms at p95 (median met by H20); Phase 5a full H0 tail acceptance on a quiet host; native Windows full-workspace run (carried to Phase 7 per decision #3) |
| Last closed | Phase 6, 2026-09-16 (#46 `486926b`, #47 `c7fd5c4`, ADR-0026) |
| Versions | Authoritative in [`protocol.md`](../design/protocol.md) § Versioning; at close of Phase 6 (2026-09-16, before T8's bump to 21): `PROTOCOL_VERSION` 20, `CAPABILITIES_VERSION` 1, `DESCRIPTOR_VERSION` 6, store schema 28, H0 fixture version 4 |

Updated 2026-09-16. The `Now` row is authoritative for what is being worked;
update it in the same PR that ships or reprioritizes work.

This plan defines how QQ becomes an extremely fast, lightweight, customizable
agent harness that can serve as the backend for products such as a
Hermes-style personal agent. It is a backend plan, not a plan to copy every
product surface from Codex, OpenCode, Pi, fx, or Hermes into QQ.

The central decision is:

> Compile customization once, execute directly in the hot path, persist before
> publishing, and keep every queue and concurrency boundary explicit.

QQ is a small durable execution kernel with a compiled customization plane.
Messaging gateways, cron, voice, browser automation, product identity, and
user-facing memory products remain clients of that kernel.

Phases 0–6 shipped. The system they built is described in
[`docs/design/architecture.md`](../design/architecture.md) (§ Extension
Contract and § Performance Discipline carry the design rules and targets this
plan introduced); decisions are in `docs/adr/` (0001–0014, 0022–0027);
receipts are in [`progress/speed-first.md`](./progress/speed-first.md) and Git
history. This file keeps only what is still open.

## Open Work

### Quiet-host recordings

No code is scheduled. Two quiet-host recordings remain (I/O pressure
`some avg10` well under 20 %); each closes with one line in its Completed
Phases row:

- Phase 5a: the full version-4 H0 baseline/candidate comparison, baseline
  `1c08cef` (pre-H23 `main`), candidate current `main`.
- Phase 6: the eight-stream mixed control/output fixture, p95 ≤20 ms; then
  tighten the executable budget in `budgets-v1.json` from 50 ms to 20 ms.

Native Windows: the targeted CI job (`windows-teardown`) passes; a full
Windows workspace run is carried to Phase 7 (decision #3).

### H22 deferrals

Cold-path and structural items. H22.1 shipped the correctness items in #22;
H22.2 shipped the rest in #46 and #47 (receipts in the ledger), except the
items below, each deferred with its reason. These are the only unscheduled
code items left in this plan; each is small enough to be its own slice when
a reason to open that code appears.

- *deferred* `StaticHttpAuth` replacing the `HttpAuth` arms and four
  `build_headers` copies: `HttpAuth` is public and re-exported; collapsing it
  is a compatibility change to the provider recipe surface, and the four
  copies differ in the per-protocol header-safety checks they enforce. Worth
  its own slice with an ADR, not a line in a bundle.
- *deferred* headless output off the Tokio worker: `headless::run` and
  `output::render` take `&mut impl Write` and 1,300 lines of tests drive them
  with `Vec<u8>`; moving to a writer task changes exit-code-on-write-failure
  and stdout/stderr interleave semantics. Needs its own contract test.
- *deferred* config parse-once-per-load: three `Document::parse` calls per
  project source (organization, trust, merge) with ordering and trust-gating
  logic between them; restructuring is a correctness risk for a cold path.
- *deferred* reviewer through `PlanCache`: the reviewer's own epoch-keyed
  cache ignores config-file changes until a credential rotates; routing it
  through `PlanCache` fixes that but is a behavior change, not structure.
- *deferred* `TurnMode`/`StreamEnd` enums and parse-tool-arguments-once: both
  live in the 1,600-line `execute` body and the argument text is embedded
  verbatim in the transcript; do them when the run loop is next opened for a
  behavioral change.
- *deferred* `Arc<[RuntimeToolCall]>` for `ModelTurnCommit.calls`: one clone
  per model turn; the store worker needs owned data regardless.
- *not done, by measurement* the approval-wait `sleep_until` per iteration:
  that loop iterates once per reviewer verdict, not per tick.

## Gated Phases

### Readiness Dependencies

Milestones owned by `terminal-bench-readiness.md`. R4 and R5 shipped and were
imported in Phase 1.

| Milestone | Owning phase | Required outcome here |
| --- | --- | --- |
| R6 | Tool tournament and terminal | Any richer search/edit/terminal contract has won its ablation and cleanup gates; gates H10 |
| R7 | Sub-agent economics and scheduling | Child accounting/admission behavior is measured and remains bounded |
| R8 | Warm runtime and request efficiency | Credential-lease caching, MCP bounds, provider prompt-cache determinism. Retry exposure, shared message storage, and request-encoding benchmarks were handed to H14 (shipped) and H18 |

| Task | Status | Outcome | Depends on |
| --- | --- | --- | --- |
| H10 | Gated | First real OS process-sandbox adapter | R6 (tool-layer T13 evidence, T10 decision), platform threat model |
| H11 | Gated | Optional ACP/OpenAI compatibility facade | A real consumer |
| H12 | Gated | Crash, load, security, quality, and performance qualification | H10, H11, and the R6–R8 milestones a shipped extension requires |

### Phase 7 — Execution Quality And Isolation

Implement H10 only after R6 has selected and shipped a real terminal/process
contract and a platform threat model defines the isolation boundary. This
document does not redefine the search, edit, terminal, sub-agent, scheduling,
or warm-runtime contracts.

Acceptance: the readiness plan records completion evidence for each R6–R8
milestone a shipped extension requires; sandbox tests prove filesystem,
network, process, and secret boundaries; local and sandbox adapters pass one
shared process contract suite; sandbox failure never silently falls back to
unsandboxed execution; terminal/process cancellation, timeout, shutdown, and
recovery leak no processes; the sandbox adapter is optional and feature-gated
where its platform dependencies are not needed.

### Phase 8 — Product Adapters On Demand

Implement H11 only for an actual client (ACP, OpenAI-compatible HTTP,
messaging gateway, cron, voice, browser/desktop, or a product memory service).

Acceptance: the adapter uses `qq-client` or the native HTTP protocol;
introduces no alternate runtime or direct store access; native QQ events
remain authoritative; capability loss in the compatibility protocol is
documented and tested; product auth and tenancy remain outside `qq-core`; the
adapter can be disabled without affecting the base binary's hot path.

### Phase 9 — Qualification

H12 qualifies the complete story. Each preceding repair already carries its
own fault, cancellation, and recovery fixtures; H12 is the combined-system
qualification. It re-runs every Phase 5–6 pre-change baseline and enforces the
recorded improvements as regression gates in `budgets-v1.json` or a successor.

Required scenarios: cold and warm direct/TUI/server execution; 1/10/100
concurrent sessions; long text and reasoning streams; provider failure before
send and ambiguous failure after send; store saturation, disk-full,
corruption, migration, and restart; client disconnect/reconnect during text,
tool, approval, and terminal work; addon discovery failure, refresh failure,
crash, timeout, overload, and shutdown; context-source slowness and stale
cache; MCP and embedded tool conformance; cancellation at every
provider/tool/sub-agent boundary; terminal process cleanup; sandbox escape
attempts; same-model agent-quality comparison; minimal/full binary, RSS,
startup, and latency gates.

## Completed Phases

| Phase | Tasks | Closed | Revision | Landed |
| --- | --- | --- | --- | --- |
| 0 — Speed constitution | H0 | 2026-09-01 | `6383305` | `cargo xtask perf baseline/check`, versioned JSON reports, `budgets-v1.json`, deterministic fake-provider fixture with 1/10/100-session load and RSS sampling |
| 1 — Prerequisites and profiles | R4, R5, H1 | 2026-09-02 | `5bb1471` | Linear/fair streaming and resolved model, context admission, `RunLimits`, and compaction hardening imported from the readiness plan; `provider-bedrock` gates the AWS crates |
| 2 — Compiled plan | H2 | 2026-09-02 | `2d2ba3b` | `AgentProfile`, secret-free `AgentPlanDescriptor` with canonical digest, runtime-only `CompiledAgentPlan`, `SourceFingerprint` revalidation, `CredentialEpoch`, root `PlanCache` (16 entries / 64 MiB, LRU, pinned generations, single-flight) |
| 3 — Backend contract | H3, H4 fixtures | 2026-09-03 | `dfaebb9` | Protocol 13, schema 21: `InputPart`, `Correlation`, `AgentProfileId`, `RunPlanIdentity`, `SteerRun`, `SetSessionProfile`, expanded `RunLimits`, `ServerCapabilities`, config `profiles`, 24 golden fixtures |
| 4 — Extensions | H5–H9 | 2026-09-03 | `f02cfc9` | Protocol 14: immutable `ToolCatalog` with progressive exposure; `ExternalToolHost` with `EmbeddedToolHost` and a shared conformance suite; `pack.ron` packs; bounded `ContextSource`; `qq-client::observer`; `ToolSpec` behind `Arc` |
| 5 — Correct the hot path | H13–H17 | 2026-09-04 | `ea5a6af`…`70166bd` | Effect-classified approval; provider-owned retry (descriptor 4→5); published-event outbox; output-lane group commit with a 128-statement cache; schema 25 with `runs.activity`, command counter, grouped snapshot accounting, two-hop claim, joined context assembly. Amplification measured 1.000; fan-out to slowest of 32 26.4→14.9 ms; `store_output_batch` 236→138 ms. The ≤20 ms service-gap gate was **not met** (29–45 ms) and is carried to H20 |
| 5b — Headless contract | HC1–HC4 | 2026-09-13 | `abad2de`, `24b6e5c`, `43caaea` | `--correlation`/`--session` behind a per-store owner lock (ADR-0022), `u32` turn limits (protocol 18); `--output-schema` with a bounded schema subset compiled at admission and judged after audit/steering, `FinalOutput` on `RunFinished` (protocol 19, schema 27, ADR-0014); headless record types in `qq-protocol` with golden JSONL per protocol version (ADR-0023). HC2 (`policy.exposed_tools`) shipped earlier in `893e582`. Gate file: `progress/g-phase-5b.md` |
| 6 — Finish fairness, shrink per-run work, consolidate | H18–H22, H27, H28 | 2026-09-16 | `61682be`…`c7fd5c4` | H20 control admission + shared commit (gap median 20 ms; ADR-0011); H27 plan-cache generation accounting; H28 context sources in the descriptor (descriptor 6, ADR-0013); H21 `RunIdentity`/`PersistenceFault`/one guarded `settle_run` (ADR-0012) and the mechanical `sessions.rs` split; H18 shared transcript, raw tool JSON, prompt prefix (heap 4.5x→1.6x; ADR-0024); H19 per-chunk SSE framing (decode 0.4x; ADR-0025); H22 correctness and structural bundles with `RunCancellation` (ADR-0026). Route-table equality test is the acceptance gate. Open: quiet-host p95 and the seven H22 deferrals. Gate file: `progress/g-phase-6.md` |
| 5a — Repair shipped contracts | H23–H26 | 2026-09-07 (tail acceptance open) | `1e6a901`, `f482b37`, `893e582` | H23 child ownership across admission/overload/steering/cleanup with fail-closed teardown; H24 per-admission child budgets, deadline carry, owned-descendant spend, `child_admission` bench; H25 exact redacted live credential bindings separate from durable identity, eager MCP after admission, pre-read source evidence; H26 validate-then-attach feeds with lease reclamation (retained RSS after 4096 rejected subscribes 135 MB→0), then the sequence-indexed feed ring (`cursor_replay` 22.6→0.87 µs median, 2001→1 store reads) and release profile (`strip`, `codegen-units = 1`, thin LTO; minimal binary −30.5%, default −33.0%; budgets tightened to 41/48 MB). Focused R4/shell/cache comparisons pass their gates; full H0 tail gates are not repeatable on the shared host (A/A fails the same set) and remain retained, not waived. Native Windows teardown runs as a targeted CI job (`windows-teardown`); full native qualification is not claimed |
