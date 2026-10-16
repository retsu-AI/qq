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
