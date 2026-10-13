# Credential attribution diagnostics progress

| slice | goal | status | branch/PR | notes |
|---|---|---|---|---|
| DI1 | Secret-free, default-off attribution for existing credential-read routes | In review | `fix/qq-credential-preparation-20261013` / no PR | Revised design awaiting independent exact-identity verdict |

## 2026-10-13 — revised design checkpoint

- Resolved the independent request for changes by specifying the explicit `--credential-diagnostics PATH` CLI activation, shared process-local session across normal and separately constructed TypeSafe stores, typed JSONL framing, bounded writer/summary/deactivation behavior, and output failure semantics.
- Replaced fabricated stored-follower cancellation with real `OuterCancelRequested`/`DrainObserved` hooks only where core/runtime context exists; stored Condvar followers remain non-cancellable and later backend return/finish remain separate.
- Added variant-specific events, per-attempt/flight/read IDs, legal ID/outcome rules, exhaustive route/hook table, shared session ID domain, saturating exhaustion, total record/byte budgets, trailing-loss summary, and lock-release-before-emission ordering.
- Expanded owned implementation/test paths and the credential-free public CLI composition fixture with literal disabled/enabled/deactivated, cancellation/drain, privacy, overflow, consumer-failure, poison, cache/follower, separate-store, OAuth, and ID-exhaustion checks.
- No source edits, builds, credentials, provider calls, live process/UI actions, or output recordings performed. Current c70 source/release/install remain untouched.
- Design identity after this revision is recorded by the parent from the exact worktree; next action is the same independent verifier’s read-only verdict. Implementation remains unauthorized until approval.

## 2026-10-13 — v2 finite corrections

- Repaired the four independent-review findings: exclusive-create/no-follow final output path with pre-runtime failure; exact 250 ms completion-signal shutdown and incomplete-evidence semantics; consistent `TypesafeReviewer`/`Typesafe` route pair with real PID and no fabricated run/thread/cancel plumbing; inclusive 255 ordinary-record plus reserved-summary budget within 256/128 KiB, saturating drops, and exact-bound tests.
- The held-backend fixture now requires no `BackendReadReturned` before release and no OS-cancellation claim. Path safety, default silence, writer failure/timeout, missing summary, trailing loss, and exact budget cases are explicit.
- No source edits, Cargo actions, credentials, provider calls, live actions, or output recordings performed. Current c70 source/release/install remain untouched.
- Next action: same independent verifier reviews this exact revised design identity; implementation remains held until approval.

## 2026-10-13 — v3 textual corrections

- Removed all `qq-core` execution-hook/test allowance and any implication that run/thread/cancel context is available; the route table now states it is unavailable and omitted.
- Defined each JSONL line as <=512 bytes including newline; ordinary events are capped at 255 records/130560 bytes, with one 512-byte summary reserved inside inclusive 256-record/131072-byte totals. `Summary.bytes` excludes itself; line-cap rejection is typed and input-free.
- No source, Cargo, credential, provider, live process, or recording action performed. Frozen for final independent exact-design review.

## 2026-10-13 — implementation repair checkpoint

- Candidate `be8af4fac449180142ac21f5f483b937d99d03b5` adds process/session/PID/monotonic metadata, typed stored-keyring owner/join/read/finish events, atomic ordinary-record/byte reservation, open-time private exclusive creation, and concurrent budget/symlink regressions.
- Focused evidence is retained at `.../qq-credential-preparation-20261013/DI1-auth-focused-final.log` and `DI1-cli-focused-final.log`; the auth selector executed 1 test and the CLI selector executed 5 tests, all passed. Earlier failed selector/assertion logs remain retained with their original outcomes.
- Current limits remain material: TypeSafe reviewer construction, Codex/XAI OAuth hooks, full route admission/owner/unwind variants, and a real public CLI composition fixture are not yet demonstrated. This checkpoint is therefore pending same-reviewer repair/acceptance; parent owns broad gates.

## 2026-10-13 — hook expansion checkpoint

- Candidate `3df536d2077bdceeb4abd6a6a8559a38f32ae02d` adds distinct reviewer-store construction for opt-in runtime factories and typed Codex/XAI request-time read start/return events, while preserving the no-diagnostic `RuntimeFactory::system()` path.
- Stored events now include owner/join/finish lifecycle records; endpoint classification remains enum-only and never serializes URLs. Focused auth and CLI selectors passed after this change (1 and 5 executed tests respectively); parent still requires a real public composition fixture and same-reviewer exact-head assessment.
