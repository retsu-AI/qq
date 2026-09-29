# Ledger — Guide expansion

Plan: [`../guide-expansion.md`](../guide-expansion.md). Only the agent
working this plan edits this file. Current state on top; dated entries
appended below, newest last.

| Slice | Page | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| GE0.1 | versions checked; prose corrections | In review | `fix/eng-979-ge0-1-versions` | ENG-979; stacked on #186 |
| GE0.2 | samples run in CI | Planned | | ENG-979; stacked on GE0.1 |
| GE0.3 | reference tables mirror the code | Planned | | ENG-979; stacked on GE0.2 |
| GE10 | `concepts.md` | Planned | | ENG-980 |
| GE1 | `agents.md` | Planned | | ENG-929; after GE0, GE10, GE4, GE5 |
| GE2 | `sessions.md` | Planned | | ENG-930; after GE0; ADR-0038 is Proposed — label it |
| GE3 | `skills.md` | Planned | | ENG-931; after GE10, GE4, GE5 |
| GE9 | `workflows.md` | Planned | | ENG-981; after GE1–GE3 |
| GE4 | `environment.md` | Planned | | ENG-932; `configuration.md` hands off |
| GE5 | `keybindings.md` | Planned | | ENG-933; `tui.md` hands off |
| GE6 | `server.md` | Planned | | ENG-934; fixes the `design/protocol.md` route list |
| GE7 | `enterprise.md` | Planned | | ENG-935 |
| GE8 | changelog page | Planned | | ENG-936; blocked on the v0.1.5 release writing `CHANGELOG.md` |

## Entries

### 2026-09-25 — plan opened

Successor to the onboarding plan, which closed the same day with every
slice shipped (receipt: [`onboarding-ux.md`](onboarding-ux.md)). The eight
pages here are the routes the v0 site design had that `docs/guide/` did not
back; OB12 dropped them rather than ship stubs. Research for each page
(sources, existing coverage to link rather than repeat, the user questions
to answer, the docs-truth surface to add) is folded into the plan's slice
table. Two design-doc drifts surfaced while researching GE6 and are
assigned to it: `design/protocol.md` names `PROTOCOL_VERSION` 28 where the
code says 30, and its route list omits `POST /v1/sessions/effort`. GE8
cannot land until a release writes `CHANGELOG.md`; the generator shipped in
#160 but `main` has not been released since.

### 2026-09-29 — plan revised after a fact-check against `main`

Rebased on `main` (7885f2c). The only conflict was the plan index in
`progress/README.md`; both sides were kept. Every code claim in the slice
table was re-checked against `main`. Five statements the live guide makes
are false. They become GE0, and it goes first:

- Quitting the TUI that owns the server cancels its runs.
- `qq run` never connects to a server; it takes the store and exits `4`
  while a TUI or `qq serve` holds it.
- `Alt-Up` does not focus the parent.
- Six policy keys marked "any" in `configuration.md` are managed-only.
- The precedence table lacks two `QQ_*` variables and the Windows policy.

Fixes to the table:

- GE2 said runs continue on a background server and that the TUI and
  `qq run` share one.
- GE5 listed `Ctrl-K` as kill-to-end-of-line; it opens the palette.
- GE4 treated `APPDATA` as an override and planned a `QQ_EVAL_ARM`
  allow-list entry that has nothing to exempt.
- GE6 called ADR-0016 Proposed; it is only reserved.
- GE7's list of what a manifest cannot do was incomplete.

Missing limits and variables were added (child counts, the 1 MiB mention
total, `XDG_RUNTIME_DIR`, the AWS file variables).

Two pages were added for newcomers, who need them before the reference
pages: GE10 concepts/glossary, and GE9 common workflows. Nothing in the
guide defines a session, run, grant or held call in one place, and no page
walks through a real task. Acceptance 2 now requires every behavioral
claim to be checked on a build, because the first draft, read only from
source, repeated the guide's server-ownership error.

### 2026-09-29 — GE0 split into three stacked, CI-enforced sub-slices

Review direction: the GE0 corrections must not be one-off edits. Each class
of fact becomes a test that fails CI when the guide stops matching the build.
Each sub-slice is a stacked PR in its own worktree. This moves work from two
later slices into GE0:

- The `design/protocol.md` version fix (from GE6) goes to GE0.1.
- The command-registry export (from GE5) goes to GE0.3.

Filed ENG-979 (GE0), ENG-980 (GE10) and ENG-981 (GE9) under ENG-928.

### 2026-09-29 — GE0.1 in review

Two new docs-truth tests in `src/docs_truth.rs` fail CI on version drift:

- `every_product_version_in_the_guide_is_this_release`: every
  `MAJOR.MINOR.PATCH` in `docs/guide/` and `README.md` equals
  `CARGO_PKG_VERSION`, unless it is listed in `FOREIGN_VERSIONS` with a
  reason. There are two entries: the Codex client `0.156.1` and the example
  pack's `0.1.0`.
- `every_compatibility_number_in_the_guide_is_this_builds`: every
  `protocol N`, `capabilities N`, `descriptor N`, and `store schema N` in
  the guide equals this build's. `design/protocol.md` must read
  `PROTOCOL_VERSION = <current>` and have a `Version <current> …`
  paragraph.

Both failed first (red) on nine stale references:
- `0.1.2` in `cli.md`.
- `0.1.3` in seven places in `install.md` and in the `troubleshooting.md`
  doctor sample.
- protocol 27, descriptor 9 and store schema 34 in that doctor sample.

`cargo xtask release X.Y.Z` now rewrites whole old-version tokens in
`docs/guide/*.md` and `README.md` and commits them with the bump, so a
release cannot make the guide fail. I checked this with a real
`cargo xtask release 0.1.5 --no-commit` in a throwaway worktree: it
rewrote 9 references, left `0.156.1` and `0.1.0` alone, and docs-truth
passed on the bumped tree.

`design/protocol.md` now reads 30, with Version 29 (`max`, store 37) and
Version 30 (`default`, store 38 envelope, store 39) paragraphs from
`fec9e78` and `5bed55d`.

Prose corrections, each checked on a build of this branch with an isolated
home:
- `tui.md`: the intro and the new §Exiting say an owning TUI's quit cancels
  runs, and that runs survive only under `qq serve`.
- `headless.md` §`qq serve`: the same, plus `qq run`/`qq ask` never use a
  server, and exit `4` when the store is owned (exit-code table row too).
- `troubleshooting.md`: the "already running" entry no longer says `qq run`
  connects. The `StoreBusy` heading now quotes the real message,
  `session store is owned by another running qq process`.

Build checks on this branch:
- `qq serve` with `qq run` gives exit `4` and that message.
- `SessionRuntime::shutdown` calls `settlement_cancel` for every unfinished
  run (`crates/qq-core/src/sessions/runtime.rs`), covered by
  `shutdown_cancels_running_and_queued_prompts_before_returning`.

Verification: `cargo fmt --check`, `cargo clippy --workspace --all-targets
--all-features -D warnings`, `cargo test --workspace` (0 failures), the
exact-test guard, and `nub run build` (479 links resolve).
