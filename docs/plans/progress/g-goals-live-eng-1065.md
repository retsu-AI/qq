# G5 — subscription-only live goal evaluation

Status: five trials settled. The fifth establishes G5 live acceptance: one
132.5-minute trial,20 real compactions and a verified terminal budget outcome.
Its useful continuation measurement task remains incomplete. ENG-1065 / ENG-809.
This is a curated evaluation record. Raw transcripts, credentials and session
stores remain outside Git under `/tmp/qq-goal-delivery-20261015/`.

## First trial source and authorization

- Source under evaluation: `e72a82cc704ae75961ce557f9a713fb5aca951a9` (qualified G4 snapshot).
- Binary SHA256: `38039783cc1a937f8bc385d328ce609929e2881d5dc81e23fd7f82e5dbc3d8fb`.
- Started: 2026-10-16T06:14:26.558871+00:00.
- Exact model route: `openai-codex/gpt-6.1-sol`, subscription only;
  no paid API fallback. The isolated credential store contains only the
  subscription credential, and the launcher excludes API credential variables.
- Bounds: four-hour goal deadline, eight goal runs; each run has at most
  64 model turns, 250000 total tokens and 30 minutes. Provider output is
  bounded at 8192 tokens per request; configured context window is 65536.

The task is an independent acceptance audit of the durable-goals stack:
paid-work admission, sponsorship and cancellation; exact check authority and
crash holds; independent audits and historical state; TUI/CLI controls; and
narrow deterministic reproductions for concrete suspected defects. Its final
artifact is `G5_REVIEW.md`. No sleeps, empty turns or repeated checks may be
used to lengthen the evaluation. No external writes or delegation are allowed.

The stack was subsequently rebased onto main `3e34eea2` for #330/#331.
Independent rebase review confirmed unchanged goal production source and
preservation of upstream auth and fixture fixes. This live record identifies
its exact earlier source; it does not qualify the later upstream auth changes.

## First trial outcome

Started 2026-10-16T06:14:26.558871+00:00;
finished 2026-10-16T06:39:29.220024+00:00.
Actual elapsed time: **1502.661 seconds (25 minutes)**.
The supervisor captured exit **8** and the terminal JSONL record states
`budget_exhausted { reason: runs }` after exactly **8** charged runs.
All **15** checklist items were done, but no independent achievement audit
remained within the run allowance. The runtime did not report achieved.

- All45 observed model turns used `openai-codex/gpt-6.1-sol`.
- Exactly **one** successful compaction committed, retaining the15-item
  checklist. Its item-id/text SHA256 was
  `e59de0b83e8f4279bf469cdabae7103c19e0df1081e4fc8469174dc6d9bea066`.
- The first work run stopped after a rejected in-run compaction summary.
  The next attempt compacted successfully and continued the same goal.
- Terminal goal usage matches the observed completed-turn sum:
  **1700179 fresh input +68480 cache-read input +20581 output tokens**;
  cache-write input0. Reported estimated dollar cost is unknown (`null`).
- **Paid API spend: USD0.** No paid API route or fallback was used.
  Subscription-amortized dollar value is unknown and is not reported as zero.

The useful acceptance audit produced `G5_REVIEW.md` without implementation edits.
The outer evaluator independently reconciled its reported test totals with the
actual tool-completion records: **192 passed,0 failed,1 deliberate ignore**.
The ignore is the manual goal-less production request-body capture. Two other
commands selected zero tests and are excluded from the pass total.

| Observed command filter | Package | Passed | Ignored |
| --- | --- | ---: | ---: |
| `sessions::tests::goal_driver_soak` | qq-core | 5 | 0 |
| `sessions::tests::goal_`, skipping goal_driver_soak | qq-core | 52 | 0 |
| `sessions::tests::goals::` | qq-core | 82 | 1 |
| `goal` | qq binary | 23 | 0 |
| `goal` | qq-tui | 22 | 0 |
| `approval::classify::tests::goal_check` | qq-core | 7 | 0 |
| exact CLI guide-flag truth test | qq binary | 1 | 0 |

Each used offline/locked dependencies and the task's isolated target. This is
focused deterministic evidence gathered by a real subscription-backed goal;
its scripted multi-day tests are not multi-day elapsed operation.

## Findings and follow-up

No implementation defect was demonstrated. The audit found an obsolete initial
plan diagram claiming goal resume acknowledged interrupted-check uncertainty.
The normative extension and implementation require independent exact workspace/
run acknowledgment. Commit `b750d347` corrects that diagram in #330.

The audit also identified a missing direct foreground test combining held
sponsored work with terminal-record emission. Existing source and core tests
support the behavior; the audit did not independently prove that complete
interleaving. Further useful concurrency acceptance work is being prepared.

## Acceptance

| Requirement | First trial evidence | Result |
| --- | --- | --- |
| Useful real multi-hour task | Audit completed in25minutes | **Not met** |
| Checklist survives at least three compactions | One committed compaction with15items | **Not met** |
| Goal reaches terminal state | Run cap8, terminal budget_exhausted, supervisor exit8 | Met |
| Usage and cost reported honestly | Reported tokens; USD0 paid API; token dollar value unknown | Met |
| Subscription only, no paid API fallback | Isolated route/credential and45 observed model identities | Met |

**G5 remains open.** This trial is not a pass for the multi-hour/three-compaction
criterion. No delay, empty work or manufactured compaction was used to extend it.
A further trial must use useful real work, preserve finite limits and record its
own source and actual outcomes.

## Second useful trial

Started 2026-10-16T06:56:09.355934+00:00
on source `07aab1d15bdb142a0c11f8510bc41cf3a1ad3e61` and binary SHA256
`9593d83cad54b216f69f369db5c1565fc854a97e2815eb1eb2ac9c1da37bc437`. This is the qualified G4 snapshot used for this trial; subsequent fixes are recorded below.
The task closes the foreground sponsored-family concurrency acceptance gaps,
using reachable interleavings, explicit channel barriers, meaningful red/green
checks and a final supported report. Implementation changes require a demonstrated
defect. No delay or manufactured compaction is allowed.

Configured bounds: **11897 seconds remaining** within the
original four-hour evaluation window; at most **8 runs**, each limited to
**64 model turns,3000000 total tokens and30minutes**. The configured context
window is **32768 tokens**, with output limited to8192 tokens per request.
The larger per-run token allowance gives a useful work run and its summaries
room to finish, unlike the first trial's restrictive250000-token allowance.

The exact authorized route remains `openai-codex/gpt-6.1-sol`, subscription only,
with no paid API fallback. The initial bounds and the final results below describe the same goal, including its recovery. Neither trial is a G5 pass.


## Second trial recovery and outcome

The 32768-token context setting was too small for fixed repository/tool context
plus the 8192-token output reserve. Three work runs failed closed when compaction
could not fit the remaining context; the goal stopped as `blocked/repeated_failure`.
This configuration failure is not evidence of a production context-window defect.
The evaluator restored the usable 65536-token setting and resumed **the same goal**
through the real server CLI. Revision advanced to2; the three spent runs, eight-run
cap and original deadline were retained. No budget was reset or extended.

Finished 2026-10-16T07:21:18.264271+00:00, **1508.908 seconds (25 minutes)**
after starting. The authoritative goal-update stream reports **achieved**, all
**5 checklist items done**, **5 charged runs** and **1 independent audit**.
The resumed server exited cleanly with code0. This resumed HTTP/SSE path did not
emit a new foreground terminal JSONL record; the earlier blocked CLI record is
not presented as its final revision's result.

- All **69** observed completed model turns used `openai-codex/gpt-6.1-sol`.
- Exactly **2 successful compactions** retained the same five checklist item IDs
  and texts, SHA256 `0e973f66ed0f897e6cd82a54c69ba8d7c2c2bc2616342898975c57ddba6ac461`.
  Both occurred before the explicit resume; no third compaction is claimed.
- Goal counters match the observed usage sum: **2282927 fresh input +409472
  cache-read input +28453 output tokens**; cache-write input0. Estimated dollar
  cost remains unknown (`null`). **Paid API spend: USD0**; subscription-amortized
  dollar value remains unknown.

### Useful result and independently reproduced defect

The goal produced a seven-row foreground acceptance test covering interrupt,
external pause, parent cancellation, clear/replace during interrupt drain, output
failure and owner drop. It holds an actually admitted child's execution
preparation through bounded channels, verifies retained cleanup ownership,
child-before-parent durable finish, unrelated-run isolation and ordered direct-run
records before a single final goal record. Clear/replace rows test mutations
during interrupt drain, not standalone external clear/replace termination.

It demonstrated a G4 bug: an external pause increments the goal revision, while
the foreground loop accepted only its original exact revision and could hang
after the owned family finished. The repair observes newer stopped revisions of
the same goal and uses the observed exact identity for settlement and cleanup;
replacement goal IDs remain excluded. It belongs in the originating #331.

The evaluator independently copied the tests onto #331 while retaining its
original production code: nextest `f96d48c8` failed with
`pause: held-family acceptance hung`. The repair and all foreground regressions
are qualified separately in #331. Independent read-only review approved the
restored fix and test. The live task also demonstrated a temporary early-drain
mutation failing the direct-run ordering assertion, restored the source exactly,
and passed its affected-package gate (**422 tests,2 deliberate skips**) and
real-binary loopback smoke (**8/8**). No mutation ships.

This was useful real subscription-backed implementation work and reached a
verified terminal state. It still does **not** meet the multi-hour requirement
or the at-least-three-compactions requirement. G5 remains open; no delay or
artificial work was added to change that outcome.

## Third trial: captured review repairs

Started 2026-10-16T07:26:25.461415+00:00 on runtime source
`f0fff5aa6881b8b485a7369e0cac03179e18b01a`, binary SHA256
`007ba4cef4625720ac64369c945aa26d187ed100fdfeefb9405df8504607e774`.
The useful task reproduced and repaired twelve captured review findings, with
local test-first commits separated by originating PR and a supported final report.
It used the same exact subscription route, with no paid API fallback, external
writes or delegation. Limits remained finite: 10081 seconds within the original
evaluation window, eight goal runs, and 64 turns/3000000 total tokens/30 minutes
per run; context 65536 and maximum output 8192 tokens.

Finished 2026-10-16T08:22:40.907001+00:00 after **3375.446 seconds
(56 minutes)**. The supervisor captured process exit **0** and an actual final
foreground goal record with status **achieved**. All **21 checklist items** were
done, with **6 charged runs**, **1 independent audit**, and **256 completed model
turns**. One earlier compaction failure consumed a run; normal continuation
recovered without extending the goal budget or deadline.

- **Four successful real compactions** retained all 21 checklist item IDs and
  texts. Every captured checklist digest was
  `4bc7cf5dae038f9dae75b56c39aab2c1c9e02f7de28add6abd6493be1892e5a6`.
- Authoritative counters match the completed-turn usage sum: **8997361 fresh
  input +1946624 cache-read input +71060 output tokens**, cache-write input0.
  Estimated dollar cost is `null`. **Paid API spend: USD0**;
  subscription-amortized dollar value is unknown.
- The local affected-package gate on task source `16a1381b` independently records
  **2202/2202 passed,14 intentional skips**, formatting/lint/Clippy green.
  Its only dirty path was the private task specification. The real task binary
  passed **8/8 loopback smoke scenarios**. This is task evidence, not a full
  workspace gate or qualification of the final outer PR heads.

The outer reviewer found limits in the initial proposed repairs and strengthened
them before integration: actual execution-start managed-policy revalidation,
monotonic sponsor expiry across held preparation, interruptible caller close,
and excluding owned compactions from preempting-prompt detection. Those fixes,
regression tests and final-head gates belong to #330/#331. The task's achieved
status and report do not independently establish those later corrections.

This trial demonstrates natural continuation through at least three compactions,
checklist preservation and a real terminal foreground record. It **does not meet
the multi-hour requirement**. Durations of separate trials are not combined into
one qualifying goal run. No delay, repeated work or manufactured compaction was
added; G5 remains unqualified and #332 remains a draft.


## Fourth trial — continuation delivery in progress

- Started 2026-10-16T08:58:27.849937Z on qualified runtime source
  `27bdd77f2c6b2e5a13e186e4d285645a4f7ba931`, binary SHA256
  `2d46ac2004afe8bf88c5368d90e4a0427c8da8c0b4f4da7e7a23400fcf76ebbf`.
- The user selected `openai-codex/gpt-6.1-sol`, subscription only, with no
  paid API fallback. Worker and audit routes use the same model; optional paid
  routing/advisories and delegation are disabled. Credentials and private run
  data remain outside Git.
- A new admission has a four-hour deadline and eight goal runs; each run has
  finite64-turn,3000000-token and60-minute limits. Context is65536 and output
  is8192 tokens per request. This does not reset or extend any prior trial.
- The useful task integrates and qualifies the existing ENG-1058 continuation
  candidate from `f98f3c2e50eed69bf597e02ae635f88114191f67` against the stable
  G3/G4 stack and current main. It works in an isolated writable checkout;
  the original continuation checkout stays untouched. Local commits only.
- Acceptance includes real long-running continuation/outage/restart evidence
  and a final `G5_CONTINUATION_REVIEW.md`. No padding, invented observations,
  manufactured compactions or relaxed gates are permitted.

The task is still running. No terminal outcome or multi-hour qualification is
claimed. Later fixes in outer #330/#331 do not retroactively change the runtime
source under this evaluation. #332 stays draft pending actual evidence.


## Fourth trial — terminal outcome and qualification limits

Finished 2026-10-16T10:43:37.007571Z after **6309.158 seconds
(105 minutes)**. The supervisor captured process exit **7**, with exactly one
final foreground record, last in valid complete JSONL, status **blocked**.
The goal used **8 charged runs**, **303 completed model turns** and **19 real
compactions**. All 11 checklist identities survived every compaction; final
states were **9 done,2 dropped**. Admission bounds and the original deadline
were unchanged; the runner did not resume the goal.

- Completed-turn accounting exactly matches authoritative goal usage:
  **12237984 fresh input +1857792 cache-read input +131690 output tokens**,
  cache-write input0. Estimated cost is `null`; **paid API spend USD0**,
  subscription-amortized value unknown. The sole route remained
  `openai-codex/gpt-6.1-sol`, subscription only, without a paid fallback.
- The useful task delivered local integration merge `0a3dec71` of the pinned
  continuation candidate and runtime; final documentation head `ab8eca3a`.
  Its pre-commit integrated-source full receipt records **2817/2817 tests**,
  **16 deliberate skips**, minimal-provider **211/211**, and smoke **8/8**.
  These qualify the pinned integration, not later outer #330/#331 repairs.
- The task's actual-outage test passed after **3225.279761 seconds**, with
  **2000 work turns,5000 unique effects,425 in-run compactions**, three actual
  600-second outages and two process kills. The earlier failed attempt remains
  retained. The separate soak is loopback task evidence, not another live
  subscription goal trial.
- The task correctly left whole resource/performance acceptance blocked:
  no relevant pre-integration RSS baseline, incomplete early process-tree
  sampling, unmatched performance fixtures and absent registered resource/fsync
  budgets. Descriptive measurements do not satisfy those acceptance gates.

The terminal audit passes every recorded check except the required **7200-second
single-trial duration**. This useful trial therefore **does not qualify G5**.
Separate trial durations are not combined; no padding or cap extension was used.
#332 remains draft. Raw transcripts, credentials, session stores and detailed
receipts stay outside Git under the private evaluation directory.


## Fifth trial — matched measurements in progress

Started 2026-10-16T11:03:41.310231+00:00 on runtime source
`4851fc2e755f7997b3e0cd5086653a64286e69bc`, binary SHA256
`1f7b30c95d78187fea69f6a11630b3bb029358370b81332440da06d4ffb2f29e`. The useful task starts from local continuation
review head `ab8eca3ac072e1186fbb305aef28bb6986adf19b` and compares the relevant
pre-integration baseline `f98f3c2e50eed69bf597e02ae635f88114191f67` with an
integration of the latest goals fixes. Sources and binaries stay pinned while
outer PR repairs continue.

- The task repairs concrete missing measurement evidence: common support,
  baseline/candidate resource sampling across actual outage/kill recovery,
  fsync observations and matched turn-overhead A/B and A/A diagnostics.
  Missing registered budgets, sampler limits or host noise remain explicit;
  no old partial sample is relabelled complete and no threshold is invented.
- Exact route is `openai-codex/gpt-6.1-sol`, subscription only, no paid API
  fallback. The fresh goal has a four-hour deadline and eight charged runs;
  each run is bounded at64turns/3000000tokens/60minutes, context65536 and
  output8192. Prior trial counters and deadlines remain unchanged.
- The worker uses an isolated checkout and local commits only. It must stop
  when the useful task is done or blocked; no padding, repeated passing work
  without cause or manufactured compactions. A setup run with180-second
  outages was caught, stopped and explicitly retained as invalid. The corrected
  baseline uses600-second outages; no result or resource acceptance is claimed.

At admission the supervisor was live, with terminal accounting, duration and
checklist retention pending. The terminal result below supersedes that admission
status; the admission alone did not establish G5 acceptance.


## Fifth trial — terminal result and live acceptance

Finished **2026-10-16T13:16:12.402844Z**, after **7951.093 seconds
(132.5 minutes)**. The foreground process exited **8** at the original
**eight-run cap**, with exactly one final `budget_exhausted` record, last in
complete valid JSONL. The supervisor is stopped. No runner resume, deadline
extension, cap reset or artificial duration padding occurred.

- **441 completed model turns** and **20 actual compactions** (11 in-run,
  9 automatic). All **12 checklist IDs and texts** survived every compaction;
  final states are **6 done,2 pending,1 dropped,3 in progress**. Budget exhaustion
  is the terminal outcome; this does not claim the useful task was completed.
- Authoritative goal usage exactly equals all completed-turn usage:
  **16746945 fresh input +4194176 cache-read input +180779 output tokens**;
  cache-write input0. Estimated cost is `null`; **paid API spend USD0**,
  subscription-amortized value unknown. The sole model route was
  `openai-codex/gpt-6.1-sol`, subscription only, without a paid API fallback.
- The original four-hour deadline, eight-run cap and per-run
  64turn/3000000token/60minute limits are unchanged. Runtime source remains
  `4851fc2e755f7997b3e0cd5086653a64286e69bc`; the launch binary hash still
  matches, and its embedded version identifies that source. Later outer
  #330/#331 fixes are not retroactively qualified by this trial.
- The independent terminal audit passes every check: actual duration,
  terminal/process agreement, complete JSONL, usage reconciliation, checklist
  identity, finite unchanged bounds, exact route and source/binary binding.
  The [published audit source](../../../benchmarks/goals/audit_live.py) is
  byte-identical to the evaluator-local script used for this receipt; SHA256
  `33c1135e24204e07e9dee1bc3477efa5d0944983f0e6488751a9c6f3e2e1a646`.

**G5 / goals acceptance5 passes for this single live trial.** Durations from
prior trials are not combined. Credentials, transcripts, stores and generated
receipts remain outside Git. The private terminal receipt is
`/tmp/qq-goal-delivery-20261015/g5-measurements/g5-terminal-audit.json`.
Reproduce the audit from the repository root on the evaluation host:

```sh
python3 benchmarks/goals/audit_live.py \
  /tmp/qq-goal-delivery-20261015/g5-measurements \
  4851fc2e755f7997b3e0cd5086653a64286e69bc
```

The private input directory contains `g5-launch.json`, `g5-live.jsonl` and
`g5-exit.json`; the launch record names the preserved executable. Linux `/proc`
is used to check the supervisor's PID/start-time identity. The command writes a
private `g5-terminal-audit.json`; **`qualifies: true` and every check true** are
required. An active or incomplete trial reports false. The script and invocation
are published; raw inputs, credentials, executable and receipt remain untracked.

### Useful task result and separate qualification limits

The task produced local measurement harness commits and a pinned integration;
none of that code is included in #332. Its baseline full soak completed with
**2000 work turns,5000 unique effects,433 completed compactions** (425 in-run,
8 automatic), three actual600-second outages and two process kills. Root
independently verified exit0,69470 process-tree samples and observation of all
three admitted worker generations. Sampled peak tree RSS was71580KiB, with a
maximum poll gap of194.792ms; this is not a continuous peak bound.

At live-goal settlement the candidate capture and comparison work were still
running. The local `G5_MEASUREMENTS_REVIEW.md` remained explicitly incomplete:
closed candidate audit, matched pair summaries and a final stable-source full
check/smoke were pending. Earlier test failures and excluded captures remain
retained. No complete resource comparison, H0, storage/fsync budget, continuation
acceptance or task implementation readiness is claimed. ENG-1058 remains open;
G5 live acceptance does not waive those separate requirements.
