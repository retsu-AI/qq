# Onboarding UX: from `git clone` to a working agent in one minute

**Status:** Active from 2026-09-22. Ledger:
[`progress/onboarding-ux.md`](progress/onboarding-ux.md). Research:
[`../design/onboarding-audit-2026-09-22.md`](../design/onboarding-audit-2026-09-22.md).
**Linear:** [ENG-875](https://linear.app/retsu-ai/issue/ENG-875) (parent);
per slice: OB0 ENG-859 (#119) + this PR, OB1 ENG-860, OB2 ENG-876, OB3
ENG-877, OB4 ENG-878, OB5 ENG-879, OB6 ENG-880, OB7 ENG-881, OB8 ENG-882,
OB9 ENG-861, OB10 ENG-883, OB11 ENG-884 (superseded by OB12), OB12 ENG-896.

## Goal

A developer who has never seen QQ installs it with one command, runs `qq` in
a repository, and is talking to an agent within a minute — with every step
along the way either done for them or explained by the tool in front of them.
An existing user finds one reference page for every setting, every command,
and every failure message.

Acceptance for the plan as a whole (the fresh-machine script in
[`../guide/quickstart.md`](../guide/quickstart.md) is the oracle):

1. `curl … | sh` (or `brew install`, or `nix run`) yields a working `qq` on
   Linux and macOS; Windows has a documented archive path.
2. `qq` in a directory with no configuration opens the TUI and asks for a
   model; `qq` with a model but no credential opens the TUI and says which
   credential and how to add it.
3. Every startup failure a new user can trigger names a file or credential
   and the command that fixes it (tested for each `ConfigError`/`AuthError`
   variant reachable from `main`).
4. `qq doctor` reports readiness in one screen.
5. Every configuration key, environment variable, CLI flag, slash command,
   and keybinding is documented in `docs/guide/`, and CI fails when a
   documented key or command disappears from the code.

## Task index

| ID | Slice | Owned paths | Status |
| --- | --- | --- | --- |
| OB0 | Audit, plan, user guide skeleton, community files, small P0 error fixes (O01, O05, O07, O08, O12; O03 plan-time half) | `docs/guide/`, `docs/design/onboarding-audit-*`, `README.md`, `CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`, `.github/ISSUE_TEMPLATE/`, `.github/pull_request_template.md`, `src/main.rs`, `src/cli.rs`, `crates/qq-config/src/lib.rs`, `crates/qq-auth/src/lib.rs` | In review |
| OB1 | Open the TUI without a model; route to `/models` (ENG-860) | `src/main.rs`, `src/runtime.rs`, `crates/qq-config` (model-optional load), `crates/qq-tui` empty-state | Planned |
| OB2 | Open the TUI without a credential; empty state names the provider, env var, and `qq auth login`; `/models` lists unauthenticated built-ins as `needs credential` (O02, O19) | `src/main.rs`, `src/runtime.rs`, `crates/qq-tui/src/app/pickers.rs`, `view/transcript.rs`, `view/sidebar.rs` | Planned |
| OB3 | Request-time credential errors name provider and remedy (O03 request-time half; O16 `GOOGLE_API_KEY` alias) | `crates/qq-provider/src/request_auth.rs`, `crates/qq-auth`, `crates/qq-config/src/providers.rs` | Planned |
| OB4 | `qq doctor`: config, model, credential, keyring, server, workspace trust, in one screen with remediation per line (O09, O10) | `src/cli.rs`, `src/main.rs` (new `doctor.rs`) | Planned |
| OB5 | `qq init [--global]`: write a commented `config.ron` with the provider the user picks, refuse to overwrite; `qq config paths` marks existing files (O06, O10) | `src/cli.rs`, `src/main.rs`, `docs/guide/configuration.md` | Planned |
| OB6 | Install: `install.sh` for the release archives, Homebrew tap formula, Nix flake `packages.qq` + `apps.default`, cargo-binstall metadata; README leads with them (O04, O20) | `install.sh`, `flake.nix`, `nix/`, `Cargo.toml` `[package.metadata.binstall]`, `.github/workflows/release.yml`, `docs/guide/install.md`, `docs/runbooks/release.md` | Planned |
| OB7 | In-TUI trust prompt: an untrusted project opens the TUI with a hold that lists what the configuration declares; Trust / This session / Quit (O01, O17; ADR) | `crates/qq-tui`, `crates/qq-client`, `src/main.rs`, `docs/design/tools.md` | Planned |
| OB8 | First-session guidance: a one-time "try these" cell and a `? for help` footer hint; `qq run` denial notice suggests `--approval auto` (O18, O19) | `crates/qq-tui/src/view/transcript.rs`, `src/headless.rs` | Planned |
| OB9 | Degrade an MCP server whose `Stored(...)` bearer is unregistered (ENG-861) | `src/mcp.rs`, `crates/qq-mcp` | Planned |
| OB10 | Docs CI: a test that every `Document` key, `PolicyPatch` key, env var, `CommandSpec` slash name, and `ConfigCommand` appears in `docs/guide/`; CHANGELOG generated from Conventional Commits at release | `xtask/`, `tests/`, `.github/workflows/ci.yml`, `docs/runbooks/release.md` | Planned |
| OB11 | Wiki mirror: a release-time job pushes `docs/guide/` to the GitHub Wiki with a `_Sidebar.md`; decide on a docs site when the guide exceeds what a wiki renders well | `.github/workflows/`, `docs/guide/_Sidebar.md` | Superseded by OB12 |
| OB12 | Docs website: an Astro/Starlight site under `website/` whose documentation pages are generated from `docs/guide/` at build time, with a landing page, search, and the real `install.sh`; built on every PR that touches it or the guide, deployed to GitHub Pages from `main` | `website/`, `.github/workflows/website.yml`, `nix/dev-shells.nix` | In review |

Dependencies: OB1 → OB2 (both change `interactive()`); OB2 → OB8; OB5 and
OB4 are independent; OB6 is independent of code; OB7 needs a protocol
addition and its ADR; OB10 after the guide is stable (OB0 + OB5).

## Slices

### OB0 — Audit, guide, community files, P0 error text

**Inputs:** #119. **Gates:** none (error path only).
**Acceptance:**
- `docs/guide/` pages: index, install, quickstart, configuration (every
  `Document`/`PolicyPatch`/`tui.ron` key), providers, permissions, tui,
  headless, mcp, troubleshooting (every startup error a new user can hit,
  with its fix), faq.
- `TrustRequired` Display names each pending file and `qq trust`; test.
- `qq trust` prints, per pending file, the sensitive sections it declares;
  test.
- `qq auth login <unknown>` fails naming the built-in providers; test.
- `EnvironmentVariableMissing` names the `qq auth login` alternative; test.
- `resume_hint` shows `qq --session` and `qq run --session`; existing test
  updated.
- Non-TTY bare `qq` points at `qq ask` / `qq run`.
- CONTRIBUTING, SECURITY, CODE_OF_CONDUCT, issue templates (bug, feature,
  docs), PR template.
- README under 120 lines: what, install, first run, links.
**Docs:** this plan, the audit, `docs/README.md` index, `plans/README.md`.

### OB1 — TUI opens without a model

**Inputs:** OB0. **Acceptance:** `qq` with `(version: 1)` opens the TUI;
top row shows `no model`; the composer notice says `choose a model with
/models`; `/models` lists every authenticated built-in; a run attempt before
choosing fails with `ModelRequired`'s text as a notice, not an exit.
`qq ask`/`qq run` still exit with the actionable message. Server `ListModels`
does not require a model. Tests: `interactive()` load path; `models_for`
without a model; TUI reducer with `model: None`.
**Docs:** `guide/quickstart.md`, `guide/tui.md`.

### OB2 — TUI opens without a credential

**Inputs:** OB1. **Acceptance:** with `model: "openai/gpt-5.6"` and no
credential, the transcript empty state reads
`openai needs a credential: run qq auth login openai or set OPENAI_API_KEY`;
`/models` shows unauthenticated built-ins greyed with `needs credential`.
Alt-N with an unauthenticated model warns with the same text. Tests on the
descriptor builder (`ModelDescriptor` gains an `available: bool` or a sibling
list) and the TUI reducer.
**Docs:** `guide/providers.md`, `guide/troubleshooting.md`.

### OB3 — Request-time credential errors

**Inputs:** none. **Acceptance:** `RequestCredentialError::Missing` carries
the provider id and the remedy so the run failure reads
`xai credential is missing: run qq auth login xai --oauth or set XAI_API_KEY`;
`GOOGLE_API_KEY` accepted as an alias of `GEMINI_API_KEY`. Tests in
`qq-provider` (both feature profiles) and `qq-auth`.
**Docs:** `guide/providers.md`, `guide/troubleshooting.md`.

### OB4 — `qq doctor`

**Inputs:** OB0. **Acceptance:** one screen, one line per check, `ok` /
`warn` / `fail` plus a remedy: configuration parses; model set and provider
known; credential resolvable for the model's provider; keyring backend
reachable; project trust; server discovery (running / none); workspace is a
directory; `qq --version` contracts. Exit 0 when nothing fails. `--json`.
Tests with a temp config tree and `MemoryKeyring`.
**Docs:** `guide/troubleshooting.md`, `guide/cli.md`.

### OB5 — `qq init`

**Inputs:** OB0. **Acceptance:** `qq init` writes
`<global>/config.ron` (or `.qq/config.ron` with `--project`) from a
commented template with the chosen `model:`; refuses to overwrite without
`--force`; prints the path and the next command (`qq auth login <provider>`).
Non-interactive with `--model`. `qq config paths` appends `(exists)` /
`(missing)`. Tests.
**Docs:** `guide/configuration.md`, `guide/quickstart.md`.

### OB6 — Install paths

**Inputs:** none. **Acceptance:** `install.sh` downloads the matching
release archive, verifies `SHA256SUMS`, installs to `~/.local/bin` (or
`QQ_INSTALL_DIR`), and prints the PATH line if needed; Homebrew formula in
`retsu-AI/homebrew-qq` generated by the release workflow; `nix run
github:retsu-AI/qq` runs `qq`; `cargo binstall qq` resolves. README install
section shows all four.
**Docs:** `guide/install.md`, `runbooks/release.md`. ADR if the release
workflow's signing or hosting boundary changes.

### OB7 — In-TUI trust prompt

**Inputs:** OB1. **Acceptance:** an untrusted `.qq/config.ron` opens the TUI
with a hold `◇ this project's configuration needs your trust` listing files
and their sensitive sections (providers, MCP servers with command/url,
grants, packs); keys `t trust  s this session  q quit`; `t` persists like
`qq trust`; `s` loads it for the process only. Headless surfaces keep
failing fast. Protocol: a new hold kind or a client-side prompt fed by
`ConfigError::TrustRequired` details; decide in the ADR.
**Docs:** `guide/permissions.md`, `design/tools.md`, ADR.

### OB8 — First-session guidance

**Inputs:** OB2. **Acceptance:** the first session in a workspace shows a
"try one of these" cell (`/models`, `/approval`, `/skills`, `@file`
mentions) that disappears after the first prompt; footer hint `? help` when
the composer is empty; `qq run` text mode prints `held calls were denied
under --approval read-only; rerun with --approval auto to allow workspace
edits` when at least one denial occurred.
**Docs:** `guide/tui.md`, `guide/headless.md`.

### OB9 — Missing MCP credential degrades the server

See ENG-861. **Acceptance:** `Stored` bearer unresolvable → that server is
`unavailable` with a message naming the credential and `qq auth set NAME`;
other servers and built-ins run.

### OB10 — Docs stay true

**Acceptance:** a workspace test enumerates `Document` and `PolicyPatch`
field names, `LoadRequest` env vars, `COMMANDS` slash names, and `ConfigCommand`
variants and asserts each appears in `docs/guide/**.md`; CI runs it.
`cargo xtask release` writes `CHANGELOG.md` from Conventional Commit subjects
since the last tag.

### OB11 — Wiki mirror

Superseded by OB12: a site renders the guide better than the wiki and can
carry a landing page and search; keeping both would mean two publishing
paths for one source.

### OB12 — Docs website

**Acceptance:** `website/` builds a static Astro + Starlight site.
`docs/guide/*.md` is the only source of the documentation pages: a build
step generates them (frontmatter from an H1 and a sidebar manifest, relative
links rewritten to routes, links leaving `docs/guide/` pointed at GitHub) and
fails when the manifest and the guide disagree. The site serves the reviewed
`install.sh` so the landing page's one-line install is real. A post-build
check fails on any unresolved internal link or fragment. CI builds the site
on every PR touching `website/`, `docs/guide/`, `install.sh`, or
`Cargo.toml`; pushes to `main` deploy to GitHub Pages. No client framework,
analytics, or third-party script. The landing page claims nothing the guide
does not say.
**Docs:** `website/README.md`, `runbooks/website.md`, `guide/install.md`.

## Decisions carried

| Decision | Where recorded |
| --- | --- |
| No built-in default model; explicit route stays required for headless | audit § 4.3; revisit with evidence |
| User docs in-tree under `docs/guide/`, plain Markdown | audit § 4.4 |
| Trust shown before granted | audit § 4.5; ADR with OB7 |
| Install script + brew + nix + binstall wrap the existing archives; no new hosting | audit § 4.6 |
