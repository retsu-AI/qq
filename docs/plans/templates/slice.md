# Slice template

Copy the relevant parts into the ledger row, the ledger receipt, and the PR
body. Keep it terse; link rather than repeat.

## Slice header (in the plan, if the plan does not already have one)

```text
### <ID> — <goal in one line>
**Inputs:** <slice ids that must be merged>
**Owned paths:** <crates/dirs/files this slice may touch>
**Gates:** <named performance gates and their budgets>
**Acceptance:** <tests/commands/measurements that prove it>
**Docs:** <design sections / ADR / runbook to add or update>
```

## Pre-flight (agent, before coding)

- [ ] Read `AGENTS.md`, `docs/plans/workflow.md`, the slice section, and the
      design docs it names.
- [ ] Inputs merged: `git log origin/main --oneline | grep <input id>`.
- [ ] No conflicting `In progress` slice on the same paths in any ledger.
- [ ] Branch from current `origin/main`; isolated worktree if another writer
      is active.
- [ ] Pre-change baseline recorded for every named gate
      (`docs/runbooks/perf-recording.md`); path noted in the ledger.
- [ ] Ledger row set to `In progress`.

## Working

- [ ] First commit is the failing test named in acceptance.
- [ ] Only owned paths touched, plus the ledger and named docs. Otherwise stop
      and file a root request.
- [ ] Errors are domain enums (`thiserror`); no `Box<dyn Error>` in library
      interfaces; sources preserved.
- [ ] No synchronous lock held across `.await`; no blocking I/O on Tokio
      workers; every queue, channel, task, and output bounded.
- [ ] Persist-before-publish preserved; a failed write never presents as
      durable.
- [ ] No `mod.rs`; children declared from a sibling file.
- [ ] Design docs amended; ADR written if `docs/adr/README.md` § When applies.
- [ ] Workspace gates green; minimal provider profile when `qq-provider`
      changed; relevant benches rerun.

## Ledger receipt (≤ 15 lines)

```text
#### <ID> receipt — YYYY-MM-DD
Commit(s): <sha> …
Tests: <n added>; workspace <passed>/<ignored>; <named regression tests>.
Gates: <metric> <before> → <after> (<budget>); A/A control <result>.
Deviations: <none | what and why>.
Docs: <files>.
Open: <none | items carried to which slice>.
Evidence: target/qq-perf/<slice>-<date>/ (untracked).
```

## PR body

```text
## <ID>: <goal>

**Spec:** docs/plans/<plan>.md#<anchor>   **Linear:** DEV-<n>

### What changed
- …

### How verified
- `cargo test --workspace` (<passed>/<ignored>)
- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo test -p qq-provider --no-default-features --features test-support` (if provider changed)
- <acceptance tests/commands by name>

### Measurements
| Gate | Before | After | Budget |
| --- | ---: | ---: | ---: |

### Docs
- <design/ADR/runbook files>

### Compatibility
- PROTOCOL / DESCRIPTOR / schema: <unchanged | n→m with fixtures>

### Follow-ups / decisions needed
- <none | items>
```

## After merge

- Ledger row: `Shipped (<sha>)`; receipt appended.
- Plan status block and task index updated.
- Linear issue closed with the PR link.
- Remote branch deleted.
