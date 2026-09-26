# Ledger — supervised delegation

Plan: [`../supervised-delegation.md`](../supervised-delegation.md).
Only the agent working this plan edits this file. Current state on top;
dated entries appended below, newest last.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| D1 | Bounded continuation on output truncation | Shipped (`e074f89`) | | Protocol 16, schema 22 |
| D2 | Child accounting and authority repair | Shipped (`7a0a1e5`; H24 refresh in `893e582`) | | Per-admission remaining budgets, deadline carry, descendant spend |
| D3 | Delegation roster | Shipped (`c8bf342`) | | Descriptor 4, prompt 10 |
| D4a | Supervised write children at depth one | Shipped (`9ddbbb8`; H23 ownership in `1e6a901`, `f482b37`) | | |
| D4b | Configurable depth to three | Shipped (`9aa30e8`) | | Schema 23 |
| D5 | Heuristic final-answer audit | Shipped (`a1939d2`) | | Schema 24 |
| D6a | Compare command, arm stamping, reasoning tokens | Shipped (`428af0a`, `66f3aba`) | | Runbook `benchmarks/arms/README.md` |
| D6b | Paired runs and the default decisions they feed | Planned | | Needs spend; decides depth and worker-model defaults |
| D6a.1 / ENG-950, ENG-957 | Correct offline cross-harness summary accounting | In progress | `fix/eng-950-harbor-summary` | Local candidate reviewed; complete workspace gate passed; branch preservation pending |

## Entries

### 2026-09-08 — ledger opened

D1–D5 and D6a shipped 2026-09-03; H23/H24 from the speed-first plan landed
through this plan's D4 and D2 contracts on 2026-09-04/05. D6b is the only
open slice and is blocked on paid runs.

Shipped: none new. In progress: none. Blocked: D6b (spend).

### 2026-09-26 — D6a.1 offline summary accounting (ENG-950/957)

- Candidate `fb63aa6b` fixes unknown-cost accounting and exception-bearing false passes; deterministic malformed-input, zero/unknown, overflow and no-op controls added.
- Bash 3/5 regressions, syntax, ShellCheck, fmt, strict workspace Clippy and workspace build passed; exact source tree independently accepted as `ddd013f82c5f9ba93e2bf886f591a64984763810`.
- Ambient workspace runs failed on temporary-path, inherited-credential and ANSI-color assumptions; successful targeted reruns do not replace the required complete workspace invocation.
- Complete workspace gate is queued with canonical temporary directory, stripped credentials, ANSI colors and serial tests; status remains In progress until that invocation succeeds.
- Local patch/bundle and raw receipts retained in the manager handoff `reviews/employee-organization-2026-09-09/roles/engineering/benchmark-20260926/HANDOFF.md` (manager workspace, outside this repository).
- No paid model runs, provider changes, publication or merge; D6b and broader evaluation/spend gates remain open.

### 2026-09-26 07:34 UTC — D6a.1 complete workspace gate

- Tested commit `d365a01ec559759d02520bfda6271215a814024f`: `cargo test --workspace --no-fail-fast -- --test-threads=1` exited 0, with 1,908 passed, zero failed and five intentionally ignored tests.
- Pinned Rust 1.97.1; canonical TMPDIR, inherited credentials removed, NO_COLOR unset, TERM=xterm-256color and COLORTERM=truecolor. No tests or Rust code changed to obtain this result.
- Full command ran 07:30:16–07:33:55 UTC; raw output `local-test-workspace-clean.log` and result JSON retained beside the manager handoff. Prior failed runs remain retained.
- Earlier fmt, strict all-feature Clippy and build passes apply to identical source; subsequent changes are ledger-only. Independent source and ledger reviews retained. No PR, merge or paid trials; Manager owns integration and publication-cost resolution.
