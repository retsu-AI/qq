# Ledger — Guide expansion

Plan: [`../guide-expansion.md`](../guide-expansion.md). Only the agent
working this plan edits this file. Current state on top; dated entries
appended below, newest last.

| Slice | Page | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| GE1 | `agents.md` | Planned | | ENG-929; after GE4/GE5 |
| GE2 | `sessions.md` | Planned | | ENG-930; after GE4/GE5; ADR-0038 is Proposed — label it |
| GE3 | `skills.md` | Planned | | ENG-931; after GE4/GE5 |
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
