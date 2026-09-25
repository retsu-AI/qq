# Guide expansion: the pages the site dropped

**Status:** Proposed 2026-09-25. Ledger:
[`progress/guide-expansion.md`](progress/guide-expansion.md). Predecessor:
onboarding UX (closed; receipt in
[`progress/onboarding-ux.md`](progress/onboarding-ux.md)).
**Linear:** [ENG-928](https://linear.app/retsu-ai/issue/ENG-928) (parent);
GE1 ENG-929, GE2 ENG-930, GE3 ENG-931, GE4 ENG-932, GE5 ENG-933, GE6
ENG-934, GE7 ENG-935, GE8 ENG-936.

## Goal

The docs site ([retsu-ai.github.io/qq](https://retsu-ai.github.io/qq/)) is
generated from `docs/guide/`, which has eleven pages. The site's first
design had nineteen; eight were dropped rather than shipped as stubs. This
plan writes those eight as real pages, each answering the questions a user
brings to it, each true to the code today, and each guarded by the
docs-truth test so it cannot drift.

Acceptance for the plan as a whole:

1. Every page lists the user questions it answers (from the research below)
   and answers each with the behavior of the shipped binary. Nothing
   documented is planned-but-unshipped unless labeled so in the sentence
   that names it (ADR-0038 archive, ADR-0015 enrollment, ADR-0016 TLS).
2. Every new page has a `website/sidebar.json` row; `nub run build` passes
   (sync, `astro check`, link check).
3. The docs-truth test (`src/docs_truth.rs`) covers the code surface each
   page introduces: keybinding chords, `tui.ron` binding names,
   profile/delegation/audit/pack keys, MDM identifiers, HTTP routes.
4. No page duplicates another: `configuration.md` keeps the RON shapes and
   key tables; `tui.md` keeps its command tables; the new pages explain,
   link, and add what was missing.
5. Two design-doc drifts found in research are fixed in the same PR as the
   page that touches them: `design/protocol.md` says `PROTOCOL_VERSION` 28
   (code: 30) and omits `POST /v1/sessions/effort`.

## Rules for these pages

- Task-oriented. Start from what the reader wants to do; end with the
  reference table or a link to it.
- One H1 (the page title); `##` sections become the on-page outline and
  the fragments other pages link to. Renaming a heading another page links
  to fails the site build — check `check-links` output.
- Code spans for every key, command, flag, variable, file name, and glyph,
  so docs-truth can see them.
- Amend the page in the same PR as any later behavior change (the
  workflow's design-doc rule applies to the guide).

## Slices

Independent unless noted. Each is one PR: the page, its sidebar row, the
docs-truth extension, and any design-doc fix it uncovers.

| ID | Page | Answers | Sources | Docs-truth extension | Size |
| --- | --- | --- | --- | --- | --- |
| GE1 | `agents.md` — Agents, profiles, and packs | profile vs pack vs roster and when each; how a child is spawned, which model (`role` / `default_role` / explicit `model`), how deep (`max_depth` ≤ 3, `0` disables `spawn_agent`); can a child write (`write_children`, `authority`, `supervised` mode, one write child at a time); watching children (`Alt-C`, `/agents`, sidebar tree, `Enter` on a spawn row); writing a pack (layout, `pack.ron`, persona, roots, tool filter, MCP subset, install location); what `audit` reviews | `configuration.md` §profiles §packs §delegation §audit (link, do not re-list); `design/architecture.md` §Agent Profiles, §Agent Packs; `qq-config/src/lib.rs` limits, `pack.rs`, `qq-core/src/tools/specs.rs` `spawn_agent` | publish `PROFILE_FIELD_NAMES`, `DELEGATION_FIELD_NAMES`, `AUDIT_FIELD_NAMES`, `PACK_FIELD_NAMES` in `qq-config` (same `derived_field_names` proof as `DOCUMENT_FIELD_NAMES`); assert `spawn_agent` argument names | M |
| GE2 | `sessions.md` — Sessions | what persists and survives restart (runs continue on the background server); find / name / resume / delete / prune (`/sessions`, `Ctrl-D`, `/prune`, `qq --session`); two terminals or TUI + `qq run` on one server; two machines (single store owner, exit 4); limits (`MAX_SESSIONS_PER_WORKSPACE` 512, 100 000 receipts) and that nothing is auto-deleted today (ADR-0038 archive is Proposed — say so); compaction vs what you see; where the database is, back up, move (`XDG_DATA_HOME`), start over | `tui.md` §Sessions (keep its table there; link); `design/headless-contract.md` §State Location; ADR-0002, 0003, 0022, 0038, 0039; `qq-core/src/sessions.rs` limits | none new (slash names and `--session` already covered); the truth risk is docs→code, which the test cannot catch — review by hand against ADR-0038 | M |
| GE3 | `skills.md` — Skills, workspace commands, and `@` mentions | command vs skill (you invoke commands; the model may load disclosed skills via `load_skill`); writing one (Markdown body, `description:` front matter, 64 KiB, `/skills` to check discovery); `.claude/` and `.agents/` compatibility roots (invoke-only, never disclosed); reserved names, ambiguity, the `//` escape; everything `@` attaches — `@path`, `@path:10-40`, `@dir/`, globs, `@diff`, `@sha:REF`, `@web:URL`, `@skill:name` — and the caps (8 files, 256 KiB each, 16 mentions); structuring `AGENTS.md` and nested instruction files | `qq-core/src/workspace/skills.rs` (roots table, limits), `design/tools.md` §Agent Instructions, §Explicit Commands And Skills, §File References In Prompts; `qq-protocol/src/mentions.rs`, `input.rs` | export `LOAD_SKILL_TOOL` or a built-in tool-name list and assert; a `MENTION_SPECIAL_KINDS` const (`web`, `diff`, `sha`, `skill`) in `qq-protocol` and assert each | S/M |
| GE4 | `environment.md` — Environment variables | every variable, grouped: configuration overrides (8), credentials (provider keys, AWS family, `TYPESAFE_API_KEY`), terminal (`VISUAL` then `EDITOR`, `COLORTERM`), installer (`QQ_VERSION`, `QQ_INSTALL_DIR`), paths (`XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `APPDATA`, `ProgramData`), the shell child's base env (`PATH HOME LANG TERM TMPDIR`) and how to pass more (`shell_env`, MCP `env`); precedence (`QQ_MODEL` vs `--model` vs project vs profile); running with no files in CI (`QQ_CONFIG_CONTENT`) | `configuration.md` §Environment variables (becomes a short table linking here); `qq-config` `ENVIRONMENT_VARIABLES`, `provider_credential_variables()`; `src/runtime.rs` AWS chain; `qq-tui/src/terminal.rs` editor; `qq-core/src/runtime/shell_policy.rs` `BASE_ENV` | add `VISUAL`, `XDG_*`, `APPDATA`, `ProgramData` to the asserted set; `QQ_EVAL_ARM` to the allow-list with its reason | S |
| GE5 | `keybindings.md` — Keybindings | one printable table per context: compose, running, approval (`y a w n`, `Y N` amend), question (`1`–`9`), trust (`t s q`), pickers (`Esc Enter Up Down`, `Ctrl-N` in `/models`, `Ctrl-D`/`Delete` then `y`/`n` in `/sessions`), transcript (`Ctrl-Up/Down`, `Enter`, `Ctrl-O`, `Alt-R`); what `Esc` does right now (its precedence order); composer editing keys (`Ctrl-A/E/W/K/U/Y/Z`, `Ctrl-Left/Right`, `Alt-Backspace`, `Home/End`, `Ctrl-J`, `Up/Down` history); rebinding (`tui.ron` `bindings`, the five actions, chord syntax, `[]` to disable); terminal caveats (`Shift-Enter`, `Ctrl-\`, `Alt-*` under tmux) | `qq-tui/src/commands.rs` `COMMANDS` chords, `settings.rs` defaults, `app.rs` key handlers; `tui.md` §Every command (keep; link); `configuration.md` §`tui.ron` | expose `qq_tui::default_chords()` beside `slash_names()` and assert each chord; publish `TUI_BINDING_FIELD_NAMES` in `qq-config` and assert; a hand-listed const for the hard-coded keys | S |
| GE6 | `server.md` — Server and protocol | do I need `qq serve` (no for the TUI; yes for shared, long-lived, remote); bind, expose (loopback only today; tunnel), browsers (`--allow-origin`); authentication (where `server.ron` and the bearer token live, scripting against the API, several clients); routes and the event stream (`Last-Event-ID`, cursor replay, `InvalidCursor` → resnapshot), limits (64/64 → 503); what `qq version`'s protocol / capabilities / descriptor / store-schema numbers mean when mixing binaries; enrollment and TLS status (ADR-0015, ADR-0016: Proposed) | `headless.md` §`qq serve` (keep the three invocations; link); `design/protocol.md`, `design/headless-contract.md` §`qq serve`; `qq-server/src/lib.rs` limits, router, discovery; `qq-protocol` `COMMAND_ROUTES`, `PROTOCOL_VERSION` | assert every route in `qq_protocol::COMMAND_ROUTES` plus the fixed routes appears as a code span; tell readers to run `qq version` rather than hard-coding numbers. **Fix `design/protocol.md`**: version 28 → 30, add `/v1/sessions/effort` | M/L |
| GE7 | `enterprise.md` — Managed and organization configuration | enforcing settings users cannot override (managed dir per OS, ownership and mode rules, `deny_tools` / `deny_shell_prefixes` / `deny_hosts`, applied after `QQ_*` overrides); MDM / GPO (`dev.qq` forced preference `ManagedConfig` on macOS; `HKLM\Software\Policies\dev.qq\ManagedConfig` on Windows; none on Linux) and what the value must contain; publishing a team manifest (HTTPS, must set `organization` to its own name, no secrets, size cap) and enrolling (`qq org enroll/use/refresh/remove`, `--organization`, `QQ_ORGANIZATION`); when the manifest is fetched (enroll and `refresh` only; cache; failed refresh keeps last good); what a manifest cannot do (secrets, non-HTTPS, bypass project trust); debugging with `qq config explain` / `paths` | `configuration.md` precedence rows 3, 9, 10 and the managed-only deny keys; `cli.md` §`qq org`; `qq-config/src/loader.rs` managed order and `validate_managed_metadata`, `managed.rs`, `remote.rs` | hoist the MDM identifiers to `pub const` and assert; assert `managed.ron`, `managed.d`, `organizations.ron` names | S/M |
| GE8 | Changelog on the site | what changed between versions, is it breaking, which PR; which version am I on | `CHANGELOG.md` (written by `cargo xtask release` since #160; **does not exist on `main` until the next release**); `website/scripts/sync-docs.mjs` | `sync-docs` writes `changelog.md` from the repo-root file when present, with a `sidebar.json` row and `editUrl` pointing at `runbooks/release.md`; skips with a log line when absent so `main` keeps building before the first release; update `runbooks/website.md` "Add a page" for the special case | S |

Dependencies: GE8 lands with or after the first release that writes
`CHANGELOG.md` (v0.1.5). GE4 and GE5 change how `configuration.md` and
`tui.md` hand off to the new pages; land them before GE1/GE2/GE3 so those
can link to the references. Otherwise any order; two writers may run in
parallel if they claim different pages and merge the ledger in stack order.

## Sidebar placement

`website/sidebar.json` groups: `agents`, `sessions`, `skills` → **Using
QQ**; `environment`, `keybindings`, `server`, `enterprise` → **Reference**;
`changelog` → **Help**. Labels: "Agents, profiles, and packs", "Sessions",
"Skills and commands", "Environment variables", "Keybindings", "Server and
protocol", "Managed and organization config", "Changelog".

## Out of scope

- New behavior. If a page reveals a gap (a key that should exist, a limit
  that should be configurable), file it against the owning plan and
  document what ships today.
- Translating or restructuring the existing eleven pages beyond the
  hand-offs named in GE4 and GE5.
- Screenshots or recorded terminals; the landing page's TUI mocks are
  drawn from the goldens and are enough.

## Decisions carried

| Decision | Where recorded |
| --- | --- |
| The guide is the single source; the site is generated from it | onboarding OB12, `runbooks/website.md` |
| Docs-truth is code→docs only; docs→code truth is a review duty | onboarding OB10 receipt |
| Proposed ADRs are named as proposed, never described as behavior | this plan, acceptance 1 |
