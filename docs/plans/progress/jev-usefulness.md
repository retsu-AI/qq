# Ledger — Jev usefulness

Owner: agent on `docs/eng-938-jev-usefulness-plan` (JU0); implementation
ownership is assigned per slice when accepted. [Plan](../jev-usefulness.md).
Parent tracking: [ENG-938](https://linear.app/retsu-ai/issue/ENG-938) under
ENG-791. Raw evidence: `target/qq-perf/jev-audit-2026-09-25/` (not committed).

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| JU0 | Plan, proposed ADR-0046, qualification runbook, audit, PR 187/189 comparison | In review | `docs/eng-938-jev-usefulness-plan` / [ENG-938](https://linear.app/retsu-ai/issue/ENG-938) | Docs only; base `ac28bf2`; draft PR |
| JU1 | Effective consent, cache/profile correctness, reliable Off | Planned | — | Reproduced stale on-to-off probe is the first regression |
| JU2 | Authoritative bounded task context for approval | Planned | — | Root brief gap; shares session files with JU3 (one writer) |
| JU3 | Durable delegate-vs-human lifecycle; Jev-only headless waiting | Planned | — | Split JU3.1 phases/fixtures, JU3.2 TUI/headless consumers |
| JU4 | Per-attempt receipts, admission, cancellation/recovery accounting | Planned | — | Depends on JU3 |
| JU5 | Precision-safe parsing and remote-vs-local explanation | Planned | — | Reuse #189 checkpoint feedback if it lands |
| JU6 | Narrow semantic approval pilot (shadow first) | Planned | — | Needs approved pilot scope; no authority expansion |
| JU7 | One low-authority acceleration experiment (routing) | Planned | — | Optional; must not block approval repairs |
| JU8 | Paired qualification and opt-in rollout decision | Planned | — | Paid runs stay with ENG-809/811/815 |

## Entries

### 2026-09-25 — JU0 planning

- User requested a plan, supporting docs, a PR, and a comparison with PRs
  187/189. Created ENG-938 (plan-only, under ENG-791, related to ENG-862,
  ENG-917, ENG-811) through Executor; status In Progress.
- Isolated worktree `.worktrees/eng-938-jev-plan` on branch
  `docs/eng-938-jev-usefulness-plan` from fetched `origin/main` at `ac28bf2`.
  The root checkout kept the earlier uncommitted audit files unchanged.
- Copied the pinned audit (baseline `9f2d82d`) into `design/`, scoped its
  32-review/two-repair wording to baseline `enforce` (not #187 Strict), and
  linked the plan and comparison from it.
- Wrote `plans/jev-usefulness.md` (JU0–JU8, C1–C8), Proposed ADR-0046,
  `runbooks/jev-qualification.md`, and `plans/jev-pr-comparison-2026-09-25.md`.
- Reserved ADR-0046 in `root.md`; recorded the collision between draft #187's
  ADR-0044 and the local ENG-937 ADR-0044 for root reconciliation.
- Comparison pinned PR 187 head `bdd90fb8` (base `main`) and PR 189 head
  `3e7eff09` (base `feat/eng-791-strict-verification`); both open drafts, no
  posted GitHub reviews or comments at inspection. Diffs `dc07e29...bdd90fb8`
  and `bdd90fb8...3e7eff09` read locally; approval gate, tool-call context
  loader, and routing adapter unchanged in both.
- Independent read-only review via `spawn_agent` failed twice on the selected
  route: session-context limit, then provider HTTP 401 (rejected API key).
  No independent non-author review of these documents is recorded.

### 2026-09-25 — JU0 verification (docs only, unchanged Rust)

- `git diff --check`: clean. Relative-link and anchor validation over the
  twelve new/edited Markdown pages: 166 links, 0 broken, no trailing
  whitespace, final newlines present.
- `cargo fmt --all -- --check`: passed.
- `cargo clippy --locked --offline --workspace --all-targets --all-features
  -- -D warnings`: passed (shared `target/`).
- `cargo test --locked --offline --workspace -- --test-threads=4` in the
  isolated `target/ju0-verify` directory: all suites passed, 0 failed;
  binaries 243+1+50+22+105+725+1+13+48+3+4+228+19+0+27+330+0+6+64, with
  the existing gallery, workspace-index measurement and bench ignores.
  An earlier shared-`target/` run with default threads failed only
  `sessions::tests::delegation::steering_charges_a_completed_but_unconsumed_child_exactly_once`
  by `Elapsed` under load; it passed alone and in the isolated run. Not
  caused by this Markdown-only diff; noted for the run-reliability owner.
- `cargo build --locked --offline --workspace`: passed.
- Minimal-provider profile and provider benchmarks not applicable: no Rust
  or manifest changes. No live Jev call, credential read, or paid run.
