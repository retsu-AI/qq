# Ledger — Onboarding UX

Plan: [`../onboarding-ux.md`](../onboarding-ux.md). Only the agent working
this plan edits this file. Current state on top; dated entries appended
below, newest last.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| OB0 | Audit, plan, user guide, community files, P0 error text | Shipped (#128) | `feat/eng-875-onboarding-ux` | ENG-875; ENG-859 shipped separately as #119 |
| OB1 | TUI opens without a model | Shipped (#136) | `feat/eng-860-tui-without-model` | ENG-860; shares the branch with OB2 |
| OB2 | TUI opens without a credential; empty state names the remedy | Shipped (#136) | `feat/eng-860-tui-without-model` | ENG-876; shares the branch with OB1 |
| OB3 | Request-time credential errors name provider and remedy; `GOOGLE_API_KEY` alias | Shipped (#137) | `fix/eng-877-request-credential-errors` | ENG-877 |
| OB4 | `qq doctor` | Shipped (#138) | `feat/eng-878-doctor` | ENG-878 |
| OB5 | `qq init`; `config paths` marks existing files | Shipped (#147) | `feat/eng-879-init` | ENG-879 |
| OB6 | `install.sh`, Homebrew tap, Nix package, binstall | Shipped (#139) | `feat/eng-880-install-paths` | ENG-880; tap repo + `HOMEBREW_TAP_TOKEN` are owner setup |
| OB7 | In-TUI trust prompt | Planned | | ENG-881; needs ADR + protocol row in root |
| OB8 | First-session guidance; `qq run` denial hint | Shipped (#146) | `feat/eng-882-first-session-guidance` | ENG-882 |
| OB9 | Missing MCP credential degrades the server | Shipped (#148) | `fix/eng-861-mcp-credential-degrade` | ENG-861 |
| OB10 | Docs-truth test; CHANGELOG at release | In review | `feat/eng-883-docs-truth` | ENG-883 |
| OB11 | Wiki mirror workflow | Superseded by OB12 | | ENG-884 |
| OB12 | Docs website from `docs/guide/`, GitHub Pages | Shipped (#156) | `feat/eng-896-docs-website` | ENG-896 |

## Entries

### 2026-09-22 — plan opened; OB0

Research in `docs/design/onboarding-audit-2026-09-22.md`: read the four
vendored harnesses under `.source/` for their first-run, auth, config,
trust, help, and docs surfaces; traced QQ's own startup paths; ran a
fresh-machine simulation. Findings O01–O20.

OB0 ships in this PR: `docs/guide/` (11 pages), README rewrite,
CONTRIBUTING / SECURITY / CODE_OF_CONDUCT, issue and PR templates, and the
small error-text fixes (`TrustRequired` names files and `qq trust`; `qq
trust` lists what it trusts; `qq auth login` rejects unknown providers;
`EnvironmentVariableMissing` names `qq auth login`; resume hint shows both
continuations; non-TTY bare `qq` points at `ask`/`run`). No hot-path change;
no protocol or schema change. Linear issues filed per slice.

### 2026-09-22 — OB1 + OB2 in review

Branch `feat/eng-860-tui-without-model` (off `v0.1.3`), four commits:
config `load_for_client` → `ClientSnapshot` (model optional; `load`
unchanged); runtime `load_for_client` / `client_model_options` /
`unauthenticated_providers`, `models_for` no longer requires a model;
TUI `TuiOptions.unauthenticated_providers`, `no model` top row, standing
`choose a model with /models` rule, empty-state remedy line, `needs
credential` picker rows, Alt-N remedy warning; `interactive()` wired.
Tests added: 1 qq-config, 2 qq (bin), 3 qq-tui reducer, 3 qq-tui view
(9 total). Gates: fmt, clippy `-D warnings`, `cargo test --workspace` all
green (qq-config 94, qq bin 178, qq-tui 317). Manual: bare `qq` under
`QQ_CONFIG_CONTENT='(version: 1)'` in a pty paints `no model` and the
`/models` hint; with `model: "openai/gpt-5.6"` and no key paints the openai
remedy; `qq ask`/`qq run` still exit with `no model is configured`.
Deviation: OB2 acceptance said `guide/providers.md`; the guidance landed in
`guide/tui.md` and `guide/troubleshooting.md` where the states are described.
No protocol change; no perf gate named.

### 2026-09-22 — OB3 in review

`RequestCredentialError::Missing` now carries an optional `Arc<str>` remedy
that `qq-auth` authors once per request-credential provider (xAI, Codex) and
attaches when mapping `ProviderCredentialMissing` /
`StoredCredentialNotRegistered` / `StoredCredentialMissing`; `qq-provider`
displays it opaquely, keeps the old text when absent, and the
`Authentication` classification is unchanged. `qq ask hi` on `xai/...` with
nothing stored reads `` provider response failed: no credential for provider
`xai`: run `qq auth login xai --oauth` or `qq auth login xai` or set the
environment variable `XAI_API_KEY` ``. `HttpCredential::ApiKey` gained
`alternate_variables`; google lists `GOOGLE_API_KEY`, read after
`GEMINI_API_KEY` by `resolve_provider_credential_with_aliases`, and the
plan-time message names both. Descriptor still reports `GEMINI_API_KEY` so
the plan digest is spelling-independent. Tests in qq-provider (both feature
profiles), qq-auth, and the bin (child process with `GOOGLE_API_KEY` only).

### 2026-09-22 — OB4 `qq doctor` in review

Branch `feat/eng-878-doctor` off `v0.1.3`. New `src/doctor.rs`: `run_checks`
returns a `DoctorReport` (no printing); `render_text` / `render_json` are
separate. Eight checks in order: configuration, project trust, model,
credential, credential store, server, workspace, data; statuses
`ok`/`warn`/`fail`/`skipped`; exit 1 only on `fail`. Credential logic mirrors
`RuntimeFactory::provider_authenticated`; the AWS helpers in `runtime.rs`
became `pub(crate)` so both share one definition. No provider network;
server discovery reuses `discover_at` over loopback. Tests: 11 in
`doctor::tests` + 1 CLI parse test (temp roots, `MemoryKeyring`,
`UnavailableKeyring`). Gates green: fmt, clippy `-D warnings`, `cargo test
--workspace`. Docs: guide `cli.md`, `troubleshooting.md`, `quickstart.md`,
`README.md`. Deviation: the keyring is not probed blindly (unlock prompts);
it is exercised only through the model's stored credential. No perf gate.

### 2026-09-22 — OB6 install paths

Branch `feat/eng-880-install-paths`. `install.sh` (POSIX, 135 lines,
shellcheck-clean) resolves latest via the API with a redirect fallback,
verifies `SHA256SUMS`, installs to `~/.local/bin`, prints the PATH hint per
shell; `tests/install_sh.sh` (17 checks against a local `http.server`
fixture, wired into CI) covers install, `--dir`, `QQ_INSTALL_DIR`, tampered
checksum, missing release, bad args. Verified live: `curl … | sh` installs
v0.1.3 on x86_64 Linux. Nix: `packages.qq`/`default`, `apps.default`;
`nix build .#qq` succeeded in 138 s (warm crate cache), `result/bin/qq
--version` → `qq 0.1.3 (unknown unknown)` from a dirty tree; no git deps in
`Cargo.lock`, so no `outputHashes`. binstall: metadata added, `--dry-run`
resolves the real v0.1.3 archive; `repository` fixed from the `lg2m/qq` fork
to `retsu-AI/qq` (binstall derives `{ repo }` from it). `cargo xtask
homebrew-formula` (4 tests) + a guarded `homebrew` release job; untestable
until the tap and token exist. Homebrew and `nix run github:` are documented
but not exercised against the remote.

### 2026-09-23 — OB8 first-session guidance in review

Branch `feat/eng-882-first-session-guidance` off `main` (v0.1.4). Three
render-time changes, no new state and no protocol change. (a) The empty
transcript branch in `view/transcript.rs` shows `Try one of these:` with
`/models`, `/approval`, `/skills` (spelling and title read from
`commands::COMMANDS`, so the text cannot drift) and `@path — mention a file
in your prompt` when `app.sessions.len() == 1` and the focused session's
`prompt_history` is empty; `record_prompt` runs synchronously on Enter, so
the cell is gone in the next frame. Other empty sessions keep `Ask QQ to
begin this session.`; the configured-provider remedy paints above the list
when present. (b) The composer rule's help hint reads `? help` in compose
mode with an empty composer and `F1 help` otherwise; the swap is local to
the right-side loop in `chrome::composer_rule`, `hints_for` is unchanged.
(c) `RunEnd.denied_calls` counts `ToolCallFinished` events of this run with
`ToolCallState::Denied`; in text mode under `--approval read-only` with a
non-zero count, `run` prints `held calls were denied under --approval
read-only; rerun with --approval auto to allow workspace edits` on stderr
after the answer/error and before the resume hint. Exit status unchanged;
JSONL prints nothing. Tests: 3 qq-tui view tests (first session shows the
cell and drops it on Enter; second session says Ask QQ; remedy stays above),
2 existing rule tests updated for `? help` + a `F1 help`-after-typing
assertion, 4 headless tests (hint order vs resume hint; silent under auto;
silent in JSONL; silent with no denials); 34 goldens re-recorded, every
diff is the `F1 help` → `? help` swap on the rule. Gates: fmt, clippy `-D
warnings`, `cargo test -p qq-tui` (321 + 6 goldens), `cargo test -p qq --bin
qq headless` (46) green. Docs: `guide/tui.md` (layout diagram, rule bullet,
"Your first session"), `guide/headless.md` (denial hint under the approval
table). Follow-up: no golden scene covers a first empty session; add one if
the cell's layout changes. Also marked OB0 (#128) and OB6 (#139) shipped.

### 2026-09-23 — OB5 `qq init` in review

Branch `feat/eng-879-init` off `main` (v0.1.4). New `src/init.rs`:
`init::run(paths, cwd, args, chooser, stdout)` takes injected `ConfigPaths`,
an optional `BufRead` chooser (stdin when it is a terminal; `None` makes a
missing `--model` the `ModelRequired` error), and the output stream, so the
tests run against a temp tree. Writes `<global>/config.ron` (dir 0700, file
0600 on unix) or `<cwd>/.qq/config.ron` with `--project`, from a commented
RON template with the route escaped; `create_new` unless `--force`, with
`AlreadyExists` mapped to its own variant. The written file is validated
through `ConfigLoader::check` (a project file pending trust is accepted as
the documented state; anything else is `InitError::Invalid` naming the
path). Output: `wrote PATH (model: ROUTE)`, then `next:` (`qq auth login
PROVIDER  # or export VAR`, the browser sign-in for `openai-codex`, the AWS
chain for `bedrock*`, a `providers:` declaration otherwise), `then: qq`, and
for `--project` a `qq trust` note. Chooser lists the five `LOGIN_PROVIDERS`
(shared with `qq auth login`; a test pins the two lists together) with example routes and accepts a number or a full route.
`qq config paths` gained a `global config:` row, pads labels, and appends
`(exists)` / `(missing)` to every path. Tests: 15 in `init::tests` + 1 CLI
parse test. Gates green: fmt, clippy `-D warnings`, `cargo test -p qq --bin
qq` (209). Manual: global, `--project`, second run, `--force`, bad route,
unknown provider, codex, bedrock, and the pty chooser (`2`) all behave as
documented. Docs: `guide/quickstart.md` § 2 uses `qq init` (the awk/heredoc
is gone), `guide/configuration.md`, `guide/cli.md` (new `qq init` section,
`paths` row), `guide/troubleshooting.md`, `README.md`. No `qq-config`
change; no hot-path or protocol impact.

### 2026-09-23 — OB9 MCP credential degrade in review

Branch `fix/eng-861-mcp-credential-degrade` off `main` (v0.1.4). An HTTP
server whose `Stored`/`Env` bearer does not resolve no longer fails plan
compilation for the workspace (`RuntimeBuildError::Auth` →
`RunFailureKind::Authentication`); it degrades like a connection failure.
`qq-mcp`: `McpTransportSettings::Http { bearer: McpBearer }` with
`None | Token(String) | Unavailable { reason }`; `connect` returns the reason
without building a transport; `ServerHandle::tools()` returns `Err(String)`
and `McpCatalog::unavailable` is `Vec<McpUnavailable { server, reason }>`.
`src/mcp.rs::resolve_server` matches the `AuthError` exhaustively for the
variants `resolve_with_endpoint` produces (`StoredCredentialNotRegistered`,
`StoredCredentialMissing`, `EndpointMismatch`/`EndpointRequired`,
`InvalidEndpoint`, `KeyringUnavailable`, `Environment*`) and words the
reason as problem + remedy via `BearerFailure`, shared with the new `qq
doctor` `mcp` check (warn, not fail; `none declared` when empty). Readiness:
`` unavailable MCP servers: linear (credential `linear/default` is not
registered; run `qq auth set linear/default`) ``. Re-resolution after `qq
auth set` verified by test: the registry key carries the credential epoch,
which the store advances on `set_with_metadata`; a fresh
`registry_for_snapshot` under the new epoch is a different manager that
connects with the stored token (`plan` sources already fingerprint the
credential index, `runtime.rs:1160,1397`). Tests: qq-mcp 1 (declared but
unavailable, sibling unaffected, `Unavailable` call), qq bin `mcp` 2
(regression + conformance availability subset for an unresolved bearer),
`doctor` 3 (+1 skip assertion). Docs: `guide/mcp.md`,
`guide/troubleshooting.md`, `guide/cli.md` doctor table, `design/tools.md`.
No hot-path change: resolution runs once per registry miss on the compile
thread. Gates: fmt, clippy `-D warnings`, `cargo test -p qq-mcp`, `-p qq
--bin qq mcp|doctor`, `-p qq-core hosts`.

### 2026-09-23 — OB12 docs website in review

Branch `feat/eng-896-docs-website` off `main`. The v0-designed Astro 5 +
Starlight site lands under `website/`, stripped of its sandbox artifacts
(`dist/`, `.astro/`, the preview-proxy Vite hack, the root pnpm workspace
wrapper) and of every placeholder page. Content is not copied:
`scripts/sync-docs.mjs` generates `src/content/docs/docs/*.md` from
`docs/guide/*.md` before each build (title from the H1, description from
`sidebar.json`, `editUrl` back to the guide, sibling links → relative routes,
links out of `docs/guide/` → GitHub) and fails on a guide without a sidebar
entry, a sidebar entry without a guide, or a link to a missing guide. It
also copies the reviewed `install.sh` to `public/` so the landing page's
`curl … | sh` is the real installer, and reads the workspace version from
`Cargo.toml` for the release label. Generated files are gitignored.
`scripts/check-links.mjs` walks `dist/` after the build and fails on any
unresolved internal href or fragment (447 checked). Sidebar: the 11 real
guides in four groups; the eight v0 routes with no guide (agents, sessions,
skills, environment, keybindings, protocol, enterprise, changelog) are
dropped rather than shipped as stubs; the two landing links that pointed at
them now go to `tui#sessions` and `headless#qq-serve`. Landing claims were
checked against the guide (Alt-A/Alt-D, verdict table, NEEDS YOU/WORKING/
IDLE/DONE, `12% ctx $0.04`, SQLite, Windows, MIT) and the fake star count
was removed. Deployment: `retsu-ai.github.io/qq` (base `/qq/`), decided
over a custom domain and over Vercel — no new account, PR previews not
needed for docs. `.github/workflows/website.yml` builds on PRs touching
`website/`, `docs/guide/`, `install.sh`, or `Cargo.toml` and deploys from
`main` with `actions/deploy-pages`. The site is installed and run with `nub` (already in the Nix shell); CI uses `nubjs/setup-nub`, not Nix, so the job stays at seconds. OB11
(wiki mirror) is superseded. Follow-ups: OB10's docs-truth test now also
protects the site; a custom domain is two lines in `site.config.mjs` plus
`public/CNAME`; the eight dropped topics are candidate guides.

### 2026-09-24 — OB10 docs-truth and changelog in review

Branch `feat/eng-883-docs-truth` off `main` (#156). The root crate has no
lib target, so the docs-truth tests are `#[test]`s in the binary:
`src/docs_truth.rs` (`#[cfg(test)]`, declared from `main.rs`) holds the
shared guide loader and `assert_documented`, which indexes every code span
and fenced-block token in `docs/guide/*.md` and reports every miss in one
panic grouped by category; it also carries the config-key, environment
variable, and slash-command checks. `cli::tests` walks
`Cli::command()` recursively (subcommands + visible long flags; `help`/
`version` and `is_hide_set()` skipped) and `doctor::tests` checks
`CHECK_NAMES` against `cli.md`. Sources of truth are exported, not copied:
qq-config gains `DOCUMENT_FIELD_NAMES` / `POLICY_FIELD_NAMES` (a unit test
holds each equal to the list serde's derive reports in RON's unknown-field
error, and parses a document that sets every key), `ENVIRONMENT_VARIABLES`
(`from_process_env` destructures it by position, so it cannot drift; a
child-process test round-trips every variable), and
`provider_credential_variables()` (derived from the presets; xAI's variable
pinned as `XAI_API_KEY_VARIABLE`); qq-tui gains `slash_names()`.
`install.sh` `${QQ_*}` names are parsed from the script text. Gaps found:
one — the `qq jev observe` flags were only in the runbook; `cli.md` now has
a `qq jev` table. Allow-lists: `QQ_RELEASE_BASE_URL` (installer test hook);
the CLI flag allow-list is empty. `cargo xtask release X.Y.Z` now prepends a
`## X.Y.Z — date` section to `CHANGELOG.md` from `git log <newest v*
tag>..HEAD --format=%s --no-merges` (`xtask/src/release/changelog.rs`: 5
deterministic tests for parse/render/prepend; smoke-tested `--no-commit`
against the real history: 10 entries). Tests added: qq-config 4, qq bin 7,
xtask 5. Gates: fmt, clippy `-D warnings`, `cargo test --workspace`, `cargo
xtask release --help` green. Docs: `runbooks/release.md` (changelog step and
section), `runbooks/website.md`, `guide/cli.md`. OB12 marked shipped (#156).
No hot-path or protocol change.
