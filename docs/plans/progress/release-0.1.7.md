# Ledger — v0.1.7 release preparation

Plan: [`../release-0.1.7.md`](../release-0.1.7.md).
Owner: ENG-1054, `docs/eng-1054-0.1.7-pre-release`.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| R017-prep | Reconcile docs and review candidate readiness before bump | In review | `docs/eng-1054-0.1.7-pre-release`, [#316](https://github.com/retsu-AI/qq/pull/316) | Rebased onto `0b4f29e3`; no bump, tag or publication |
| R017-perf | Release size/performance repair and qualification | Blocked (quiet-host H0 qualification) | ENG-834, #315 (merged `21332992`) | Repair merged; size passes, H0 A/B and A/A failed; acceptance remains open |
| R017-bump | Separate `chore/0.1.7` bump PR | Blocked (preparation and pre-bump gates pending) | `chore/0.1.7` (planned; no PR) | `cargo xtask release 0.1.7` only after handoff |
| R017-publish | Tag merged main and verify assets/install routes | Planned | — (merged main; no PR yet) | Separate release authorization; never tag this branch |
| R017-docs | Move guide pins after assets publish | Planned | — (follow-up PR not opened) | `cargo xtask release --docs`; not before publication |

## Entries

### 2026-10-15 — preparation started

- Reviewed fetched main `3b763ea1` and live open PRs, hosted check results and
  Linear. #306/#282/#311 are merged; #308/#309/#313/#314/#315 are not.
- Reconciled plan indexes/status and ledger top tables by exact merged SHA;
  retained all missing live/performance evidence and append-only histories.
  User explicitly requested repository-wide release-doc reconciliation.
- Corrected route inventory, descriptor history, guide upgrade instructions,
  ADR index placement/allocation and the stale Windows/H10 dependency.
  Formal Proposed ADR statuses are preserved, not silently accepted.
- Added the release scope, explicit open gates, deferred PR disposition,
  contract delta table and later bump/tag/docs handoff. No runtime change.
- Baseline hosted main CI/website pass; controlled performance jobs are
  skipped. #315 turn overhead passes but H0/size is still running. Local
  documentation and release-candidate verification will be recorded below.


### 2026-10-15 — local verification

- `cargo xtask check` passed: affected qq/xtask, fmt/lint/Clippy/tests, 59.3s.
- `cargo xtask check --full` passed: 2,464 workspace tests, 14 explicit skips;
  doctests and minimal-provider Clippy/tests (211/211) pass, 329.3s.
- `cargo xtask smoke`: all six real-binary loopback scenarios pass, 2.51s.
  `git diff --check` passes. These prove this documentation candidate, not
  release-size/latency budgets, paid evaluation or live-use qualification.
- #315 advanced to integrated head `2f7c5af2`; its author still says do not
  merge before fresh performance qualification. No acceptance inferred.

- Independent review requested changes: require full backup/rollback guidance
  in the new release section, repair a split navigation bullet, and avoid a
  permanent runbook link to a temporary plan. All three corrected; cleanup
  explicitly removes temporary navigation/index references. Final gates and
  independent follow-up review run before publication of the PR.


### 2026-10-15 — PR opened and final-head follow-through

- Opened [#316](https://github.com/retsu-AI/qq/pull/316), source `f46bc741`.
  Independent follow-up review: Approve; all three findings addressed.
- First clean-head full run: 2,463/2,464 passed; existing child-summary test
  timed out at the 30-second event wait. Narrow rerun passed in10.50s.
  A subsequent unmodified full run passed2,464/2,464,14skips; doctests and
  minimal provider211/211 passed,293.7s. Retain timing instability as an AF6
  follow-up, not a defect fixed by this documentation PR.
- Final-head smoke6/6 passes,1.80s. PR body includes exact full evidence and
  the prior failure. This entry changes bookkeeping only; final affected gate
  follows on the ledger commit. Release performance/size still not accepted.


### 2026-10-15 — Codex review fixes

- Both Codex findings are valid: blank lines split the ledger inventory and
  ADR allocation Markdown tables. Removed both separators so the new rows
  remain attached to their headers and preceding entries.
- No release-scope or qualification change. Verify table continuity and the
  affected documentation gate before replying to the two review threads.


### 2026-10-15 — additional Codex finding and main rebase

- Added the workflow-required Branch / PR column; moved preparation ownership
  out of Notes and identified planned handoff branches without inventing PRs.
- Rebased onto `0b4f29e3`, resolving six documentation conflicts while retaining
  both append-only histories, ADR-0062 and #317's feedback-gate repairs.
- #315 merged as `21332992`; recorded size/correctness passes are not H0
  acceptance. Its audited A/B and A/A failures still require a quiet host.
- No runtime, version, contract, changelog or guide-pin changes. Final affected
  gate and table-continuity checks required before publishing the rebased head.

- Rebase gate first exposed main’s stale legacy-sleep inventory after #315/#317.
  Removed the deleted sleep entry and updated three line locations; no sleep
  added, no test changed, ceiling remains 76. Final affected gate passes
  fmt/lint/Clippy/tests (28.5s); table structure and diff checks follow.
