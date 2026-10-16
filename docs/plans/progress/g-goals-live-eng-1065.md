# G5 — subscription-only live goal evaluation

Status: first trial settled; second trial running; G5 acceptance is not established. ENG-1065 / ENG-809.
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
`9593d83cad54b216f69f369db5c1565fc854a97e2815eb1eb2ac9c1da37bc437`. This is the final qualified G4 source from #331.
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
with no paid API fallback. Final duration, compactions, code/test results,
terminal state and usage are pending. Neither trial is currently a G5 pass.
