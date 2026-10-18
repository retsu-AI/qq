# Ledger — Developer experience

Plan: [remaining work](../developer-experience.md). Predecessor evidence is linked there.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| consolidation | Consolidate current owners | In review | #323 | Docs only |
| GE1 | `agents.md` | Planned | | ENG-929; after GE0, GE10, GE4, GE5 |
| GE2 | `sessions.md` | Planned | | ENG-930; after GE0; ADR-0038 is Proposed — label it |
| GE3 | `skills.md` | Planned | | ENG-931; after GE10, GE4, GE5 |
| GE9 | `workflows.md` | Planned | | ENG-981; after GE1–GE3 |
| GE4 | `environment.md` | Planned | | ENG-932; `configuration.md` hands off |
| GE5 | `keybindings.md` | Planned | | ENG-933; `tui.md` hands off |
| GE6 | `server.md` | Planned | | ENG-934; fixes the `design/protocol.md` route list |
| GE7 | `enterprise.md` | Planned | | ENG-935 |
| GE8 | changelog page | Planned | | ENG-936; CHANGELOG.md exists; old first-release prerequisite satisfied |
| AF5 | Structured tracing | In progress | `feat/eng-1028-af5-tracing` | ENG-1028; ADR-0063 reserved; clean baseline `target/qq-perf/af5/baseline.json` (77 metrics, 100 samples) |
| AF6 | Remaining test audit | Planned | — | ENG-1028; AF6.1 cancellation and AF6.2 delegation (#325, `bd17a206`) shipped; further modules remain |
| AF9 | Review lesson mining | Planned | — | ENG-1028; initial lint rules and AF9.1 shipped |
| AF10 | Skill evaluation | Planned | — | ENG-1028; 3–5 task replay |
| AF7 operator | Merge-queue activation | Planned | — | ENG-1028; CI shipped; branch protection not observed |

## Consolidation session

- Removed shipped implementation checklists and overlapping ownership. Preserved
  numeric acceptance, IDs, failure receipts and historical ledgers.
- No runtime, protocol, schema, default, budget or tracker-state changes.
- Verification: initial cargo xtask check passed fmt/lint (no affected Rust
  package); local Markdown file-link audit and git diff --check passed.
  Independent review found AC15 latency-budget acceptance missing; restored it.
  Final documentation gate reruns after this receipt. No runtime tests or
  live/performance qualification claimed.

## Codex review follow-through

- Carried remaining GE/AF states from predecessors; no active branch was
  recorded for these planned rows. Verify live Linear assignments before work.
- Initial commit full gate and smoke passed; follow-up documentation gate pending.


## AF5 restart checkpoint

- Worktree `.worktrees/af5-tracing`, branch `feat/eng-1028-af5-tracing`, based on `bd17a206`; no PR/push yet. ENG-1028 remains open.
- ADR-0063 reserved before design. Implemented opt-in `QQ_LOG=info`, metadata-only run/turn/provider/tool/store spans, bounded writer and exclusive retention slots; no schema/wire change.
- Full CI-profile check passed before independent-review fixes (412.7s); final source needs a fresh full gate. Five logging tests passed; real-binary `structured-tracing` smoke passed after state-dir/teardown fixes.
- Independent review fixed data-vs-state location, Unix no-follow directory traversal, and writer shutdown ordering after runtime teardown. Review again before PR. Non-Unix traversal remains precheck-based and needs qualification; no Windows claim.
- Performance baseline: `target/qq-perf/af5/baseline.json`; candidate: `candidate-complete.json`, 77 metrics, 100 samples/10 warmups. `candidate.json` is an interrupted partial recording; retain, do not treat as evidence.
- Budget comparison FAILED: server-ready p95 445256978 ns vs relative 310386433; minimal binary 43038608 > 41000000 bytes; minimal dependency closure 305 > 300; snapshot p95 7402739 ns vs relative 7269491. No budget widened. A/A control not yet recorded.
- Resume: reduce tracing dependency/size cost or otherwise fix absolute gates; investigate relative failures with same-binary A/A, not retries-as-proof. Strengthen ancestry/cancellation/error redaction tests; review log limits/usability and portability.
- Then rerun focused tests, all smoke (new total 7), full CI-profile gate, website checks for guide edit, qualifying perf comparison, independent review; open PR per playbook, request Codex and explicitly resolve findings. Require current-tip Codex actual 👍 before declaring merge-ready.
- User requested immediate local checkpoint to restart machine. No passing final gate, completed AF5, open PR, or merge readiness claimed.


## AF5 resumed verification

- Added unique process-local run ordinals, instrumented outer stream polling, and retained store span ownership through worker settlement. Shifted seven legacy sleep line keys by +2 only; ceiling stays 73.
- Full CI-profile gate initially failed seven retried-pass fixtures under concurrent host load; focused checkpoint rerun passed. Subsequent complete gate passed (428.0s), including doctests and minimal provider checks. All seven real-binary smoke scenarios passed.
- Clean baseline minimal artifact already exceeds absolute size budget: 42288944 > 41000000 bytes; baseline dependency count 300. Initial AF5 adds 749664 bytes and five crates. This is not an unchanged-budget pass; reducing cost remains required.
- Attempted post-restart recording timed out at 600s and is partial, not A/A evidence. Other test jobs and desktop load were observed on host; no unrelated processes terminated.
- Still blocked on qualifying performance, Windows no-reparse path behavior and expanded cancellation/correlation coverage. Open only as draft until these and current-tip Codex review are complete.


## AF5 Codex follow-through, PR #329

- Codex reviewed `832edb25`: special-tool span coverage, side-write store spans, QA-root isolation. Local fixes instrument both special branches and retain explicit side-write spans through settlement. QA-root plus logging is explicitly refused before creating files, preserving fixture freshness/isolation.
- Enabled logging is explicitly unsupported on non-Unix rather than following reparse points. Default behavior unchanged; platform scope documented.
- Replaced tracing-subscriber with a bounded numeric-only local subscriber to remove five added closure crates. Six focused logging tests pass; 7/7 real-binary smoke pass including QA refusal. Full gate passed before the final typed QA diagnostic fix (273.7s); final gate rerun required.
- Independent subscriber review corrected cross-dispatch TLS isolation, saturating reference counts and serialization/send outside the span-state lock. Further concurrency/bounds coverage remains.
- `control-final.json` completed 77 metrics after restart; pre-restart comparison refuses different memory capacity. `lean-complete.json` restores dependency budget but minimal artifact still 43039856 > 41000000 bytes and relative tails fail versus control. This is NOT qualifying A/A or a budget pass. No budgets widened. User requests merge only after merge readiness plus current-tip Codex actual 👍.


## AF5 current-main reconciliation and size qualification

- Merged main `a9bff3ef` in `c85c0366`; resolved exact legacy sleep locations without changing ceiling. Latest three Codex threads explicitly resolved after scheduler-run persistence context and platform smoke fixes; fresh review requested.
- Candidate package release profiles use `z` for qq/core/tui/provider and paired xtask; minimal local artifact 40194448 bytes, below unchanged 41000000 cap. These profile changes remain provisional pending latency qualification; no runtime limits changed.
- Fresh main baseline and A/A: `.worktrees/af5-perf-baseline/target/qq-perf/af5/{baseline-current-main,baseline-aa}.json`. Candidate: `target/qq-perf/af5/compact-candidate-complete.json`. All complete 77 metrics/100 samples. Earlier partial files are not evidence. Old pre-restart reports no longer exist in target.
- Current main and candidate have IDENTICAL normal dependency closure sets, 305 crates; current main itself fails unchanged 300 ceiling. Main A/A also fails many relative tails, so local environment cannot currently qualify latency differences. Candidate comparison remains failed, not waived.
- Full gate failed twice at new-main headless drain fixture's shared 10-second deadline across 16 complete runtime setups; focused case passed in 5.78s. Moved the same 10-second guard inside each iteration (all 16 assertions retained). Full CI-profile gate then passed 278.8s and smoke 9/9. This is a timing-guard correction, not evidence that initial gates passed.
- Remaining: qualify profiles and tracing on controlled host, close inherited dependency-budget violation without widening, update ADR/build rationale, final source gate/CI and actual current-tip Codex 👍 before ready/merge.


## AF5 terminal-span review and controlled recording

- Current-tip CI and dispatched run [39091894930](https://github.com/retsu-AI/qq/actions/runs/39091894930) passed functional, Windows and WASM checks. Controlled turn recording completed, but is diagnostic only, not an H0 budget gate: median core work gaps changed +3.86%/+4.97%/+1.50% at turns 10/100/1000. Provider/TUI/startup/streaming qualification is absent.
- Removed provisional qq-core/qq-provider/qq-tui `z` overrides rather than shipping unqualified hot-path code generation. Prior 40194448-byte receipt does not qualify the resulting artifact; size and full performance acceptance remain open, without budget changes.
- Codex identified direct streams retaining run/turn handles after terminal events. New real-subscriber regression failed with zero run-close records while retaining the terminal stream; wrapper now drops suspended phases and its span before returning terminal results, including the deadline wrapper. Success/failure matrix and all seven logging tests pass after the fix. Full CI-profile gate passes (347.6s); real-binary smoke 9/9 passes.
- All 15 earlier threads remain resolved; two new threads await verified fixes/replies and explicit resolution. No current-tip Codex thumbs-up or merge readiness claimed.


## AF5 root-profile and workflow fallback review

- At `a60224f0`, current-tip CI is green and all17 prior Codex threads are resolved. Two further findings require restoring root qq `s` optimization and honoring top-level release optimization in artifact validation.
- Restored qq and paired xtask `s` profiles; no unqualified `z` treatment remains. Updated validation to use the identical package→release→3 fallback as the build. Added four-case workflow expression regression: top-level `s` and `2` fail before fix; all5 fixture tests pass after.
- Minimal artifact at prior a60224f0 is42883152 bytes, above unchanged41000000 cap; this is not a passing size receipt for the restored profile. Full performance/size and fresh current-tip Codex thumbs-up remain required. Final gate pending.


## AF5 paused/context terminal and special-tool lifetime review

- `af394f8d` CI is green. All19 earlier threads explicitly resolved; Codex adds performance acceptance plus two span-lifetime findings. Performance acceptance remains open, not dismissed or waived.
- Public adapter now releases its internal stream before every terminal result, including context rejection and paused→failed conversion. Added five-path drop-guard regression (success, failure, pause, budget, context refusal); passes without dropping the retained public stream. Selector/goal tool handles release before completion yields, including interrupted goal completion. Focused selector/goal tests3/3 pass.
- First full gate refused shifted legacy sleep keys after adding regression; adjusted only the two exact lib.rs line numbers (+71), no new sleeps or ceiling change. Full final gate reruns next.
- Interrupted release-build call has no durable result and was not retried; existing binary remains prior42883152-byte artifact. Read-only performance investigation confirms hosted turn job is diagnostic; existing full H0 workflow is hardcoded to another branch/baseline and needs a bounded generalized immutable-SHA dispatch before current-tip qualification. No safe five-crate subtraction established.


## AF5 hosted full-H0 dispatch

- Added opt-in `perf_h0` dispatch using immutable baseline and dispatch candidate SHA. Reuses full baseline/candidate/same-binary-control recorder, exact budget checks, pressure refusal and retained evidence; no thresholds changed. Historical retry-contract exception remains restricted to its original pinned baseline; all other comparisons require identical budget documents.
- New workflow-contract regression accepts identical current budgets and rejects a changed retry threshold. Fixture suite6/6 passes; full gate pending before publishing/dispatch. The current performance thread stays open until an actual qualifying result, not merely a successful recording.

- Full gate exposed inherited audit allowance fixture timeout: shared10s across17runs/8audits expired under workspace load, retry and focused rerun passed. Kept10s stalled-progress guard, refreshing only when persisted audit counter advances (bounded8), retaining exact17run/8audit/no-ninth-request assertions. Audit suite4/4 passes; final full gate reruns. No failed attempt reported as success.

- Further Codex fixes: provider attempt handle releases on terminal/error/EOF/interrupt before synthesized output; real-subscriber incomplete/EOF reasoning regression passes, logging8/8. Debuginfo arm mismatch now refuses with four-setting regression (fixture suite7/7). ADR0063 moved into contiguous index table. Actionlint caught malformed new dispatch input nesting; corrected it, focused actionlint passes with only pre-existingSC2129 style excluded.
- Full parallel gate then hit three inherited compaction deadlines; focused exact cases pass serial31.6s. Lower-concurrency synchronous gate killed at600s is incomplete. Async full two-process gate remains running under shared-host load; no final-source pass claimed yet.

- Async two-process run completed all tests1205.0s plus fmt/lint/Clippy/doctests/minimal checks, but correctly refused its report because script/docs/workflow changed during execution. This is not a final gate pass. Commit stable snapshot then rerun without edits before push. Latest smoke9/9, fixture7/7 and focused actionlint pass.

- Stable gate at3e565e67 failed a retried-pass side timeout fixture: finite5s real-time clock keeper exited before native read entry under load, advancing paused time prematurely. Replaced finite keeper with explicit lifetime and independent10s entry watchdog; targeted case passes1.38s. Only two shifted legacy line keys changed(+11), ceiling73 unchanged. Stable full gate rerun pending; no push yet.


## AF5 current-tip clean review and CI deadline repair

- Stable8bdd246c full gate passes1078.0s (2785/2785, minimal211/211), pushed; zero unresolved threads. Codex receipt6247771317 reviews exact8bdd246c: no major issues. Actual thumbs-up not observed. HostedH0 run39112300274 remains running; Windows/WASM pass.
- PR CI failed inherited answer-wait deadline test:1500ms budget elapsed during setup before wait receipt under runner load. Changed fixture to paused clock with explicit wait-entry signal and independent10s watchdog, then1501ms advance; resume real time for persistence/cleanup so fake clock cannot outrun blocking I/O. First paused cleanup attempt timed out, final focused test passes0.37s. Same assertions, budget and ceiling retained; two legacy keys shifted+20 only. Full final gate pending.

-791efffe stable full gate passes1005.4s, pushed. Manual functional gate also exposed headless trace fixture race: large delta could precede attachment cursor, so buffered/dev/full failure never occurs. Test now gates its sole large delta until foreground subscription's interrupt poll; retains payload/timeouts/assertions. Focused pass0.31s. Eight headless legacy line keys shifted+15 only; final gate pending. HostedH0 still running; no performance pass.


## AF5 measured dependency subtraction and size blocker

-4d1075a0 stable full gate passes911.4s and pushed. HostedH0 run39112300274 fails unchanged caps: full48444280>48000000, minimal43645144>41000000; A/B replay737865>729073ns. Same-binary A/A fails command ack/long-stream tails and long-stream MAD, so latency remains unqualified. Artifacts `target/af5-hosted-h0`, no waiver.
- Disabled unused crossterm derive-more and pulldown-cmark CLI/HTML defaults, preserving bracketed paste/events/Windows/parser. Measured Linux minimal normal closure299 (was305); six helper crates absent. TUI393/393 passes (1skip). Local minimal release still43824208>41000000; removal fixes dependency count, not binary-size gate.
- Typed subscriber serialization experiment passes logging9/9 but grows artifact1024bytes to43825232; reverted experiment rather than retaining unmeasured complexity. Release optimization/budgets unchanged. Full final gate for manifest/lock subtraction pending.