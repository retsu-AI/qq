# Ledger — Guide expansion

Plan: [`../guide-expansion.md`](../guide-expansion.md). Only the agent
working this plan edits this file. Current state on top; dated entries
appended below, newest last.

| Slice | Page | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| GE0.1 | versions checked; prose corrections | In review | `fix/eng-979-ge0-1-versions` | ENG-979; stacked on #186 |
| GE0.2 | samples run in CI | In review | `fix/eng-979-ge0-2-samples` | ENG-979; stacked on GE0.1 |
| GE0.3 | reference tables mirror the code | In review | `fix/eng-979-ge0-3-tables` | ENG-979; stacked on GE0.2 |
| GE10 | `concepts.md` | In review | `docs/eng-980-ge10-concepts` | ENG-980; stacked on GE0.3 |
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

### 2026-09-29 — GE0.1 in review (#222)

- Tests (`src/docs_truth.rs`):
  - `every_product_version_in_the_guide_is_this_release`: guide and README
    versions must equal `CARGO_PKG_VERSION`; a foreign version sits on a
    `<!-- not-qq-version -->` line.
  - `every_compatibility_number_in_the_guide_is_this_builds`: protocol,
    capabilities, descriptor, and store schema must match this build, and
    `design/protocol.md` must name the current version.
- `xtask`: `cargo xtask release` rewrites tracked, regular guide/README
  files only, skips marked lines, and commits them with the bump. Checked
  with `release 0.1.5 --no-commit`. 4 tests added.
- Red on 9 stale versions and doctor numbers; `design/protocol.md` was at 28
  with no 29/30 entries.
- Prose checked on a build: a quitting owner cancels its runs; a second TUI
  attaches to the first; `qq run` exits `4` when any other qq process
  (including another `qq run`) owns the store.
- Workspace fmt, clippy and tests pass; the site builds.

### 2026-09-29 — GE0.2 in review (#223)

- Tests (`src/docs_truth.rs`):
  - `every_ron_sample_in_the_guide_loads`: every ```ron block loads through
    the real loader as the file its fence names; its model routes must be
    in the catalog.
  - `every_qq_command_in_the_guide_parses`: every `qq …` sample goes
    through the real `Cli`.
  - `output_samples_in_the_guide_match_the_code`: the doctor sample, resume
    hint, and exit-code table match the code.
- Red on:
  - the policy sample, which mixed in managed-only keys (now split into a
    separate `managed.ron` sample);
  - pack and Mantle samples that were not valid RON;
  - a truncated MCP `pin`;
  - a missing doctor `mcp` row;
  - ULID `01J…` session ids (real ids are 32 hex).
- Each test also caught a planted defect of its kind: flag, subcommand,
  route, MCP key, pack enum, chord, doctor row.
- Workspace fmt, clippy and tests pass; the site builds.

### 2026-09-29 — GE0.3 in review (#224)

- Tests:
  - `qq-config`: `MANAGED_ONLY_POLICY_FIELD_NAMES` must equal the set a
    user layer is refused, checked key by key.
  - `src/docs_truth.rs`: the policy, precedence and TUI command tables and
    the troubleshooting headings are checked against the code.
- `qq-tui`: exports `command_rows()`. The `tui.ron` defaults move to
  `settings::default_chords`, so the builder and the registry share one
  source.
- Red on:
  - 6 policy keys marked "any";
  - a precedence table missing 2 `QQ_*` variables and the Windows MDM
    source;
  - `Alt-Up` documented as focusing the parent;
  - a grouped command table the test could not check.
- Planted defects caught: a key marked "any", a dropped override, a changed
  chord, a reworded message.
- Workspace fmt, clippy and tests pass; the site builds.

### 2026-09-30 — GE0 rebased onto v0.1.5; GE10 in review

- v0.1.5 shipped with a new rule (#228, runbook step 6): the bump PR keeps
  the guide's pins, and a follow-up PR moves them once the assets exist.
  GE0.1's exact-version check and bump-time rewrite contradicted it.
  - The check now requires one QQ version across the guide and README:
    the manifest's, or the previous `CHANGELOG.md` release.
  - `cargo xtask release --docs` moves the pins for step 6; the bump no
    longer rewrites docs. Tried on a scratch 0.1.6 bump.
- GE10 `concepts.md`:
  - The nesting diagram, a "what is saved where" table (including what is
    not: queued drafts, a held call's wait), and 20 glossary terms, each
    linked to its home page.
  - Checked in code: held calls become `interrupted` on restart; queued
    drafts live only in the TUI; delegation depth is 1 by default, at most 3.
  - `glossary_defines_each_term_with_the_builds_names_and_bounds`: every
    term is a `###` heading; approval modes and roles are checked against
    the enums; the TUI default and 3 bounds are checked against constants.
- Found, not fixed (not guide pages): `design/protocol.md:746` and
  `design/architecture.md:761` still say sub-agent depth is "fixed at 1".
- Workspace fmt, clippy and tests pass; the site builds (549 links).
