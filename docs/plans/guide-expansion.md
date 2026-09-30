# Guide expansion: correct the guide, then fill its gaps

**Status:** Proposed 2026-09-25; revised 2026-09-29. Ledger:
[`progress/guide-expansion.md`](progress/guide-expansion.md). Predecessor:
onboarding UX (closed; receipt in
[`progress/onboarding-ux.md`](progress/onboarding-ux.md)).
**Linear:** [ENG-928](https://linear.app/retsu-ai/issue/ENG-928) (parent);
GE0 ENG-979, GE1 ENG-929, GE2 ENG-930, GE3 ENG-931, GE4 ENG-932, GE5
ENG-933, GE6 ENG-934, GE7 ENG-935, GE8 ENG-936, GE9 ENG-981, GE10 ENG-980.

## Goal

A new user should get from install to useful work without reading code or
asking anyone, and an experienced user should find every key, variable,
limit and route in one obvious place. The docs site
([retsu-ai.github.io/qq](https://retsu-ai.github.io/qq/)) is generated from
`docs/guide/`, which has eleven pages. The site's first design had nineteen;
eight were dropped rather than shipped as stubs. This plan:

1. corrects what the eleven pages say wrongly today (GE0);
2. adds the two pages a newcomer reaches for first and cannot find: what
   the words mean (GE10) and how to do common jobs (GE9);
3. writes the eight dropped pages (GE1–GE8), each answering the questions a
   user brings to it, each true to the code, and each guarded by the
   docs-truth test so it cannot drift.

Acceptance for the plan as a whole:

1. Every page lists the user questions it answers (from the research below)
   and answers each with the behavior of the shipped binary. Nothing
   documented is planned-but-unshipped unless labeled so in the sentence
   that names it (ADR-0038 archive: Proposed; ADR-0015 enrollment:
   Proposed; ADR-0016 TLS: reserved, not written).
2. Every behavioral claim on a new or corrected page was checked on a build
   of `main` (the command run, the key pressed), not only read from source.
   Docs-truth proves code→docs; this is the docs→code half it cannot catch,
   and GE0 exists because reading source alone got it wrong.
3. Every new page has a `website/sidebar.json` row; `nub run build` passes
   (sync, `astro check`, link check).
4. The docs-truth test (`src/docs_truth.rs`) covers the code surface each
   page introduces: keybinding chords, `tui.ron` binding names,
   profile/delegation/audit/pack keys, MDM identifiers, HTTP routes.
5. No page duplicates another: `configuration.md` keeps the RON shapes and
   key tables; `tui.md` keeps its command tables; the new pages explain,
   link, and add what was missing.
6. `design/protocol.md` matches the code. GE0.1 fixes the version: it says
   `PROTOCOL_VERSION` 28 (code: 30) and has no changelog paragraphs for 29
   and 30. GE6 fixes the route list, which omits `POST /v1/sessions/effort`.
7. The guide cannot drift from the build: GE0 turns every version, sample,
   and reference table into a CI failure when it stops matching the code
   (see GE0 below). Later slices extend the same checks to their pages.

## Rules for these pages

- Task-oriented. Start from what the reader wants to do; end with the
  reference table or a link to it.
- One H1 (the page title); `##` sections become the on-page outline and
  the fragments other pages link to. Renaming a heading another page links
  to fails the site build — check `check-links` output.
- Code spans for every key, command, flag, variable, file name, and glyph,
  so docs-truth can see them.
- Define a term once, in the GE10 glossary, and link to it; do not
  re-explain sessions, runs or grants on every page.
- Amend the page in the same PR as any later behavior change (the
  workflow's design-doc rule applies to the guide).

## Slices

Independent unless noted. Each is one PR: the page, its sidebar row, the
docs-truth extension, and any design-doc fix it uncovers. Each PR is stacked
on the previous one in its own worktree, and merges in stack order.

GE0 is three stacked PRs. Each adds a docs→code check to `cargo test` (so
CI fails when the guide stops matching the build) and fixes what that check
finds:

| Sub-slice | Check that fails CI | Fixes it forces |
| --- | --- | --- |
| GE0.1 versions | every product version in the guide and `README.md` equals the workspace version (`cargo xtask release` rewrites them); every `protocol` / `capabilities` / `descriptor` / `store schema` number equals the build's; `design/protocol.md` names the current `PROTOCOL_VERSION` and has a paragraph for it | the stale `0.1.2`/`0.1.3`/protocol 27 samples; `design/protocol.md` 28 and the missing 29/30 paragraphs; the server-ownership prose (not mechanically checkable; verified on a build) |
| GE0.2 samples run | every `qq …` sample parses with the real `Cli`; every RON sample loads through the real loader as the file its fence title names (`config.ron`, `managed.ron`, `pack.ron`, `tui.ron`); every model route they name is in the catalog; the doctor sample lists `CHECK_NAMES` in order; the resume-hint sample equals `resume_hint`; the exit-code table equals `HeadlessStatus` | ULID-style `01J…` ids (ids are 32 hex digits); the truncated MCP `pin`; the pack sample's non-RON `mcp`; the policy sample's managed-only keys; the doctor sample's missing `mcp` row |
| GE0.3 tables mirror code | the policy table marks exactly the managed-only keys `managed layers only`; the precedence table names every `QQ_*` override; `tui.md` "Every command" has one row per registry command with exactly its default keys and slashes; every troubleshooting heading quotes a message template the code has | `Alt-Up` as focus-parent; six policy keys marked "any"; rows 8 and 10 of the precedence table |

| ID | Page | Answers | Sources | Docs-truth extension | Size |
| --- | --- | --- | --- | --- | --- |
| GE0 | Corrections to the shipped guide, made CI-checked (GE0.1–GE0.3 above) | nothing new; makes these true: **who owns the server** — the TUI embeds it when none is running, and quitting that TUI durably cancels its queued and running runs; runs outlive the TUI only under a separate `qq serve` (`tui.md` §Exiting, `headless.md` §`qq serve`); **`qq run` never connects to a server** — it opens the store itself and exits `4` (`StoreBusy`) while a TUI or `qq serve` owns it (`troubleshooting.md` §`qq server already running`, §`StoreBusy`); **`Alt-Up` only pulls back a queued draft** — `Esc` walks to the parent (`tui.md` session table and footnote); **managed-only policy keys** — `allowed_providers`, `denied_providers`, `max_output_tokens`, `require_https`, `allow_custom_providers`, `allow_literal_secrets` are managed/MDM-only like the three `deny_*` keys (`configuration.md` policy table says "any"); **precedence** row 8 lacks `QQ_JEV_APPROVAL` and `QQ_APPROVAL_DELEGATE`, row 10 lacks the Windows registry policy | `src/main.rs` `interactive()` (`server::reserve`, `embedded.shutdown()`), `qq-core/src/sessions/runtime.rs` `shutdown`; `src/main.rs` `run` (`RuntimeHandler::open_with`), `design/headless-contract.md` §State Location; `qq-tui/src/commands.rs` `DequeueDraft`/`FocusParent`, `app.rs` `Esc`; `qq-config/src/document.rs` `has_managed_only_fields`; `qq-config/src/lib.rs` `ENVIRONMENT_VARIABLES`, `managed.rs` | the GE0.1–GE0.3 checks above | M |
| GE10 | `concepts.md` — Concepts and glossary | what a workspace, session, child session, run, turn, steer/queue, compaction, profile, pack, roster, role, grant, approval mode, held call, delegate, reviewer, trust, and Jev are — one short definition each, how they nest (workspace → session → run → turn → tool call), and the page that goes deeper; which of these persist and where | existing guide pages (link each term to its home); `design/architecture.md`; `design/tools.md`; `runbooks/decision-models.md` (the site has no Jev explanation) | assert each glossary term is an `##`/`###` anchor other pages can link; none new in code | S |
