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
| AF5 | Structured tracing | Planned | — | ENG-1028; ADR required |
| AF6 | Remaining test audit | Planned | — | ENG-1028; AF6.1 cancellation shipped |
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
