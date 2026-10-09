# Onboarding and configuration UX audit — 2026-09-22

**Kind:** research (design). Motivated the onboarding plan, closed 2026-09-25 with every finding addressed; receipt in [`../plans/progress/onboarding-ux.md`](../plans/progress/onboarding-ux.md).
**Method:** read the vendored sources of Codex, OpenCode, Pi, and fx under
`.source/`, traced QQ's own first-run paths in `src/main.rs`, `qq-config`,
`qq-auth`, and `qq-tui`, and ran a fresh-machine simulation (empty
`XDG_CONFIG_HOME`, no credentials, a clone of this repository).

## 1. The question

`docs/design/product.md` promises: *"Running QQ in a repository should
require no ceremony: `cd my-project; qq`."* Today that promise holds only for
someone who already has a model configured, a credential stored, and the
project trusted. For everybody else the first minute looks like this:

| Situation | What the user sees | Where |
| --- | --- | --- |
| Fresh machine, no config | `error: no model is configured. Choose one with any of: …` (actionable since #119) | `crates/qq-config/src/lib.rs` `ModelRequired` |
| Clone with `.qq/config.ron`, first open | `error: project configuration trust is required` — no path, no `qq trust` hint | `crates/qq-config/src/lib.rs` `TrustRequired`, `src/main.rs` error sink |
| Model set, no credential, bare `qq` | TUI opens with `Alt-N creates the first session.`; Alt-N → `choose a model with /models before creating a session`; `/models` → `no authenticated providers have selectable models`. Nothing names the provider, env var, or `qq auth login` | `src/main.rs` `interactive()`, `crates/qq-tui/src/app/pickers.rs` |
| Model set, no credential, `qq ask` | `error: environment variable `OPENAI_API_KEY` is not set` (openai/anthropic/google) or `error: provider response failed: request credentials are missing` (xai/codex) | `crates/qq-auth/src/lib.rs`, `crates/qq-provider/src/request_auth.rs` |
| `qq auth login opnai` (typo) | `stored opnai/default in keyring` — a dead credential, silently | `src/main.rs` `auth_command` |
| `qq trust` | `trusted /repo/.qq/config.ron` — without saying what was in it | `src/main.rs` `trust_command` |
| Bare `qq` in a pipe | `error: interactive mode requires a terminal` — no pointer to `qq ask`/`qq run` | `src/main.rs` |
| Exit TUI | `To continue this session: qq run --session ID "<prompt>"` — the TUI continuation `qq --session ID` is not shown | `src/cli.rs` `resume_hint` |

Distribution: GitHub release archives only. No install script, Homebrew tap,
`cargo install`, cargo-binstall metadata, or Nix package/app (the flake is a
dev shell). The README installs by prose ("put `qq` on your `PATH`") and every
example runs `cargo run -- …`.

Documentation: `docs/` is an engineering corpus (design, ADRs, plans,
runbooks). There is no user guide, no configuration reference (the `Document`
struct has 17 top-level keys; the README documents 5), no troubleshooting
page, no CONTRIBUTING, SECURITY, CODE_OF_CONDUCT, CHANGELOG, issue or PR
templates.

## 2. What the reference harnesses do

All four reference harnesses converge on the same first-run shape. File
references are under `.source/`.

### 2.1 Install and first launch

| | Codex | OpenCode | Pi | fx |
| --- | --- | --- | --- | --- |
| Install | `curl … \| sh`, npm, brew cask | `curl … \| bash`, npm, brew, scoop, choco, pacman, mise, nix | npm, `curl … \| sh` | `curl … \| bash` (single static binary) |
| First command | `codex` | `opencode` | `pi` | `fx` |
| Zero-config launch | Welcome → Auth → Trust wizard (`codex-rs/tui/src/onboarding/onboarding_screen.rs`) | Home screen; provider-connect dialog auto-opens when no provider (`packages/tui/src/app.tsx:540`) | Header with key hints; warning when no credential; optional first-time theme/analytics setup | Provider picker opens before first frame when no credential (`app_bootstrap_runtime.zig:224`) |

Nobody exits. Every harness reaches an interactive surface and asks the user
for the one thing it needs. Codex and fx put explicit escape hatches on the
first screen: *"esc to set up later · explore all commands with /help"*.

### 2.2 Authentication

| | Codex | OpenCode | Pi | fx |
| --- | --- | --- | --- | --- |
| Methods | ChatGPT OAuth, device code, API key, Bedrock | OAuth or API key per provider via `/connect` | `/login` OAuth or API key; env vars | Vercel/Codex/Grok OAuth, API key, env |
| Provider tiering | Billing hint per method (*"Usage included with Plus, Pro…"* / *"Pay for what you use"*) | *"Popular"* group with *"(Recommended)"*, *"(API key)"*, *"(ChatGPT Plus/Pro or API key)"* | Default model per provider table | Recommended source first |
| Missing credential | `codex doctor` remediation: *"Run codex login or provide an API key through a supported auth env var."* | Toast *"Connect a provider to send prompts"*; footer *"Connect a provider"* | Centralised `auth-guidance.ts`: *"No models available. Use /login … See: docs/providers.md"* | Distinguishes CLI (*"Run fx login"*) from in-app (*"Run /login"*) remediation |
| Storage | `auth.json` or keyring (`cli_auth_credentials_store`) | `auth.json` 0600 | `auth.json` 0600 | `~/.fx/*.json` 0600, optional keychain |

QQ's credential store (OS keyring, DPAPI on Windows, explicit `--allow-file`)
is stronger than any of these. Its *guidance* is weaker: no central place
turns "credential missing for provider X" into "run `qq auth login X` or set
`X_API_KEY`".

### 2.3 Model defaults

Codex hardcodes `gpt-5.5`; fx hardcodes `moonshotai/kimi-k3`; OpenCode picks
the first authenticated provider by a priority list; Pi has a
default-model-per-provider table and picks the first authenticated one. All
four let the user change models with `/model[s]` and a flag.

QQ requires an explicit route. The error since #119 is good; the decision to
stay explicit is recorded in § 4 and revisitable.

### 2.4 Configuration

| | Codex | OpenCode | Pi | fx |
| --- | --- | --- | --- | --- |
| Format | TOML | JSON/JSONC | JSON | JSON |
| Schema | `core/config.schema.json` in repo | `https://opencode.ai/config.json`, seeded into a new global file as `$schema` | none | none |
| Layers | 9, numbered (`config_layer_source.rs`) | remote → global → env → project → managed → MDM | global + project (after trust) | env > profile > project > defaults |
| Invalid config | `Error loading config.toml:` with `path:line:col` and `^^^` carets | Typed error map with *"Did you mean:"* / *"Try:"* and a directory-typo detector | `Invalid settings file <path>: <msg>` | `fx: config user: malformed_settings` |
| Init | `/init` (writes AGENTS.md) | seeds `$schema` | `/settings` UI | `fx setup`, `/settings` |
| In-app inspection | `/debug-config`, `/status` | `/help`, palette | `/settings`, `/hotkeys` | `/settings`, `fx status --json`, `fx doctor` |

QQ already has the strongest *layering* story (compiled → global →
`config.d` → project root-to-leaf → explicit → inline → overrides → managed →
MDM, digest-trusted project layers, `qq config sources|explain`). It has no
key reference, no example file, no `init`, and RON has no editor schema story.

### 2.5 Trust and permissions

| | Codex | OpenCode | Pi | fx |
| --- | --- | --- | --- | --- |
| Directory trust | Wizard step: *"Do you trust the contents of this directory? … Trusting the directory allows project-local config, hooks, and exec policies to load."* Yes/No | none | *"Trust project folder?"* with Trust / Trust parent / This session only / Do not trust | none (MCP servers need `/mcp trust approve`) |
| Permission presets | Read Only / Default / Full Access with one-sentence descriptions | `build` / `plan` agents (Tab); Allow once / Allow always / Reject | none (documented: sandbox it yourself) | ask / auto / full access; LLM reviewer |

QQ's model — `read_only`, `ask`, `auto`, `supervised`, `full`; a CST shell
classifier with allow/prompt/forbidden; effect-classified approval;
once/session/workspace grants written back to `.qq/config.ron` — is more
capable than any of them. It is also the least explained: `qq trust` shows
nothing, there is no in-TUI trust prompt, and the `w` (workspace) grant key
appears nowhere in the README.

### 2.6 Discoverability and errors

- Codex opens every first session with *"To get started, describe a task or
  try one of these commands: /init /status /permissions /model /review"* and
  rotates tips from `tui/assets/tooltips.txt`; the footer says *"? for
  shortcuts"*.
- OpenCode's palette flags *suggested* commands (e.g. `provider.connect` when
  no provider is connected).
- Pi's header prints a compact keybinding cheat sheet and *"Pi can explain
  its own features and look up its docs."*
- fx prints `𝒇x <build> · Run /help for commands` and has `fx doctor`.
- Every one of them formats fatal errors with a next action. Codex additionally
  has `codex doctor` with per-check remediation strings.

QQ has `/help` (F1), `Ctrl-K` palette, and a good footer hint row, but no
empty-state guidance, no doctor, and one error sink (`error: {error}`) that
prints whatever `Display` says.

### 2.7 Documentation and community

| | Codex | OpenCode | Pi | fx |
| --- | --- | --- | --- | --- |
| README | Quickstart / Install / Plans / Docs | Install / Desktop / Agents / Docs / Contributing | Quick Start / Providers / Interactive / Sessions / Settings / Telemetry / CLI ref | Install / Run / Embed / Extend / Docs |
| Docs | external site | Astro Starlight in repo, 20+ locales | `docs/*.md` + `docs.json` nav | external site |
| CHANGELOG / CONTRIBUTING / templates | releases; 6 issue templates, CODEOWNERS | PR template, 2 issue templates, Discord | Keep-a-Changelog; 3 templates | `CHANGELOG.md`; 1 template |

OpenCode and Pi keep authoritative user docs in-tree next to the code they
describe, which is the only arrangement that survives fast iteration.

## 3. Findings

Ordered by how many new users hit them and how cheap the fix is.

| ID | Finding | Evidence |
| --- | --- | --- |
| O01 | `TrustRequired` names no file and no command | `qq-config/src/lib.rs` Display; `pending` dropped by `src/main.rs` sink |
| O02 | TUI with model but no credential opens silently with no next step | `src/main.rs` `interactive()`; `qq-tui` empty-state strings |
| O03 | Credential-missing errors do not mention `qq auth login` (plan-time) or the provider at all (request-time) | `qq-auth/src/lib.rs` `EnvironmentVariableMissing`; `qq-provider/src/request_auth.rs` `Missing` |
| O04 | No copy-pasteable install; README examples use `cargo run` | `README.md` § Install |
| O05 | `qq trust` grants blind | `src/main.rs` `trust_command` |
| O06 | No configuration reference, example file, or `init` | `qq-config/src/document.rs` `Document` (17 keys) vs README (5) |
| O07 | `qq auth login` accepts any provider string | `src/main.rs` `auth_command`, `built_in_endpoint` |
| O08 | Resume hint omits `qq --session` | `src/cli.rs` `resume_hint` |
| O09 | No readiness check (`doctor`) | `qq config check` validates syntax only |
| O10 | `config paths` does not say which files exist | `src/main.rs` |
| O11 | `w`/workspace grant undocumented; approval modes documented only in prose | README § Tools And Approvals |
| O12 | Non-TTY bare `qq` does not point at `ask`/`run` | `src/main.rs` |
| O13 | No Anthropic quick start despite a built-in provider | README |
| O14 | No CONTRIBUTING, SECURITY, CODE_OF_CONDUCT, CHANGELOG, issue/PR templates | `.github/` has only workflows |
| O15 | Stale `/status` comment; no such command | `qq-tui/src/view/chrome.rs` |
| O16 | `GOOGLE_API_KEY` not accepted (only `GEMINI_API_KEY`) | `qq-config/src/providers.rs` |
| O17 | Editing a trusted `.qq/config.ron` re-requires trust with the same opaque error | digest-per-file trust; O01 |
| O18 | `qq run` defaults to `--approval read-only`; denials give no hint to raise it | `src/cli.rs` |
| O19 | Empty-state strings carry no guidance | `qq-tui/src/view/transcript.rs`, `sidebar.rs` |
| O20 | No Nix package/app; no Homebrew; no `cargo install` | `flake.nix`, `nix/packages.nix` |

## 4. Positions taken

These are the decisions the plan builds on. Each is small enough to record
here; the ones that change a boundary get an ADR when their slice ships.

1. **Never exit when an interactive surface could ask instead.** Bare `qq`
   with a valid configuration that lacks only a model or a credential opens
   the TUI and routes to the thing that is missing. Headless entry points
   (`ask`, `run`, `serve`) keep failing fast with actionable text.
2. **Every actionable error names its next command.** The rule for a
   `ConfigError`/`AuthError` Display string that a new user can hit: name the
   file or credential, and name the exact `qq …` command or environment
   variable that fixes it. Test it.
3. **Stay explicit about the model.** QQ does not invent a default route.
   The repository's own `.qq/config.ron` carries the team default; the guide
   tells each user how to pick theirs. Revisit when there is evidence that
   the explicit step loses users.
4. **User docs live in-tree under `docs/guide/`**, plain Markdown, one page
   per task, amended in the same PR as the behavior. They are the source for
   a GitHub Wiki mirror and a future docs site; no generator is added until a
   page needs one.
5. **Trust is shown before it is granted.** `qq trust` and any future in-TUI
   prompt list what the pending configuration declares (providers, MCP
   servers and their commands, grants, packs) so the user can decide.
6. **One install command per platform.** A POSIX install script and a
   Homebrew tap wrap the existing release archives; the Nix flake exposes a
   `qq` package. `cargo install --git` is documented as the from-source path.

## 5. Out of scope here

Provider breadth, auto-selected default models, telemetry, a hosted docs
site, localisation, and an in-TUI settings editor. Each is a plan slice or a
later plan, not part of this audit's remit.
