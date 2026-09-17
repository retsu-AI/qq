# QQ harness audit: reliability, efficiency, and scale

Research snapshot: 2026-09-16, QQ `7956e8e5570eeec0be636eb554893bd6088e26f1`.
This document records an audit and proposes work; it does not authorize or
claim implementation. It supersedes the August 2026 reference audit and the
September 2026 per-feature catalog (both deleted; see Git history before
`445d740`).

Implementation status lives in the ledgers, not here. At the time of writing
F01 (#55), F02 (#57), F14 (#63), and the context-usability stack C1–C6
(#56, #58, #59, #61, #64) had merged. C2 ships proactive compaction and
mid-run stale-read stubbing and explicitly defers full mid-run summarization,
so it is not full closure of F03; C4 ships provider cache breakpoints, not
every generation/cache control in F17. Findings below are evidence at the
pinned snapshot, not a claim that repaired defects persist on `main`.

## Assessment

QQ has a useful architecture for a fast harness: one shared runtime, compiled
immutable plans, provider-owned transport and retry, bounded tool execution,
durable commands/events, and clients that consume the same protocol. Preserve
that shape. A rewrite or a universal plugin framework is not justified by this
audit.

The immediate obstacle to full-time use is continuity and reliability. The
audit reproduced incorrect tool-result replay, a wall-clock limit that fails
to interrupt a running shell, and loss of attached file content from the next
prompt's reconstructed context. Long-running tasks also cannot compact at a
later turn boundary. Fix these before adding more autonomous work on top.

Speed has three separate meanings here: local overhead, model/network time,
and time or tokens spent recovering from mistakes. QQ has invested heavily
in the first. Context loss, unnecessary retries, missing cache controls, and
repeated discovery can dominate the other two. The primary comparison should
be **time, tokens, and cost to an independently verified useful result**, with
local latency and memory budgets enforced alongside it.

There is no evidence for an absolute claim that no competing harness has an
advantage. These snapshots have substantive advantages in interactive process
control, process sandboxing, provider cache/continuation controls, visual input,
scoped instructions, language intelligence, reusable agent collaboration, and
client integrations. Close the relevant gaps through small core contracts and
optional adapters, clients, and supervisors. Feature counts alone cannot
establish superiority, and this audit did not benchmark the reference runtimes.

## Scope and confidence

Three read-only agents investigated QQ core reliability, Codex/OpenCode, and
Pi/fx plus provider/config/MCP behavior. The coordinating audit checked the
architecture, crate graph, clients/server, packaging, CI, plans, and selected
findings independently. All workspace crate families and all four reference
roots were inventoried. Investigation followed implementation and tests in
each capability family; it was not a line-by-line review of every source file.

| Tree | Exact inspected revision | Interpretation |
| --- | --- | --- |
| QQ | `7956e8e5570eeec0be636eb554893bd6088e26f1` | Clean starting checkout; local `main` and `origin/main` agreed |
| `.source/codex` | `0df6366a87dbadf5376cdd26b7675935ef76f893` | Local source snapshot; some capabilities feature gated |
| `.source/opencode` | `b6914b39db86e196ebcc95e92a0188cdf58ef67a` | Distinguish V1 `packages/opencode` from V2 `packages/core` |
| `.source/pi` | `400d6905ce46ec46e79da8a7701b1b48850192df` | Coding agent plus newer agent/harness/client packages |
| `.source/fx` | `69d0e9610c7958ae2429d3a1ff40a509e3340781` | Zig harness and its tools, permissions, sessions and terminal code |

Reference trees were inspected without updating them. The conclusions describe
these revisions, not current upstream releases. Reference implementations are
capability evidence, not templates to copy or proof their guarantees hold.

Evidence labels used below:

- **R — reproduced:** exercised current QQ behavior with a deterministic local
  diagnostic or an exact existing test.
- **S — source-confirmed:** a concrete implementation path establishes the
  behavior; no new end-to-end reproduction was run for it.
- **G — capability/qualification gap:** missing or limited capability, or an
  unproven performance/quality guarantee; not automatically a bug.
- **H — hypothesis:** a risk requiring a focused test before implementation.

No live model calls, paid evaluations, reference builds, native Windows runs,
or full workspace qualification were performed. Linear's read-only issue query
failed because its connection requires reauthentication; no issue IDs or live
issue states are inferred. Local Git history establishes the inspected code,
not hosted CI state. No runtime code is changed by this audit.

## What is already strong

Do not reintroduce the following as missing features from the older catalogs:

| Existing capability | Current implementation | Why it matters |
| --- | --- | --- |
| Shared runtime and client separation | `src/runtime.rs`; `qq-core`; `qq-client::state`; `qq-server`; `qq-tui` | Core changes benefit direct, durable headless, and interactive use |
| Compiled plans and shared immutable requests | `src/plan.rs`; `qq-core/src/plan.rs`; `runtime/prompt.rs`; `qq-provider/src/model.rs` | Avoid repeated config, schema, prompt-prefix and transcript work |
| Durable run ownership, settlement and replay | `qq-core/src/sessions/{claim,settlement,store,feed}.rs` | Persist-before-publish and explicit interruption avoid replaying uncertain side effects |
| Efficient built-in reads and search | `tools/{read,search,tree,lang}.rs` | Multi-range reads, hash short-circuit, outlines, ignore-aware search, paging and tree views |
| Rich edits and output handling | `tools/{edit,write,output}.rs`; `runtime/spill.rs` | CAS, batching, anchors, dry-run, bounded model output, display separation and durable spill retrieval |
| Shell policy and exact argv execution | `approval/{classify,rules}.rs`; `tools/shell.rs` | Parsed policy, Forbidden decisions, cleared environments and an argv path; still not an OS sandbox |
| Structured user interaction and mentions | `tools/ask.rs`; `qq-protocol/src/{input,mentions}.rs`; TUI composer | `ask_user`, ranges, directory/glob mentions, Git references and completion |
| Steering and bounded delegation | `runtime/steering.rs`; `sessions/{execution,subagents}.rs` | Interrupting/queued steering, supervised write children, inherited budgets and cancellation |
| Structured output and host integration | `output.rs`; `src/headless.rs`; `qq-protocol/src/headless.rs` | Bounded output validation/repair, typed outcomes, correlation and versioned JSONL fixtures |
| Real extension seams | `hosts.rs`, `hosts/embedded.rs`, `context_source.rs`; `qq-client/src/observer.rs` | MCP/embedded tools, bounded retrieval and post-commit observers already exist |

Delegation depth is configurable up to three, with defaults and authority
restrictions; see `sessions/execution.rs:269`. The older comment in
`runtime/subagent.rs:90` saying child sessions get no spawner is stale. QQ
also already reserves a tool-free final response near certain countable
budget limits. Neither nesting nor budget-final responses is a new gap.

Here “CAS” follows the repository's shorthand for hash preconditions plus QQ's
process-local workspace apply lock. It is not an atomic filesystem
compare-and-swap against arbitrary external editors; see
`workspace/access.rs:97` and `tools/edit.rs:303,741`. New restore/integration
contracts must explicitly handle concurrent changes outside QQ too.

Paths without `.source/` or a leading `src/` in this section are under
`crates/qq-core/src/` unless a crate is named explicitly.

## Reliability findings

Priority describes proposed order: P0 blocks trustworthy unattended use; P1
blocks dependable daily use or efficient scale; P2 improves capability or
maintainability. It is not a security severity score.

### F01 — Tool results collide across turns during replay

**P0 · R · core correctness.** `sessions/transcript.rs:181–209` stores results
in a map keyed by run and provider call ID, dropping the turn ordinal.
`append_run_turns` removes each entry by call ID at `:654`. The database correctly
allows the same ID in different turns (`sessions/store/schema.rs:1176`). Google
can generate the same `call_0_<tool>` ID in successive response streams
(`qq-provider/src/providers/google.rs:638`).

Reproduction: one successful run reads `a.txt` and `b.txt` in two turns, both
using `call_0_read_file`; submit a follow-up. The first replayed result contains
`BETA_LATER` from `b.txt`; the second becomes an interrupted-tool error despite
both calls having succeeded. This can corrupt follow-up, restart and compaction
context. The particular result retained depends on query iteration; either
collision is incorrect.

**Fix:** preserve full call identity through reconstruction, including run,
turn, and call identity. Audit pruning as well: `transcript.rs:199–204` records
read-only eligibility in a set of bare call IDs, so repeated IDs can also
misattribute the effect of another call. Do not solve only the Google adapter;
provider IDs need not be globally unique across turns.

**Acceptance:** repeated IDs within a run and across runs, mixed tool effects,
follow-up, reopen, compaction and history search all retain the right result.
Measure assembly allocation/time to ensure the stronger key adds bounded cost.

### F02 — Run duration does not interrupt tool execution

**P0 · R · core budget/cancellation.** The run deadline participates in the
provider-stream select (`qq-core/src/lib.rs:1310–1347`), but the approval and
tool-await paths do not select on that same deadline (`:2054`, `:2429`).

Reproduction: submit a run with `max_duration_ms=100`; its first turn invokes
`shell` with `sleep 1` and a two-second tool timeout. The run reports duration
exhaustion only after approximately **1,075 ms**, after the shell finishes.
This is time spent executing past the budget, not merely post-cancellation
cleanup. The analogous approval/host cases need their own regressions.

Source inspection also shows the clock starts in `qq-core/src/lib.rs:990`,
after session loading and attachment resolution, despite the adjacent admission
comment. Re-preparation after automatic compaction constructs a new meter.
Include these paths in the fix: a per-provider timeout alone cannot enforce
one run-wide budget. Cancellation during attachment resolution currently drops
the `spawn_blocking` waiter (`sessions/execution.rs:203–215`); a started blocking
operation must remain owned until it exits before the session is released.

**Fix:** carry one absolute deadline through preparation, provider work,
approval waits, tool/host calls, child waits and completion repair. On expiry,
request cancellation, drain owned work, then publish the typed terminal result.
Keep cleanup completion distinct from the execution deadline.

**Acceptance:** a hanging provider, sleeping shell, pending approval, blocked
host and nested child each begin cancellation at the same deadline; settlement
waits for cleanup and occurs once. Test explicit run limits and inherited child
limits separately, with delayed preparation and saturated store lanes.

### F03 — A long run cannot compact and continue at a tool boundary

**P1 · S/G · core continuity.** `sessions/execution.rs:1429–1454` selects
`CompactionDisposition::BetweenRunsOnly` for later prompt turns. On exceeding
the estimate, it stops the run instead of compacting and continuing.
`sessions/context.rs:211–213` exposes that reason. An existing test pins this
behavior; this is a known limitation requiring a deliberate design change.

**Fix:** support a durable checkpoint and compaction at a safe turn boundary,
with no in-flight tool replay and no new user-level run. Preserve goals, recent
steering, tool call/result pairs, owned-child state, spend and output-repair
counters. The 256-call execution-slice checkpoint is not context compaction.

**Acceptance:** a scripted task exceeds several context windows and completes
the same run through multiple compactions; cancellation and crash injection at
each boundary retain valid history and exact accounting. Report compaction
tokens, pause duration, retained obligations and task success.

### F04 — Conservative context estimates can leave compaction unable to start

**P1 · S/H · core/provider admission.** Without compatible measured occupancy,
`sessions/context.rs:86` treats each request byte as an input token. A separate
4 MiB storage backstop reserves 32 bytes per output token (`:1–4`, `:88–105`).
Automatic compaction clears occupancy and checks the summarization request as
`AlreadyAttempted` (`sessions/execution.rs:762–788`). The content requiring
compaction can therefore also be rejected before the summarizer is called.

The conservative estimate is intentional, not proof of incorrect arithmetic.
The gap is a dependable route from an overfull request to a bounded summary,
especially after a model/prefix switch or pruning invalidates measured reuse.

**Fix:** retain provider-specific occupancy evidence safely, add a bounded
estimator where supported, and make summarization fit by construction using
chunked reduction or a bounded retained suffix plus durable recall. Keep byte,
token, output-reserve and storage-limit reasons separate in diagnostics.

**Acceptance:** byte-heavy text, code, Unicode, large tool schemas, model
switches, unknown usage and overflow retries either recover or return an
actionable irreducible-input diagnosis. Never loop on identical rejected input;
never silently discard commitments. This audit did not reproduce every
dead-end variant.

### F05 — File attachments disappear from reconstructed context

**P1 · R · core input provenance.** `sessions/execution.rs:188–239` expands
attachments into the live run's prompt. The durable user message retains an
`@path` placeholder; this is explicitly asserted in
`sessions/tests/commands.rs:556–598`. On the next run, reconstruction has the
placeholder, not the exact content that the model saw.

Reproduction: attach `a.txt` containing `ALPHA_ORIGINAL`, complete a run, then
send `continue`. The first provider request contains the file bytes; the second
contains `inspect\n@a.txt`, `done`, `continue`, with no original file content.
This is current intentional storage behavior with a continuity gap, not a claim
that mentions themselves are absent.

**Fix:** persist a bounded immutable attachment artifact/reference with path,
hash, selected range and the exact resolved bytes or an explicitly evicted
state. Reconstruct what was originally observed; rereading the current file is
a separate action. Apply the same principle to source context and images.

**Acceptance:** follow-up, reopen and compaction after file modification or
deletion can cite/retrieve the original observation within retention policy.
Deduplicate bytes and avoid duplicating attachment payloads in every snapshot.

### F06 — Compaction does not bound history assembly or recall work

**P1 · S · core/store performance.** Prompt selection honors the compaction
cutoff, but `sessions/transcript.rs:156–233` loads turns, tool results and
steering for the whole session into maps. Old history is allocated before it
is discarded. `search_session_history` at `:485–592` also collects historical
prompts and performs nested reads/scans; a rare or absent term traverses the
archive. These operations run through the shared store worker.

**Fix:** constrain assembly queries to retained run/turn ranges. Give historical
recall bounded pages/scan budgets and a continuation; evaluate indexed recall
and a separate bounded read connection if measurements justify it. Preserve
consistent snapshots and ordering. A result-count cap is not a scan-work cap.

**Acceptance:** compare the same active context with 10, 1,000 and 10,000
archived turns. Assembly RSS/time should follow the retained context; history
search must not make another session miss command/cancellation service budgets.
Measure absent-term search, cold/warm caches and concurrent streaming.

### Additional defects and qualification gaps

| ID | Priority / evidence | Finding and source | Proposed change and focused acceptance |
| --- | --- | --- | --- |
| F07 | P0 at exhaustion · S | `MAX_COMMANDS=100_000` (`sessions.rs:139`); `sessions/commands.rs:325–331` rejects every new command before dispatch, including cancel, approvals and delete. Counter only increases (`:1452–1461`); no command-retention path found. Replayed old IDs still work. | Reserve control/cleanup admission independently of new-work limits; introduce documented idempotency retention/archive semantics. Fill to the cap in a temporary DB and verify cancel, approval resolution, shutdown and cleanup still settle; do not simply delete old receipts and permit replayed side effects. |
| F08 | P1 · S | Retry-After is capped by client max delay, then full-jittered down toward zero (`qq-provider/src/http.rs:124–127,170–181`); HTTP-date form is ignored (`:246–250`). | Treat a supported server minimum separately from randomized client backoff. If it exceeds the remaining deadline, stop/defer instead of retrying early. Fake-clock tests for 429/503, long minimum, cancellation and concurrent sessions; report sends per successful turn. |
| F09 | P1 · S | Approval reviewer cache only checks credential epoch before returning (`src/runtime.rs:1306,1334–1345`); a config edit/removal does not invalidate the provider, route or pricing. Its workspace map has no explicit eviction. | Reuse configuration fingerprints and bounded cache admission. Change/remove reviewer model, endpoint and pricing without rotating credentials; next review must observe the change. Bound workspace churn and refresh storms. Existing H22 deferral is not evidence this is safe. |
| F10 | P1 · S | Client `post_json` times out `.send()` but reads the response body afterward with no deadline (`qq-client/src/lib.rs:373–387,456–467`). An incomplete body can hold a client request permit indefinitely. | Bound the complete JSON exchange, preserve timeout versus size/transport errors, and test headers followed by a stalled or slowly dripped body. SSE already has a separate idle deadline; retain streaming semantics there. |
| F11 | P1 · H | Snapshot assembly is count-bounded, not encoded-byte-bounded (`sessions/snapshots.rs:95–170`), while the client refuses more than 8 MiB (`qq-protocol/src/limits.rs:10`). Bootstrap requests a focused body plus four prewarmed sessions (`qq-client/src/interactive.rs:510–526`). Legal large messages/tool results may make reconnect fail. | Reproduce with large histories; add byte-budgeted projections and paging/artifact references. Large-session attach and resnapshot must work within the wire cap without materializing an oversized body first. |
| F12 | P1 · S/H | HTTP request semaphore is acquired in `decode_bounded` after Axum has extracted `Bytes` (`qq-server/src/lib.rs:759–779`). Individual bodies have a cap, but the 64-request permit does not bound concurrent incoming body buffers. | Put admission before body aggregation and bound slow readers/connections for remote serving. Load-test many partial authenticated bodies; verify bounded RSS and fast rejection/cancellation. Impact magnitude not measured here. |
| F13 | P1 · S | MCP `list_all_tools()` collects before catalog budgets, with a linear duplicate search per tool (`qq-mcp/src/lib.rs:413–447`). Result rendering builds full strings before core bounds (`:571–599`). | Bound catalog pages, tool count/schema bytes and response decoding at the adapter/transport, before allocation. Test giant/paginated catalogs, duplicates and oversized text/structured content; measure peak RSS. Core truncation alone cannot bound upstream allocation. |
| F14 | P1 · R/S | Five Windows CI exact selectors still name `sessions::tests::<name>` after tests moved under `sessions::tests::delegation` (`.github/workflows/ci.yml:64–74`). One stale selector reproduced **0 tests, success**; its corrected path ran **1 test, pass** on Linux. | Update selectors and fail CI when an intended test selector matches zero cases. Re-run all intended native Windows cases; Linux evidence is not Windows acceptance. |
| F15 | P1 · G | CI reports release size only on main; it does not enforce `budgets-v1.json`, run minimal-provider tests, or run browser wasm tests (`.github/workflows/ci.yml`). Full H0 quiet-host tails and the 20 ms fairness target remain open in the speed-first ledger. | Enforce deterministic packaging/profile checks on PRs; execute browser tests; maintain controlled-host latency gates separately from noisy hosted runners. Record exact build, sample count and correctness receipts. |
| F16 | P1 · G | Native shell/exec lacks OS filesystem/network isolation (`tools/shell.rs:252–277`). Parsed policy and capability file tools do not constrain arbitrary code spawned by shell. | Deliver H10 platform sandbox adapter with explicit supported guarantees and egress policy. Keep platform dependencies optional; test path escape, process cleanup and launch cost. Do not make basic isolation wait indefinitely for unrelated paid tool-economics evaluation. |
| F17 | P1 · G | Cache usage is reported, but resolved cache control and reasoning effort are `Unsupported` (`src/runtime.rs:818–825`); `ModelRequest` has no explicit cache-affinity/retention controls. | Implement small provider-neutral capability/configuration controls translated by adapters. Measure cache hit tokens, TTFT, billed cost and prefix stability across turns/tool selection/compaction; do not imply implicit provider caching is absent. |
| F18 | P1 · G | Visible reasoning events do not preserve opaque provider reasoning state. Anthropic signature/redacted blocks are ignored (`qq-provider/src/providers/anthropic.rs:547–568,1079–1104`); generic content has no signed/encrypted continuation block (`model.rs:271–290`). | Add bounded, provenance-scoped opaque continuation state only where a supported protocol needs it. Test multi-turn tools, restart, model switch and invalid signatures; never send another provider's private continuation blindly. This is a compatibility gap, not proof every existing model call fails. |
| F19 | P2 · S/G | Root `--no-default-features` removes AWS only; it retains unconditional TUI, server, auth and MCP dependencies (`Cargo.toml`). Some fleet/embedding consumers do not need these surfaces. | Define measured build profiles for actual consumers, preserving one runtime implementation. Gate existing adapters/features instead of multiplying crates. Require disabled-profile behavior, dependency, bytes, startup and RSS receipts. |
| F20 | P1 at sustained use · S/G | Workspace limit includes child sessions: 512 (`sessions/commands.rs:133–139,391–397`). Events are retained without a bounded retention policy; historical commands/runs accumulate. | Add explicit archival, export, retention and user-visible approaching-limit signals. Preserve descendants until owning accounting settles and define cursor-expiry/resnapshot behavior. Test long-running daily use and repeated agent trees, not only fresh stores. |
| F21 | P1 · G | Scheduling has bounded root/per-depth pools (`sessions/runtime.rs:354–364,498–501`) but no demonstrated per-provider RPM/TPM admission or coordinated 429 cooldown. | Add measured provider-aware admission with queue-wait/cooldown telemetry and cancellation, keeping durable job DAG/fleet scheduling above core. Test multiple providers, fair sessions, budgeted children and a throttled endpoint. Never let waiting parents consume every permit children need. |
| F22 | P2 · S | Roadmap tables lag local merged history: H22.2 #46/#47 and T8 #49 are present at HEAD but still marked in review; old catalog has shipped features marked absent. | Reconcile plan/ledger status in a bounded docs follow-up using actual revisions and receipts. Do not erase historical evidence or mark outstanding tail/quality gates complete just because code merged. |
| F23 | P1 · S | Live tool results are reduced to a 96 KiB aggregate turn budget only after per-call results have been persisted (`qq-core/src/lib.rs:2488,2538,2582–2605`). Replay loads the larger stored strings. The test at `:4340` explicitly keeps persisted results whole. | Persist or deterministically reconstruct the exact model-facing projection separately from retained full output. Compare live versus follow-up/reopen/compaction requests after four large reads; the turn budget must survive reconstruction and every omitted span must have a defined recall path. |
| F24 | P2 · S | History excerpts locate matches in lowercased text, then use those byte offsets in the original text (`runtime/history.rs:94–109`). Unicode lowercasing can change byte length. | Preserve an offset mapping or search with original-span semantics. Regress repeated `İ`/`ẞ` before a query; every returned excerpt must contain its claimed match and remain bounded/valid UTF-8. |
| F25 | P1 · R | Ranged attachment of an empty file allows start=1 with total=0, then subtracts `end - start + 1` (`input.rs:155–168`). Current dev build reproduces a caught worker panic and a run failure of kind `Server`, message `input resolution stopped unexpectedly`; no provider request is sent. The process remains usable. Release behavior was not tested. | Define empty-file/range behavior and use validated arithmetic. Test empty files, EOF clipping, trailing newline and extreme ranges in debug/release paths; return defined content or a typed input error, never a worker panic. |
| F26 | P1 · S/G | Cold MCP catalog compilation waits for all configured servers (`src/mcp.rs:33–51`; `qq-mcp/src/lib.rs:673–679`), including those otherwise described as lazy. One slow server can hold the aggregate catalog until its timeout. | Measure cold readiness with one fast and one stalled server. Use per-server cached readiness and explicit selection/lazy discovery while preserving frozen run catalog generations; no unrelated tool host should dominate ordinary first-token latency. |
| F27 | P2 · H | Concurrent identical `ContextSource` cache misses start independent fetches (`context_source.rs:308–341`). The cache is bounded; miss work is not coalesced. | First measure duplicate retrieval under many agents. If material, add bounded shared in-flight fetch ownership with independent caller cancellation/deadlines. Do not cache failures indefinitely or merge different authority scopes. |
| F28 | P1 · G | Compaction validation checks structure/shrinkage (`sessions/compaction.rs:9`); the repeated fact-retention test uses a fake summarizer programmed to preserve fact lines (`sessions/tests/compaction.rs:1874`). | Keep deterministic plumbing tests, add opt-in model-quality evaluation for retained obligations, corrections, source citations, pending work and repeated summary degradation. This is missing quality evidence, not proof the deployed summarizer always loses facts. |

QQ core-relative paths in the findings mean `crates/qq-core/src/`.

## Comparative capability matrix

The matrix normalizes capabilities, not product marketing or implementation
languages. **Y** means located in the snapshot; **P** means partial, optional,
feature-gated or available through an extension; **—** means not located in the
reviewed implementation, not a proof of universal absence. **V1/V2** identifies
the OpenCode runtime. Source groups below provide the reference anchors.

Placement: **K** existing core/protocol/provider contract; **A** optional tool,
provider or context adapter; **C** client; **S** supervisor/application. The
last column states what QQ should actually do, including when parity is enough.

### Context, provider behavior, and long-task continuity

| Capability | QQ now | Codex | OpenCode | Pi | fx | Decision / cost / evidence |
| --- | --- | --- | --- | --- | --- | --- |
| Automatic summarization | Between runs; later-turn gap | Y | Y | Y | Y | K: F03/F04 before further autonomy; measure summary cost. C1/O1/P1/X1 |
| Provider-overflow recovery | Evidence persisted; bounded next admission; limitations above | Y | Y | Y | Y | K: verify recovery, avoid repeating old “absent” claim |
| Persisted tool-call/result history | Present with F01 defect | Present | Present; V2 recovery caveats | Present | Present | K: full call identity and public replay conformance; exact reference replay not qualified here |
| Original attachment continuity | Live content, durable placeholder | Attachment support; lifecycle not qualified | Attachment support; lifecycle not qualified | Attachment support; lifecycle not qualified | Attachment support; lifecycle not qualified | K/A: F05 immutable artifacts, bounded retention; no exact reference durability claim |
| Scoped repository instructions | Root AGENTS/CLAUDE first match | Root→cwd chain | Global/upward, lazy scopes V1 | Ancestors + system append | Global/ancestor/root and tool-target scopes | K/A: bounded fingerprinted scoped index; avoid per-turn tree scans. C2/O2/P2/X2 |
| Model-visible context remaining | Internal/UI evidence; no dedicated tool | P: TokenBudget flag | — | Extension interface | Usage UI | K: expose estimates with provenance, not false precision. C1/P1 |
| Explicit fresh context/handoff | Compaction/checkpoint primitives | P: TokenBudget flag | P | Compaction/branch summaries | Compact/continue | K/S: obligation-preserving handoff, not blind reset |
| Cross-session memory | ContextSource/observer seams; no bundled memory product | Gated pipeline | Plugin/custom | Extension/custom | Custom | A/S: bounded memory retrieval, provenance, eviction; no vector DB in K. C3/P3 |
| Session history recall | Bounded result excerpts; F06 scan cost | Files/search tooling | Session tooling | Session/branch navigation | Session tooling | K/A: cheap indexed/paged recall; retain citations |
| Cache usage/cost reporting | Y | Y | Y | Y | Y | Preserve; add measurements of unknown usage and cache misses |
| Explicit provider caching | No cache controls | Y | Provider-dependent | Y | Provider-dependent | K/provider: F17, metadata-only policy, no required new runtime. C4/P4 |
| Reasoning effort selection | Unsupported in descriptor | Y | Y | Y | Y | Provider/config: bounded model capabilities; quality/cost A/B |
| Signed/encrypted reasoning replay | Missing generic representation | Y | Provider-dependent | Y | Y | Provider/K: F18; conformance per adapter. P4/X1 |
| Warm/incremental provider transport | Shared HTTP pool; full request encoding | WebSocket/prewarm/incremental | WebSocket V1 | Some providers | Provider-specific | A/provider: opt-in experiment; bounded sockets/fallback. C4/O3/P4 |
| Truncated model output continuation | Y, bounded | Y/P | Y/P | Y/P | Y/P | Preserve QQ's typed behavior; compare unfinished-tool handling, not just flags |
| Stable system/tool prefix | Compiled prefix, dynamic pins | Y | V2 context epochs | Y | Y | K/provider: test cache stability as pins/guidance/source blocks change |

### Coding, tools, and research

| Capability | QQ now | Codex | OpenCode | Pi | fx | Decision / cost / evidence |
| --- | --- | --- | --- | --- | --- | --- |
| File reads, bounds, continuation | Y | Shell/read tools | Y | Y | Y | Parity; benchmark useful bytes per turn |
| Multi-range/hash/outline read | Y | Partial via other tools | Partial via LSP | Partial via extensions | Read tracking | QQ strength; syntax-pattern outlines are not semantic LSP |
| Ignore-aware content/name search | Y | rg/file search | Y | Y | Y | Parity; cursor/query identity and changed-tree tests |
| Lightweight definition/reference modes | Pattern-based | Model-authored rg | LSP V1 | Tools/extensions | Literal search | QQ strength with explicit semantic limitations |
| Counted depth-bounded tree | Y | Shell | Listing/shell | Listing | Globs | Keep cheap; validate large/ignored repository bounds |
| Multi-edit, CAS, anchors, dry-run | Y | Patch/multi-file | Edit/patch | Single-file batch | Exact edit | QQ strength; distinguish per-file atomicity from workspace transactions |
| Output/display separation | Y | Y/P | Y | Y | Y | Parity; rendering must not expand model context |
| Retrievable output spill | Durable per session | Some tool paths | File store | Shell temp file | Result handles | QQ strength; test eviction and actual byte-retention guarantees |
| Per-turn aggregate output cap | Live only; replay gap F23 | Per-tool/other controls | Per-tool/other controls | Per-tool | Per-tool/compaction | Preserve through reconstruction; ablate against task quality, not just fewer tokens |
| Exact argv execution | Y | Shell-based execution | Shell | Shell | Internal direct planner | Preserve; optional tools should reuse ownership/cleanup |
| Persistent terminal, stdin, dev server | Missing; T10 | Y | UI PTY; model parity partial | External/extension | Y | A/K lifecycle: necessary daily-dev gap. C5/X3 |
| Structured human questions | Y; headless needs_input | Y | Y | Extension UI | Y | Preserve protocol as authority; asynchronous client behavior separate |
| Semantic LSP diagnostics/navigation | Proposed; pattern tools exist | External integrations | Y V1 | Extension | — | A: lazy shared server; don't add fixed two-second delay per edit. O4 |
| Workspace undo/checkpoints | Proposed | P: experimental worktrees; legacy snapshots retired | Y V1/V2 | Branching ≠ file undo | Latest tracked file undo; bounded in-memory preimages | A/S: run-snapshots plan; user-change conflict checks. C6/O5/X4 |
| First-class HTTP fetch | Missing; T9 | Hosted/shell | Y | Extension/shell | Y | A: streaming size limits, network policy, extraction/artifact cache. C7/O6/X5 |
| Search engine integration | MCP possible | Hosted/gated | Gated/provider-dependent | Extension | Gateway-backed | A: normalized queries/results, citations and budget; no baked-in vendor in K |
| Browser automation | External host possible, no image round-trip | Optional tool ecosystem | External integration | Extension | External integration | A: browser process outside core; image contract first |
| Image input and image tool output | Missing; T11; MCP omits image content | Y | Y | Y | Y | K/provider artifact contract + A/C transforms. C8/O6/P5/X5 |
| PDF/document extraction | No dedicated workflow | Tools/adapters | Tools/adapters | Extensions | Tools/adapters | A: extraction + cited pages/ranges; no OCR/browser dependency by default |
| Audio/realtime/voice | Missing | Optional/gated | Not established | Not established | — | C/A: defer until demanded; preserve modality-ready artifact seam |
| Research provenance/deduplication | Generic hashes/events, no research product | Partial | Partial | Extension | Partial | S/A differentiator: source snapshot, citation range, digest, fetched time and shared artifacts |

### Agents, execution policy, extensibility, and clients

| Capability | QQ now | Codex | OpenCode | Pi | fx | Decision / cost / evidence |
| --- | --- | --- | --- | --- | --- | --- |
| Read-only child fanout | Y | Y | V1 task | Extension/harness lanes | Y | Parity; measure cost and queueing, not child count |
| Supervised write child | Y, serialized parent checkout | Y | Agent/task policy | Extension | Policy-controlled | Preserve authority/teardown; not isolated parallel coding |
| Configurable child nesting | Y, up to 3 | Y/P | V1 agent policy | Extension | P | No new gap; report exact caps and defaults |
| Child follow-up/mailbox/reuse | No model-facing lifecycle | Y | Task resume/background V1 | New harness lanes; subprocess example single/parallel/chain, no durable child mailbox established | Message/run | S + small commands: bounded mailbox/idempotency/cancel. C9/O7/P6/X6 |
| Isolated parallel coding/patch integration | Supervisor needed | Worktree support | Worktree/session support | Extension | — | S: isolated checkout lease, patch, review and conflict-aware apply |
| Provider-aware fleet admission | Pool bounds; no established quotas | Not established | Not established | Not established | Not established | K/A/S: F21; per-request retries do not establish shared admission |
| Cross-worker durable task DAG | No; intentionally above core | Not established | Not established | Not established | Not established | S: QQ binary contract remains worker interface; external applications can supply scheduling |
| Task goals/todos/checkpoint intent | Run/session metadata only | Y | Y | Extension | P | S/C: structured durable obligations feed compaction |
| Process sandbox and egress | File capabilities; no process sandbox | Multiple OS backends | No equivalent kernel sandbox | External/container | Settings/policy, backend limited | A/K: F16, avoid overstating competitor guarantees. C10 |
| Approval policy/typed effects | Y | Y | Y | Extension/trust | Y | Preserve; fix reviewer invalidation; measure reviewer spend |
| File identity/concurrent-edit guard | Y CAS | Patch checks | File-state checks | Queued edits | Identity/preimages | Preserve; use CAS on restore/integration too |
| Cleared environment/output masking | Y | Policy-dependent | Hooks/adapter | Hook-dependent | Y | Keep bounded; masking is not a complete secrecy guarantee |
| Skills/packs/progressive tool discovery | Y | Y | Y | Y | Y | Parity; T14 improves retrieval when measured |
| In-process custom tools/MCP | Y | Y | Y | Extensions; MCP optional | Y | Existing EmbeddedToolHost is enough; no replacement framework |
| MCP resources/prompts/OAuth breadth | Tools-focused; incomplete breadth | Y | V1 support | Extension | Features tool | A: extend qq-mcp for concrete integrations, with F13 bounds. C11/O8/X7 |
| Arbitrary lifecycle hooks | Targeted seams, no broad hook ABI | Y | Plugins | Extensive typed hooks | Compile-time hooks | A: add only demonstrated hooks; post-commit observers for passive consumers. C12/P3 |
| Model-written code-mode tools | Missing | Gated | Experimental V1 | Extension | — | A experiment only: token/round-trip savings must beat interpreter overhead |
| Local interactive and automation surfaces | TUI/headless/HTTP+SSE | TUI/exec; stdio/Unix/WebSocket app-server | TUI/server/SDK | TUI/print/RPC; newer server infrastructure | TUI/headless/ACP/SDK | Preserve shared implementation; transports are not identical |
| Durable replay and crash interruption | Y | Y | V2 transactions, recovery unfinished | Multiple backends/harness | Y | QQ strength, not unique across all competitors; F01/F06 matter |
| Conversation fork/branch/export/import | Resume and compaction rollback; no arbitrary fork/turn rollback/export product | Resume/fork/rollback; import/export not qualified here | Fork/revert; exact portability not qualified | Rich tree/branching | Resume and portable bounded checkpoint; no full tree established | K small identity + C/S UX; distinguish file undo. C6/P6/X4 |
| Native/browser client foundations | Rust native + wasm client and shared reducer | App-server clients | SDK/client | Client/protocol packages | Native N-API + browser WASM SDK; separate terminal | Good QQ base; wasm compile is not browser runtime acceptance |
| External language SDK/editor ACP | Not shipped | TS and sync/async Python SDKs/integrations | JS SDK/ACP | TypeScript SDK/RPC; ACP not established | JavaScript native/WASM SDK + ACP | C/A: TS/Python/ACP candidates over existing protocol; test reconnect/version skew. C13/O9/P6/X8 |
| Browser/desktop/mobile apps | Planned | App-server integrations; app implementations not inspected | Web/desktop located; mobile not established | SDK/browser-compatible primitives; no bundled app established | Browser/native agent and terminal SDK; example web apps | C: separate deliverables and build dependencies |
| Remote client enrollment/TLS | Foundations; remaining multi-surface work | Not qualified | Not qualified | Server protocol over host-authorized transport; Unix preset | ACP/host-dependent; enrollment not qualified | Server/C: finish S2/S4/S6 before calling remote daily-dev ready |
| Fleet traces, notifications, analytics | Events/trace/observer; no full product | Detailed telemetry | Event/plugin integrations | Extension/server | Trace hooks | A/S: bounded export, exact queue/provider/store attribution. C14 |
| Autonomous scheduled/social/bot apps | Not core | Bundled implementation not established | Slack thread bot and GitHub comment-to-PR integration; scheduler not qualified | External applications/extensions possible; bundled apps not established | External applications possible | S: build applications around QQ, not into every agent loop. O10 |

## Keep the core small

The current direction already has the right useful interfaces. Improve their
contracts instead of adding a general interception framework.

| Layer | Owns | Must not become its default dependency |
| --- | --- | --- |
| Runtime/protocol/provider kernel | Run/turn/call identity, authoritative events, budgets, cancellation, context projection, provider capabilities, bounded tools and artifact references | Browser, OCR, language servers, vector DB, workflow scheduler, UI framework |
| Optional adapters | Providers/transports, MCP, sandbox backends, retrieval, LSP, terminal, research extraction, telemetry | Mandatory work on disabled paths; unbounded background workers |
| Clients | Composer, visual transcript, diff/approval UI, model/context inspector, editor integrations | Direct mutation of authoritative run state or another agent loop |
| Supervisor/apps | Worktree ownership, agent mailboxes/task DAG, repository setup, patch integration, independent verification, cross-worker quotas, research knowledge products | Linking untrusted repository execution into a multi-tenant control plane |

Current host-target normal/feature dependency inventory, counted as unique
`name version` packages using `cargo tree --locked --offline --target
x86_64-unknown-linux-gnu --edges normal,features`:

| Profile | Distinct packages including QQ packages | Interpretation |
| --- | ---: | --- |
| Root `qq`, default | 335 | Full binary composition |
| Root `qq --no-default-features` | 281 | AWS omitted; TUI/server/MCP still present |
| `qq-core --no-default-features` | 147 | Existing core library closure, not a runnable product profile |

The executable budget currently allows 300 packages for the minimal profile,
41,000,000 bytes for its release binary, and 48,000,000 bytes for the default
binary (`benchmarks/perf/budgets-v1.json:6–12`). The package count is under its
cap. No current release-byte, startup or RSS claim follows from these counts;
existing binaries under `target/` were not used as exact-revision measurements.

An equivalent independently checked count for the minimal root is:

```sh
cargo tree --locked --offline --target x86_64-unknown-linux-gnu \
  -p qq --no-default-features --edges normal --prefix none \
  --format '{p}' --no-dedupe | sort -u | wc -l
```

Use `-p qq-core --no-default-features` for core, or remove
`--no-default-features` for full QQ. Excluding feature-label rows and `(*)`
back-reference decoration is necessary when counting unique package identities.

Recommended profiles should follow real consumers: a small headless worker,
the full local developer binary, and library embedding. Prefer gating existing
adapters over speculative crate splits. Record transitive dependency deltas,
binary size, cold/warm startup, idle/active RSS, and feature-disabled work. Keep
SQLite durability and policy where the selected consumer requires them.

## Distinctive capabilities worth building

These are useful product hypotheses, not claims of industry-wide uniqueness.
Several use capabilities QQ already has but needs to validate together.

| Proposal | Small core requirement | Product above it | Proof needed |
| --- | --- | --- | --- |
| Explainable context continuity | Correct call identity, artifact provenance, bounded compaction checkpoints and recall | Inspector showing retained goals, facts, evidence, omissions and available recall | Long-task constraint retention and recovery rate; tokens and latency per successful task |
| Verifiable task receipt | Stable run/plan/model identity, events, artifacts and typed outcome | Exact changed files, commands, tests, unresolved failures and spend linked to the claimed result | Independent verifier accepts/rejects misleading completion; receipt survives restart |
| Work sharing without history duplication | Immutable artifact references and context-source budgets | Research children share fetched documents/indexes; parent receives cited findings, not every transcript | Duplicated fetch/read tokens avoided; provenance and authority isolation maintained |
| Bounded fleet service | Fair admission, provider cooldown, deadlines and queue timing | Supervisor selects concurrency/model routes against throughput, spend and deadlines | Useful-result throughput/RSS/cost curves under provider throttling and slow consumers |
| Reversible parallel coding | Owned mutations, CAS and artifact receipts | Worktree leases, patch handoff, independent checks, conflict-aware integration and undo | Concurrent user edits preserved; abandoned work isolated; integration cost measured |
| Efficient workspace intelligence | Existing hash reads, ranges, outlines, search cursors, batches and spills | Clients display semantic diagnostics/diffs while model receives small actionable results | T13 ablation improves completion time or tokens at equal task success |
| Research evidence ledger | Artifact hashes, bounded typed results and durable links | URL/source version, fetched time, excerpt/page range, claim→citation graph and refresh policy | Citation correctness, source reuse, contradictory evidence handling, bounded retrieval cost |

Do not advertise “cheapest,” “fastest,” “durability nobody else has,” or
“bounded everything” from source inspection. Several such aspirations in the
older catalog are not qualification receipts, and this audit found concrete
exceptions to the intended bounds.

## Proposed work order and ownership

Use the existing plans where they already own the capability. The entries
below are proposed work packages, not new Linear issues or declarations that
older gates are closed. Split each into reviewable slices after confirming the
tracker and exact acceptance.

| Order | Work package | Findings/capabilities | Acceptance before advancing |
| --- | --- | --- | --- |
| 1 | Restore trustworthy session continuity and control | F01, F02, F05, F07, F14, F25 | Public deterministic regressions, crash/reopen cases, no zero-test CI selectors; cancellation/settlement invariants preserved |
| 2 | Make long sessions bounded and recoverable | F03, F04, F06, F10, F11, F20, F23, F24, F28 | Multi-window task completes, old-history size no longer drives active assembly, large attach/reconnect succeeds, retention preserves recall/accounting |
| 3 | Close provider/config/adapter bounds | F08, F09, F12, F13, F17, F18, F21, F26; measured F27; R7/R8 | Fault-injected provider/host tests, correct cache invalidation, shared cooldown, cold/warm token and latency receipts |
| 4 | Qualify the lean profiles and existing tools | F15, F19, F22; H0/H12 and T13/T14 | Exact profile matrix, controlled-host tails, meaningful task ablation; reconcile current plans without waiving gates |
| 5 | Finish daily-dev essentials as optional adapters | H10 sandbox, T10 terminal, scoped instructions, run snapshots, LSP | Concrete user workflow passes; disabled profile pays no adapter initialization/RSS cost; teardown and user-edit preservation tested |
| 6 | Add research and visual workflows | T9 fetch, T11 image/artifacts, search/browser/PDF adapters, evidence ledger | Bounded end-to-end research/coding tasks with citations and visual verification; artifact reuse measured |
| 7 | Build reusable agents and clients above core | Mailboxes/task supervisor, isolated parallel coding, SDK/ACP, multi-surface plan | Durable idempotent task handoff, versioned clients, independent verification and multi-worker fault tests |

Independent work can run in parallel when owned paths do not overlap. In
particular, thin clients and read-only adapter prototypes can proceed beside
core fixes. Do not use UI completion or more agent concurrency as evidence
that core continuity is fixed.

H10 sandboxing should run as an independent lane alongside orders 1–3 once its
relevant execution/teardown prerequisites are satisfied. Its placement with
daily-development adapters is architectural, not a dependency on completing
the paid evaluations or all work in order 4.

Existing owners: [speed-first](../plans/speed-first-extensible-agent-harness.md),
[tools](../plans/tool-layer.md), [readiness](../plans/terminal-bench-readiness.md),
[delegation](../plans/supervised-delegation.md),
[clients](../plans/multi-surface-clients.md),
[snapshots](../plans/run-snapshots.md), and
[LSP](../plans/lsp-diagnostics.md). H22.2 and T8 are in the inspected Git history;
their stale “in review” ledger rows should not cause implementation to restart.
The carried H0/fairness/native-platform and paid quality gates remain separate.

## Performance and quality acceptance

Preserve the [perf recording discipline](../runbooks/perf-recording.md): same
revision-bound profiles, deterministic fixtures, A/B and same-binary A/A where
tails are noisy, explicit host conditions, no dropped outliers, and sufficient
samples for the reported percentile. The existing recorder already covers
many local paths; extend it rather than creating a parallel benchmark system.

| Axis | Required experiment | Decision it supports |
| --- | --- | --- |
| Lightweight core | Fresh-process startup, serve readiness, idle/active RSS, release bytes and dependency closure for each real profile | Whether optional capabilities are actually optional |
| Local responsiveness | Command acknowledgement, first provider send, durable delta, client/render delivery, cancellation and cleanup | Existing <=10 ms ack, <=15 ms durable delta p95 / <=40 ms p99; carried <=20 ms output-service-gap target must be qualified, not assumed |
| Long-session cost | Fixed active suffix with 10/1,000/10,000 archived turns; absent-term history search; large snapshot | Whether work follows active context instead of archive size |
| Fleet efficiency | 1/8/32/128 requested agents, actual active roots/children, 1/8/32 subscribers, slow consumer and shared provider quotas | Throughput, queue p95/p99, peak RSS and successful tasks per resource budget; do not confuse queued sessions with active agents |
| Provider efficiency | Cold/warm prefix, tool-schema selection, cache affinity, continuation transport and 429/503/stream failures | TTFT, request bytes, cache-hit tokens, requests per successful turn and billed tokens |
| Coding effectiveness | Fixed model/config/task sets; current tools versus T13 ablations; edit→diagnostic→repair and terminal workflows | Pass rate, wall time, tool calls, repeated reads and total tokens per verified task |
| Context reliability | Long task with later steering, attachment edits, repeated call IDs, many compactions and model switches | Original constraints and evidence survive; rejected states are actionable |
| Failure durability | Crash after commit/before publish, mid-tool, pending approval, child admission, compaction and artifact eviction | No repeated uncertain mutation, duplicate charge, missing terminal event or invalid reconstructed request |
| Research effectiveness | Multi-source question with contradictory documents, follow-up and source refresh | Supported claims, citation accuracy, duplicate retrieval and tokens per verified answer |
| Native platforms and browser | Execute intended Windows/macOS/Linux cases and browser wasm client flows | Compilation or Linux success is not cross-platform acceptance |

First establish QQ baselines and correctness. Then compare the reference
snapshots on the same workload, model, tool permissions, context policy and
hardware. Count retries, background auditors, extraction, indexing and child
spend in the result. For hosted/vendor-specific tools, record the capability
difference rather than pretending all runs have equal inputs. Paid model
evaluations require a separately chosen budget; none were launched here.

## Source map

The following anchors make the comparison reviewable without treating a README
claim as implementation proof. Paths are relative to the repository; line
numbers refer to the snapshot identities above. Flags and V1-only features
remain qualified in the matrix.

| Group | Source anchors |
| --- | --- |
| C1: context controls | `.source/codex/codex-rs/core/src/tools/handlers/get_context_remaining.rs:81`; `new_context_window.rs:38` in the same directory; `.source/codex/codex-rs/core/src/tools/spec_plan.rs:1206`; `.source/codex/codex-rs/core/src/compact.rs` |
| C2: instructions | `.source/codex/codex-rs/core/src/agents_md.rs:1,65` |
| C3: memory | `.source/codex/codex-rs/memories/README.md:34–120`; `.source/codex/codex-rs/memories/write/src/start.rs:24`; `phase2.rs:49,96,210` in that directory |
| C4: provider transport/cache/continuation | `.source/codex/codex-rs/core/src/client.rs:12,491,845,1298,1419,1735`; `.source/codex/codex-rs/core/src/context_manager/history.rs:840` |
| C5: terminal | `.source/codex/codex-rs/core/src/unified_exec/mod.rs:1,76,108,129` |
| C6: session branches/worktrees | `.source/codex/codex-rs/core/src/thread_manager.rs:1085,1316,1350`; `.source/codex/codex-rs/core/src/config/mod.rs:214` retires snapshots; `.source/codex/codex-rs/tui/src/chatwidget/worktree_picker.rs:82` gates worktrees |
| C7: web | `.source/codex/codex-rs/core/src/tools/spec_plan.rs:609,1038` |
| C8: images | `.source/codex/codex-rs/core/src/tools/handlers/view_image.rs:52,93` |
| C9: collaboration | `.source/codex/codex-rs/core/src/tools/handlers/multi_agents_v2/`; `followup_task.rs:41`, `spawn.rs:115` |
| C10: sandbox | `.source/codex/codex-rs/sandboxing/src/manager.rs:42,119,138` |
| C11: MCP content/auth | `.source/codex/codex-rs/core/src/tools/handlers/mcp_resource.rs:37`; `.source/codex/codex-rs/rmcp-client/src/lib.rs:40,53` |
| C12: hooks/code mode | `.source/codex/codex-rs/hooks/src/lib.rs:22`; `.source/codex/codex-rs/core/src/tools/mod.rs:70` |
| C13: SDK/transports | `.source/codex/sdk/typescript/src/{codex,thread,exec}.ts:25,66,196` respectively; `.source/codex/codex-rs/app-server-transport/src/transport/mod.rs:84,103` |
| C13: Python SDK | `.source/codex/sdk/python/src/openai_codex/api.py:78,304` provides sync/async entry points |
| C14: telemetry | `.source/codex/codex-rs/otel/src/metrics/names.rs:22`; `runtime_metrics.rs:122` in that directory |
| O1: context | `.source/opencode/packages/core/src/session/`; `.source/opencode/packages/opencode/src/session/compaction.ts` |
| O2: instructions | `.source/opencode/packages/opencode/src/session/instruction.ts:34,60` |
| O3: provider transport | `.source/opencode/packages/opencode/src/plugin/openai/ws-pool.ts:31` |
| O4: diagnostics | `.source/opencode/packages/opencode/src/tool/lsp.ts:84`; `edit.ts:198`, `write.ts:76`, `apply_patch.ts:265` in that directory |
| O5: checkpoints | `.source/opencode/packages/opencode/src/snapshot/index.ts:318,382,408`; `.source/opencode/packages/core/src/session/revert.ts:61,113` |
| O6: research/images | `.source/opencode/packages/opencode/src/tool/webfetch.ts:24,95,110`; `registry.ts:58`, `read.ts:300–323` in that directory. The latter includes model PDF attachments, not a qualified OCR/page-citation service |
| O7: tasks | `.source/opencode/packages/opencode/src/tool/task.ts:43,97` |
| O8: MCP/hooks | `.source/opencode/packages/opencode/src/mcp/index.ts:114,781`; `.source/opencode/packages/core/src/tool/registry.ts:85` |
| O9: SDK | `.source/opencode/packages/sdk/js/src/v2/client.ts:50`; `.source/opencode/packages/sdk-next/src/opencode.ts:32` |
| O10: applications | `.source/opencode/packages/slack/src/index.ts:78,105` creates thread sessions and submits prompts; `.source/opencode/github/index.ts:128,813` handles comment events and creates PRs |
| P1: context/compaction | `.source/pi/packages/coding-agent/src/core/agent-session.ts:545,2137,2193`; `core/extensions/types.ts:593` in the same package; `test/suite/regressions/7048-compaction-truncated-summary.test.ts:32`, `8328-zero-usage-auto-compaction.test.ts:31` |
| P2: instructions/resources | `.source/pi/packages/coding-agent/src/core/resource-loader.ts:72,119,1024`; `core/skills.ts`, `core/prompt-templates.ts`, `core/package-manager.ts` in the same package |
| P3: extension seams | `.source/pi/packages/coding-agent/src/core/extensions/types.ts:133,451,593,940`; `.source/pi/packages/agent/src/harness/types.ts:43`; `.source/pi/packages/agent/src/harness/utils/adaptive-publisher.ts:11,64` |
| P4: provider controls | `.source/pi/packages/ai/src/api/openai-responses.ts:301`; `anthropic-messages.ts:1026,1287,1373,1458`, `openai-codex-responses.ts:267`, `openai-responses-shared.ts:533` in that directory |
| P5: image results | `.source/pi/packages/coding-agent/src/utils/tool-result-images.ts:12,54`; `.source/pi/packages/coding-agent/src/core/agent-session.ts:669,1269`; `.source/pi/packages/agent/src/harness/tools/image.ts:3` |
| P6: sessions/embedding/agents | `.source/pi/packages/coding-agent/src/core/sdk.ts:173,306,388`; `core/agent-session.ts:3131`; `examples/extensions/subagent/index.ts:4,33,604`; `.source/pi/packages/agent/src/harness/types.ts:83,158`; `runtime/harness.ts:279,305` under that harness directory; `.source/pi/packages/server/src/connection.ts:6`, `transports/unix/preset.ts:7` |
| X1: context/continuation | `.source/fx/src/core/agent/runtime/prompt_context.zig:49,305,674`; `context_compaction.zig:22,61,150` in that directory; `.source/fx/src/core/session/session.zig:2393,4922` |
| X2: scoped instructions | `.source/fx/src/builtins/context.zig:204,489,1376`; `.source/fx/src/core/workspace/context_contract.zig:8,263`; `.source/fx/src/core/agent/runtime/tests/tool_flow.zig:3253` |
| X3: terminal | `.source/fx/src/tools/shell/shell.zig:54,597,682,785,921`; `.source/fx/src/core/terminal/{host,engine,native_session,tmux_session,recovery}.zig` |
| X4: checkpoints/undo | `.source/fx/src/core/agent/runtime/checkpoint.zig:8,31,133`; `.source/fx/src/core/workspace/change_tracker.zig:30,53,272`; `.source/fx/src/builtins/commands.zig:443`; `.source/fx/src/core/app/app_commands.zig:1208`; `.source/fx/sdk/fx-sdk.js:1483` |
| X5: research/vision | `.source/fx/src/tools/web/search.zig:27,56`; `fetch.zig:29` in that directory; `.source/fx/src/tools/agent/vision.zig`; `.source/fx/src/tools/session/read_tool_result.zig:118,125` |
| X6: delegation | `.source/fx/src/tools/agent/subagent.zig:105,169` |
| X7: MCP/discovery | `.source/fx/src/tools/capabilities/capability_search.zig:112`; `.source/fx/src/core/tooling/tool_mcp_feature_dispatch.zig:40,132`; `.source/fx/src/core/mcp/features/prompts.zig:134,236,342`; `resources.zig:19` in that directory; `.source/fx/sdk/mcp.js` |
| X8: distribution/benchmarks | `.source/fx/src/acp/server.zig:71`; `.source/fx/sdk/node.js:548`, `browser.js:17`; `.source/fx/src/wasm_core_main.zig`; `.source/fx/sdk/tests/test-libfx-benchmark.mjs:14`, `test-pi-benchmark.mjs:23` |

### Inventory and maturity caveats

| QQ area | Inspected concerns | Important limits of the audit |
| --- | --- | --- |
| `src/` | Composition, plans/cache, reviewer, MCP bridge, CLI/headless | No live OAuth/model discovery or interactive end-to-end session |
| `qq-core` | Loop, budgets, tools/approvals, files/instructions/skills, plans/hosts/context, sessions/store/projections/compaction/delegation | Targeted paths and tests, not a formal review of every mutation or policy rule |
| `qq-provider`, `qq-reasoning` | Neutral request/event vocabulary, compilation, retries, adapter encoding/reasoning/cache behavior | Provider conformance inspected; no live credentials or provider matrix run |
| `qq-auth`, `qq-config`, `qq-mcp` | Credential/config identity and caching, profiles/metadata, discovery/transport/host bounds | No new audit of OS credential-store security or all OAuth flows |
| `qq-protocol`, `qq-client`, `qq-server` | Wire limits/commands, client reducer/reconnect/observer, server admission and snapshots | Large-body/snapshot pressure cases remain proposed tests |
| `qq-tui` | Surface separation, retained rendering/composer, client integration and existing tests | No visual terminal qualification or full input/accessibility audit |
| `xtask`, benchmarks, CI/release | Perf budgets/recorder, profile graph, workflow selectors and acceptance boundaries | No full performance recording, release rebuild, or native platform run |

Pi's inventory includes `ai`, `agent`, `coding-agent`, `tui`, `chord`,
`protocol`, `client`, `server`, `session-backends/sqlite-node`, `telemetry`, and
`evals`. The newer harness is not automatically the coding CLI's runtime:
`coding-agent/src/core/sdk.ts:306,388` still constructs `Agent`/`AgentSession`,
while `agent/src/harness/runtime/harness.ts:305` throws
`SliceNotImplemented` for `watchSession`. Source package presence is not
default-product acceptance. Pi's subprocess subagent example is an extension,
not proof of a built-in durable agent scheduler.

Fx's inventory includes native CLI/core, provider gateway, filesystem/shell/
research/vision/skill tools, terminal UI, ACP, N-API/WASM, Node/browser SDKs,
MCP/skills adapters, examples, evals and benchmarks. A browser-hosted runtime
is a different product from QQ's existing wasm client; build one only for an
actual consumer. Fx's bounded portable checkpoint is not by itself proof of
safe replay of interrupted remote side effects.

Codex has broad tools, platform sandboxing, app-server/SDK, provider transport,
memory, hooks and client infrastructure. Optional/gated capabilities do not all
describe the default configuration. OpenCode V1's mature tool/client surface
and V2's newer core must remain distinct. V2's own
`specs/v2/session.md:165` and `specs/v2/todo.md:56–70` defer crash-continuation
work; transactional events alone do not close that gap.

Avoid copying reference weaknesses: OpenCode V2's broad event pubsub is
unbounded (`packages/core/src/event.ts:175,188`); V1 webfetch reads an entire
unknown-length body before checking size (`packages/opencode/src/tool/webfetch.ts:95–103`).
Pi's mutable pre-request extension hook explicitly lacks revalidation
(`coding-agent/src/core/extensions/types.ts:940–943`). Keep QQ's bounded,
validated contracts even when adopting a useful capability.
Fx's file undo restores preimages without a current-content compare at
`core/workspace/change_tracker.zig:53–92`; conflict-aware restoration is a
better acceptance target than copying that behavior.

## Verification receipt and remaining uncertainty

The starting QQ worktree was clean. Research changes are limited to this
document, its documentation index entry, and the root ledger; diagnostic
artifacts are local and ignored. The source findings are tied to the base
revision above even though the documentation is written on a new branch.

| Check | Result | What it establishes |
| --- | --- | --- |
| Fresh `cargo build -p qq-core` for the probe | Passed | The public-API diagnostic used current source |
| Scripted `duplicate` probe | Reproduced independently by audit author and core investigator | F01: two completed reads become one wrong result and one interrupted placeholder |
| Scripted `duration` probe | Reproduced twice; approximately 1,071 / 1,075 ms for a 100 ms limit | F02: execution outlives the budget before cancellation begins |
| Scripted `attachment` probe | Reproduced independently by audit author and core investigator | F05: original bytes are absent from the next assembled prompt |
| Scripted `empty` probe | Worker arithmetic panic caught; `Server` run failure; follow-up succeeds | F25: empty ranged attachment is misclassified as a server failure; dev build only |
| `cargo test -p qq-core known_later_turn_context_overflow_starts_no_second_provider_request -- --nocapture` | 1 passed | F03 safely rejects a later overflowing request; it does not establish recovery |
| `cargo test --locked -p qq-core --lib sessions::tests::unconfirmed_shell_exit_prevents_session_continuation -- --exact` | 0 tests; exit 0 | Stale selector silently passes |
| Same command with `sessions::tests::delegation::` | 1 passed on Linux | Correct selector resolves and its Linux case passes |
| Host-target dependency inventories | 335 full / 281 minimal root / 147 minimal core | Selected package closure, independently recounted |
| Independent factual review | Three investigators reviewed the synthesis; corrections incorporated | Removed unsupported reference parity claims; clarified nested delegation, retired Codex snapshots, transport and package maturity, CAS scope, and sandbox sequencing |
| Documentation validation | Passed: local links/source paths, table columns, F01–F28 sequence; 63 capability rows; whitespace check | Document structure and evidence pointers, not runtime qualification |

Local probe source and the compact receipt are retained under
`target/qq-perf/harness-audit-2026-09-16/` (untracked). The probes use a fake
provider and fresh temporary stores/workspaces; they do not call a model or
touch user sessions. One initial probe rerun collided with its scratch-directory
name; the harness was corrected to use a timestamp and rerun successfully.
That diagnostic-only collision is not a QQ finding.

Outstanding verification is explicit in the findings: full workspace checks,
native Windows selectors, exact-release performance/size, model-quality and
compaction evaluations, adversarial MCP/HTTP bounds, large snapshot reproduction,
and normalized competitor benchmarks. No absence of failure in this audit
should be read as proof those areas are qualified.
