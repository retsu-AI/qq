# Guide expansion: correct the guide, then fill its gaps

**Status:** Proposed 2026-09-25; revised 2026-09-29. Ledger:
[`progress/guide-expansion.md`](progress/guide-expansion.md). Predecessor:
onboarding UX (closed; receipt in
[`progress/onboarding-ux.md`](progress/onboarding-ux.md)).
**Linear:** [ENG-928](https://linear.app/retsu-ai/issue/ENG-928) (parent);
GE1 ENG-929, GE2 ENG-930, GE3 ENG-931, GE4 ENG-932, GE5 ENG-933, GE6
ENG-934, GE7 ENG-935, GE8 ENG-936; GE0, GE9 and GE10 are filed when the
plan is accepted.

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
6. `design/protocol.md` matches the code in the same PR as GE6: it says
   `PROTOCOL_VERSION` 28 (code: 30), has no changelog paragraphs for 29 and
   30, and omits `POST /v1/sessions/effort`.

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
docs-truth extension, and any design-doc fix it uncovers.

| ID | Page | Answers | Sources | Docs-truth extension | Size |
| --- | --- | --- | --- | --- | --- |
| GE0 | Corrections to the shipped guide | nothing new; makes these true: **who owns the server** — the TUI embeds it when none is running, and quitting that TUI durably cancels its queued and running runs; runs outlive the TUI only under a separate `qq serve` (`tui.md` §Exiting, `headless.md` §`qq serve`); **`qq run` never connects to a server** — it opens the store itself and exits `4` (`StoreBusy`) while a TUI or `qq serve` owns it (`troubleshooting.md` §`qq server already running`, §`StoreBusy`); **`Alt-Up` only pulls back a queued draft** — `Esc` walks to the parent (`tui.md` session table and footnote); **managed-only policy keys** — `allowed_providers`, `denied_providers`, `max_output_tokens`, `require_https`, `allow_custom_providers`, `allow_literal_secrets` are managed/MDM-only like the three `deny_*` keys (`configuration.md` policy table says "any"); **precedence** row 8 lacks `QQ_JEV_APPROVAL` and `QQ_APPROVAL_DELEGATE`, row 10 lacks the Windows registry policy | `src/main.rs` `interactive()` (`server::reserve`, `embedded.shutdown()`), `qq-core/src/sessions/runtime.rs` `shutdown`; `src/main.rs` `run` (`RuntimeHandler::open_with`), `design/headless-contract.md` §State Location; `qq-tui/src/commands.rs` `DequeueDraft`/`FocusParent`, `app.rs` `Esc`; `qq-config/src/document.rs` `has_managed_only_fields`; `qq-config/src/lib.rs` `ENVIRONMENT_VARIABLES`, `managed.rs` | publish the managed-only policy names from `qq-config` and assert the table marks exactly those; assert every `ENVIRONMENT_VARIABLES` entry appears in the precedence table | S |
| GE10 | `concepts.md` — Concepts and glossary | what a workspace, session, child session, run, turn, steer/queue, compaction, profile, pack, roster, role, grant, approval mode, held call, delegate, reviewer, trust, and Jev are — one short definition each, how they nest (workspace → session → run → turn → tool call), and the page that goes deeper; which of these persist and where | existing guide pages (link each term to its home); `design/architecture.md`; `design/tools.md`; `runbooks/jev.md` (the site has no Jev explanation) | assert each glossary term is an `##`/`###` anchor other pages can link; none new in code | S |
| GE1 | `agents.md` — Agents, profiles, and packs | profile vs pack vs roster and when each; how a child is spawned, which model (`role` / `default_role` / explicit `model`) and effort (roster `effort`, RR8.4); how deep (`max_depth` ≤ 3, default 1, `0` disables `spawn_agent`) and how many (3 concurrent, 8 per run); can a child write (`write_children`, `authority: write`, needs `reviewer_model`, only from a top-level run, runs `supervised`, one write child at a time); watching children (`/agents`, sidebar tree, `Enter` on a spawn row) vs making one by hand (`Alt-C`); writing a pack (layout, `pack.ron`, persona, roots, tool filter, MCP subset, install location); what `audit` reviews | `configuration.md` §profiles §packs §delegation §audit (link, do not re-list); `design/architecture.md` §Agent Profiles, §Agent Packs; `qq-config/src/lib.rs` `MAX_DELEGATION_DEPTH`, `pack.rs`; `qq-core/src/sessions.rs` child limits, `sessions/subagents.rs`; `qq-core/src/tools/specs.rs` `SpawnAgentArgs` | publish `PROFILE_FIELD_NAMES`, `DELEGATION_FIELD_NAMES`, `AUDIT_FIELD_NAMES`, `PACK_FIELD_NAMES` in `qq-config` (same `derived_field_names` proof as `DOCUMENT_FIELD_NAMES`; `ProfilePatch` is an enum variant, so check the probe reaches it); assert `spawn_agent` argument names | M |
| GE2 | `sessions.md` — Sessions | what persists (every session, forever, in one SQLite store) and what does not (runs die with the process that owns the server; see GE0); keeping runs alive (`qq serve`, then attach any number of TUIs); find / name / resume / delete / prune (`/sessions`, `Ctrl-D`, `/prune`, `qq --session`); `qq run` against a session (only while no TUI or server owns the store; exit `4` otherwise); limits (512 sessions per workspace, children included; 100 000 work receipts) and what is trimmed (tool-output and attachment spill past 64 MiB per session; 3 compaction records) — nothing else is deleted, and the ADR-0038 archive is Proposed; compaction vs what you see; where the database is, backup, moving it (`XDG_DATA_HOME`, Linux only), starting over; one store per machine on a local disk (ADR-0022) | `tui.md` §Sessions (keep its table there; link); `design/headless-contract.md` §State Location; ADR-0002, 0003, 0022, 0038, 0039; `qq-core/src/sessions.rs` limits | none new (slash names and `--session` already covered); the truth risk is docs→code — acceptance 2 | M |
| GE3 | `skills.md` — Skills, workspace commands, and `@` mentions | command vs skill (you invoke commands; the model may load disclosed skills via `load_skill`, offered only when one exists); writing one (Markdown body ≤ 64 KiB, `description:` front matter ≤ 512 bytes, 64 per index, `/skills` to check discovery); names (`[a-z0-9_-]`, lowercase first, ≤ 64 bytes, not a built-in slash name); `.agents/skills`, `.claude/commands`, `.claude/skills` compatibility roots (invoke-only, never disclosed); ambiguity and the `//` escape; everything `@` attaches — `@path`, `@path:10-40`, `@dir/`, globs, `@diff`, `@diff:REF`, `@sha:REF`, `@web:URL`, `@skill:name`, `@@` for a literal `@` — and the caps (16 mentions, 8 files, 256 KiB each, 1 MiB total); `AGENTS.md` (or `CLAUDE.md`) at the root is injected, ≤ 64 KiB — nested ones are read by the agent through tools, not injected; `.qqignore` and what the agent's search skips | `qq-core/src/workspace/skills.rs`, `guidance.rs`, `instructions.rs`; `design/tools.md` §Agent Instructions, §Explicit Commands And Skills, §File References In Prompts; `qq-protocol/src/mentions.rs`, `input.rs`; `qq-core/src/mentions.rs` | a `MENTION_SPECIAL_KINDS` const (`web`, `diff`, `sha`, `skill`) in `qq-protocol`, asserted; assert `load_skill` and `.qqignore` appear | S/M |
| GE9 | `workflows.md` — Common workflows | step-by-step recipes, each ending in the exact commands: fix a failing test; review a branch or PR (`@diff`, read-only mode); refactor across files with approvals you trust (`w`, grants); delegate to sub-agents; resume yesterday's work; run the same prompt in CI (`qq run`, exit codes, `QQ_CONFIG_CONTENT`); switch or compare models mid-session | the other guide pages (each recipe links, does not re-explain); `headless.md` §In CI | none new; every command in a recipe is already asserted where it is defined | M |
| GE4 | `environment.md` — Environment variables | every variable, grouped: configuration overrides (the 8 in `ENVIRONMENT_VARIABLES`), credentials (provider keys, `TYPESAFE_API_KEY`, AWS family incl. `AWS_CONFIG_FILE` / `AWS_SHARED_CREDENTIALS_FILE`), terminal (`VISUAL` then `EDITOR`, `COLORTERM`), installer (`QQ_VERSION`, `QQ_INSTALL_DIR`), paths (`XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_RUNTIME_DIR` — Linux only; `ProgramData`; `%APPDATA%` names the Windows location but setting it does not move it), network (proxy variables — confirm which clients honor them before writing), the shell child's base env (`PATH HOME LANG TERM TMPDIR`) vs an MCP child's (`PATH HOME`), and how to pass more (`shell_env`, MCP `env`); precedence (`--model` replaces `QQ_MODEL`; then project, profile); running with no files in CI (`QQ_CONFIG_CONTENT`); `QQ_EVAL_ARM` as eval-only | `configuration.md` §Environment variables (becomes a short table linking here); `qq-config` `ENVIRONMENT_VARIABLES`, `provider_credential_variables()`; `src/runtime.rs` AWS chain, `TYPESAFE_API_KEY`; `qq-tui/src/terminal.rs` editor; `qq-core/src/runtime/shell_policy.rs` `BASE_ENV`; `qq-mcp` child env; `qq-server` runtime dir | add `TYPESAFE_API_KEY`, `VISUAL`, `XDG_*`, `ProgramData`, `BASE_ENV` to the asserted set | S |
| GE5 | `keybindings.md` — Keybindings | one printable table per context: compose, running, approval (`y a w n`, `Y N` amend, `Esc` denies), question (`1`–`9` on an empty composer, `Enter` free text), trust (`t s q`, `Esc`), pickers (type to filter, `Esc Enter Up Down`, `Ctrl-N` in `/models`, `Ctrl-D`/`Delete` then `y`/`n` in `/sessions`), transcript (`Ctrl-Up/Down`, `Ctrl-Home/End`, `PageUp/PageDown`, `Enter`, `Ctrl-O`, `Alt-R`); what `Esc` does right now (clear the transcript cursor → leave a workspace view → dismiss a notice → `Esc Esc` cancels the run → focus the parent); composer editing keys (`Ctrl-A/E/W/U/Y/Z`, `Ctrl-_`, `Ctrl-Left/Right`, `Alt-Backspace`, `Ctrl-Backspace`, `Home/End`, `Ctrl-J`, `Up/Down` history — `Ctrl-K` is the command palette, not kill-to-end); rebinding (`tui.ron` `bindings`: `toggle_navigator`, `create_root_session`, `create_child_session`, `cancel_run`, `interrupt_run`; chord syntax; `[]` to disable; `Ctrl-C`, `Enter` and bare characters cannot be bound); terminal caveats (`Shift-Enter`, `Ctrl-\`, `Alt-*` under tmux) | `qq-tui/src/commands.rs` `COMMANDS` chords, `settings.rs` defaults, `app.rs` and `app/pickers.rs` key handlers, `input.rs`; `tui.md` §Every command (keep; link); `configuration.md` §`tui.ron` | expose the default chords from `qq-tui` as strings beside `slash_names()` (the table's `Command` type is private) and assert each; publish `TUI_BINDING_FIELD_NAMES` in `qq-config` and assert; a hand-listed const for the hard-coded keys | S/M |
| GE6 | `server.md` — Server and protocol | do I need `qq serve` (only to keep runs alive after the TUI quits, share one server between clients, or reach it remotely); bind, expose (loopback only today; tunnel), browsers (`--allow-origin`); authentication (`server.ron` in the runtime dir — `$XDG_RUNTIME_DIR/qq` on Linux — and its bearer token; scripting against the API; several clients); routes and the event stream (`Last-Event-ID` required, cursor replay, a stale cursor → 400 → resnapshot), limits (64 requests / 64 subscriptions → 503); what `qq version`'s protocol / capabilities / descriptor / store-schema numbers mean when mixing binaries; enrollment (ADR-0015, Proposed) and TLS (ADR-0016, reserved, not written) | `headless.md` §`qq serve` (keep the three invocations; link); `design/protocol.md`, `design/headless-contract.md` §`qq serve`; `qq-server/src/lib.rs` limits, router, discovery; `qq-protocol` `COMMAND_ROUTES`, `PROTOCOL_VERSION` | assert every route in `qq_protocol::COMMAND_ROUTES` plus the fixed routes appears as a code span; tell readers to run `qq version` rather than hard-coding numbers. **Fix `design/protocol.md`** (acceptance 6) | M/L |
| GE7 | `enterprise.md` — Managed and organization configuration | enforcing settings users cannot override (managed dir per OS; on Unix root-owned, not group/world-writable, no symlinks; the nine managed-only policy keys; applied after `QQ_*` overrides); MDM / GPO (`dev.qq` forced preference `ManagedConfig` on macOS; `REG_SZ` `HKLM\Software\Policies\dev.qq\ManagedConfig` on Windows; none on Linux) and what the value must contain; publishing a team manifest (HTTPS, no redirects, ≤ 1 MiB, must set `organization` to its own name) and enrolling (`qq org enroll/use/list/refresh/remove`, `--organization`, `QQ_ORGANIZATION`); when the manifest is fetched (enroll and `refresh` only; cache; failed refresh keeps last good); what a manifest cannot do (secrets, local credential references, MCP servers, approval grants, bypass project trust); debugging with `qq config explain` / `paths` | `configuration.md` precedence rows 3, 9, 10 and the managed-only keys; `cli.md` §`qq org`; `qq-config/src/loader.rs` managed order and ownership checks, `document.rs` `validate`, `managed.rs`, `remote.rs` | hoist the MDM identifiers out of their functions to `pub const` and assert; assert `managed.ron`, `managed.d`, `organizations.ron` names | S/M |
| GE8 | Changelog on the site | what changed between versions, is it breaking, which PR; which version am I on | `CHANGELOG.md` (written by `cargo xtask release` since #160; **does not exist on `main` until the next release**); `website/scripts/sync-docs.mjs` | `sync-docs` writes `changelog.md` from the repo-root file when present, with a `sidebar.json` row and `editUrl` pointing at `runbooks/release.md`; skips with a log line when absent so `main` keeps building before the first release; update `runbooks/website.md` "Add a page" for the special case | S |

## Order

1. **GE0 first.** The live site states things that are false, and GE2
   builds on the corrected server-ownership story.
2. **GE10, GE5, GE4** — the definitions and references every other page
   links to.
3. **GE2, GE3, GE1, GE9** — the pages a user reaches for in their first
   week. GE9 links into the others, so it lands after them.
4. **GE6, GE7** — shared servers and fleet administration.
5. **GE8** lands with or after the first release that writes
   `CHANGELOG.md` (v0.1.5).

Two writers may run in parallel if they claim different pages and merge the
ledger in stack order.

## Sidebar placement

`website/sidebar.json` groups: `concepts` → **Getting started**, after
Quickstart; `workflows`, `agents`, `sessions`, `skills` → **Using QQ**;
`environment`, `keybindings`, `server`, `enterprise` → **Reference**;
`changelog` → **Help**. Labels: "Concepts and glossary", "Common
workflows", "Agents, profiles, and packs", "Sessions", "Skills and
commands", "Environment variables", "Keybindings", "Server and protocol",
"Managed and organization config", "Changelog".

## Out of scope

- New behavior. If a page reveals a gap, file it against the owning plan
  and document what ships today. Known so far: the composer has no
  kill-to-end-of-line key (`Ctrl-K` belongs to the palette); `FocusParent`
  has no chord outside `Esc`.
- Restructuring the existing eleven pages beyond GE0's corrections and the
  hand-offs named in GE4 and GE5.
- Translation; screenshots or recorded terminals (the landing page's TUI
  mocks are drawn from the goldens and are enough).

## Decisions carried

| Decision | Where recorded |
| --- | --- |
| The guide is the single source; the site is generated from it | onboarding OB12, `runbooks/website.md` |
| Docs-truth is code→docs only; docs→code truth is checked on a build (acceptance 2) | onboarding OB10 receipt; this plan |
| Proposed ADRs are named as proposed, never described as behavior | this plan, acceptance 1 |
