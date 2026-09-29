# Ledger — Guide expansion

Plan: [`../guide-expansion.md`](../guide-expansion.md). Only the agent
working this plan edits this file. Current state on top; dated entries
appended below, newest last.

| Slice | Page | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| GE0 | corrections to the shipped guide | Planned | | not yet filed; first — the site states five false things today |
| GE10 | `concepts.md` | Planned | | not yet filed |
| GE1 | `agents.md` | Planned | | ENG-929; after GE0, GE10, GE4, GE5 |
| GE2 | `sessions.md` | Planned | | ENG-930; after GE0; ADR-0038 is Proposed — label it |
| GE3 | `skills.md` | Planned | | ENG-931; after GE10, GE4, GE5 |
| GE9 | `workflows.md` | Planned | | not yet filed; after GE1–GE3 |
| GE4 | `environment.md` | Planned | | ENG-932; `configuration.md` hands off |
| GE5 | `keybindings.md` | Planned | | ENG-933; `tui.md` hands off |
| GE6 | `server.md` | Planned | | ENG-934; fixes `design/protocol.md` drift |
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
