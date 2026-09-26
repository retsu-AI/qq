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
| OB7 | In-TUI trust prompt | Shipped (#161) | `feat/eng-881-tui-trust-prompt` | ENG-881; ADR-0042, no protocol change |
| OB8 | First-session guidance; `qq run` denial hint | Shipped (#146) | `feat/eng-882-first-session-guidance` | ENG-882 |
| OB9 | Missing MCP credential degrades the server | Shipped (#148) | `fix/eng-861-mcp-credential-degrade` | ENG-861 |
| OB10 | Docs-truth test; CHANGELOG at release | Shipped (#160) | `feat/eng-883-docs-truth` | ENG-883 |
| OB11 | Wiki mirror workflow | Superseded by OB12 | | ENG-884 |
| OB12 | Docs website from `docs/guide/`, GitHub Pages | Shipped (#156) | `feat/eng-896-docs-website` | ENG-896 |
| OB12.1 | Replace Windows placeholder-copy action with download/setup links | In review | `fix/eng-906-windows-install` | ENG-906; bounded follow-up, not a reopening of shipped onboarding slices |

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

### 2026-09-24 — OB7 in-TUI trust prompt in review

Branch `feat/eng-881-tui-trust-prompt` off `main` (#156). Decision in
ADR-0042: client-side prompt fed by root-computed data, **no protocol
change** (`PROTOCOL_VERSION` stays 28); the plan's "needs a protocol
addition" note is superseded. `qq-config`: `ConfigLoader::pending_trust`
(read-only scan shared with `grant_pending_trust` via
`scan_pending_trust`), `PendingTrust::declarations()` built by
`Document::sensitive_declarations` (`TrustDeclaration`: route, provider
name+kind, MCP name+command/url, grant counts, pack ids; never a secret,
argument, or env value), and `LoadRequest::with_process_trust(Vec<ProcessTrust>)`
admitted into the in-memory `TrustState` at load (no write). Root:
`RuntimeFactory::trust_for_process` / `process_trust` (mutex on the inner),
applied in `request_for_workspace`; `PlanKey.process_trust:
Option<ProcessTrustFingerprint>` (SHA-256 of sorted path+digest) so a
pre-grant compile is never served after `s`; `resolve_trust(Persist |
Session)` runs the same `grant_pending_trust` as `qq trust` or the process
grant, then reloads and recomputes `TuiModelState` (extracted from
`interactive()` so startup and post-trust agree). `interactive()` matches
`TrustRequired` only on its own load, opens the TUI with
`TuiOptions.pending_trust`, and passes a `TrustResolver` that refuses with
"run `qq trust` on the server host" when `server::reserve` returned
`Existing`. `qq-tui`: `Mode::Trust` (after overlays, before approval),
`t`/`s` → `Effect::ResolveTrust`, `q`/Esc → quit, other keys swallowed;
`trust_block` in the empty transcript (approval-block style),
`ComposerMode::Trust` placeholder `✎ Answer the trust prompt above`, rule
`F1 help` only; `apply_trust_resolved` installs model/catalog/remedies,
notices `trusted N file(s)` / `trusted for this session`, and re-requests
`Capabilities` + the new client-internal `ClientRequest::Models`. Tests: 2
qq-config, 3 qq bin (plan key, resolve both choices, plan-cache slot), 5
qq-tui (2 app, 1 view, 2 loop). Gates green: fmt, clippy `-D warnings`,
`cargo test --workspace` (qq-config 100, qq bin 229, qq-tui 328). Manual
pty smoke in an isolated `HOME` with `.qq/config.ron` declaring a model, an
HTTP MCP server, and one grant: the block paints with three declaration
lines; `s` clears it, shows `trusted for this session`, the OB2 remedy
takes over, no `trust.ron`; `t` writes `trust.ron` with the file's digest
and the next launch does not prompt; `qq ask` still exits with the
`TrustRequired` text. Docs: `guide/permissions.md`, `guide/tui.md`,
`guide/troubleshooting.md`, `design/tools.md`. Also marked OB12 shipped
(#156). No hot-path change: the scan runs once per prompt on the blocking
pool. Follow-up: a remote client with ADR-0015 enrollment could be offered a
server-side trust command.

### 2026-09-24 — follow-ups (ENG-897, ENG-898) in review

Branch `chore/eng-897-eng-898-onboarding-followups` off `main`. ENG-897:
every tool-using run on `google/*` failed with HTTP 400 `Unknown name
"additionalProperties"` because the Google codec sent each `ToolSpec`
schema verbatim and Gemini's `Schema` is a restricted OpenAPI subset.
`ToolSpecInner` now caches a Gemini-shaped schema in a `OnceLock`
(`gemini_parameters()`, `qq-provider/src/model.rs`) that strips the
unsupported keywords (`additionalProperties`, `$schema`, `$ref`, `$defs`,
`oneOf`, `allOf`, `const`, `exclusiveMinimum`, … ) from the root and every
schema under `properties`, `items`, and `anyOf`; only `FunctionDeclaration`
consumes it, so OpenAI and Anthropic bodies are byte-identical and
equality/`wire_size_hint` stay on the original text. Four built-in `enum`
properties in `qq-core/src/tools/specs.rs` gained `"type": "string"`, which
moves the built-in schema fingerprint golden in `tools.rs`
(`built_in_tool_declarations_keep_their_order_and_schema_identity`).
Tests: `model.rs` 2 (computed once and pointer-shared across clones;
nothing-to-strip is text-identical), `google.rs` 1 (nested
`additionalProperties`/`$schema`/`oneOf` gone from the captured
`parameters`, still present in the OpenAI and Anthropic bodies). Bench
`provider_encode` google: 389–404 us/iter before, 404 us/iter after (steady
state unchanged; the first request parses each schema once), body 1139321 →
1138393 bytes. ENG-898: `ToolHostSummary.message` was carried to the TUI
and read by nothing. `App` now raises the reason as a warning on the
composer rule when a capability document arrives, once per distinct message
(re-fetching the same document does not re-nag; a healthy document clears
the memory so a later regression warns again), and `/skills` shows `MCP:
<reason>` under its search row. Tests: `app/tests.rs` 1, `view/tests.rs` 1
(+1 negative assertion). Docs: `guide/providers.md` google row,
`guide/troubleshooting.md` new HTTP 400 entry, `guide/mcp.md` § When a
server is unavailable. Housekeeping: `website/tsconfig.json` comment says
when to revert the inlined preset (a nub release after 0.9.3; not yet);
`onboarding-ux.md` gains a "Candidate guides" table for the eight dropped
site routes, all not scheduled; `AGENTS.md` Linear team `DEV` → `ENG` and
branch examples; `runbooks/local-dev.md` recommends `CARGO_TARGET_DIR` for
`.worktrees/` (no repo `.cargo/config.toml`, which would redirect CI caches;
`nix/dev-shells.nix` does not set it). Gates: fmt, clippy `-D warnings`,
`cargo test --workspace`, `cargo test -p qq-provider --no-default-features
--features test-support`.

### 2026-09-25 — plan complete; site layout shift fixed

Every slice is shipped: OB0–OB10 and OB12 (OB11 superseded by the site);
follow-ups ENG-897 (Gemini schemas) and ENG-898 landed as #162. Of the
twenty audit findings O01–O20, all are closed; O05 ("`qq trust` grants
blind") is closed by OB7's declaration list in the TUI and by
`qq trust`'s own `declares:` output from OB0.

Lighthouse on the live site (desktop preset, three pages): landing
100/100/100/100; docs pages 100 accessibility, best practices, and SEO
but performance 85–94 from a layout shift of 0.19–0.29 on cold loads,
traced to the web font arriving after first paint: Inter's fallback
(Arial) breaks lines differently, and the fixed right sidebar is
positioned from `--sl-content-width`, which was `76ch` — a unit that
changes with the font. Fixed in `fix/eng-926-site-layout-shift`: the
content width is `50rem`; `scrollbar-gutter: stable` so the first layout
and the final one agree on viewport width; a metric-matched `Inter
Fallback` `@font-face` (`size-adjust`, ascent/descent overrides against
Arial) so lines break identically before and after the swap; and the
three Latin font files copied by `sync-docs` to `public/_fonts` under
stable names and `<link rel=preload>`ed from the head, so they arrive
with the HTML rather than a round-trip after the CSS. Verified locally
against the built `dist/`: five consecutive runs of `/docs/install/` at
CLS 0, and 100 in every category on `/`, `/docs/install/`,
`/docs/configuration/`, `/docs/tui/`. Method note: a CDP probe sampling
the sidebar rect never reproduced the shift because the probe's own
observer delayed first paint past the font load; Lighthouse's trace was
the reliable instrument.

What worked in this plan: one audit up front (O01–O20) that every slice
cited; small slices with one ledger, merged in stacks so conflicts were
only ever in this file; the docs-truth test and the generated site
landing together, so the guide is now load-bearing. What to carry
forward: per-worktree `target/` directories filled the disk once
(`runbooks/local-dev.md` now recommends a shared `CARGO_TARGET_DIR`);
the ADR-first rule for OB7 cost one research pass and saved a protocol
bump. Owner setup still open: none — the Homebrew tap
(`retsu-AI/homebrew-qq`, formula at v0.1.4) and `HOMEBREW_TAP_TOKEN` are
in place. Candidate guides (eight topics) are listed in the plan and
unscheduled.

### 2026-09-26 — OB12.1 Windows install entry follow-up

ENG-906, branch `fix/eng-906-windows-install`, implementation `4da55cb3`
on `bd8c580b`. The Windows tab copied a shell comment containing an
unresolved version placeholder. It now links to the latest release and
the existing Windows checksum/setup guide; the other four copy commands
are unchanged. Source and rendered behavior were independently approved.
Node 24.19.0/nub 0.8.3 site build passed (478 internal links); the focused
check fails on the retained live baseline and passes on the candidate.
Isolated Chrome fixtures at 1280px and 390px verify keyboard links and all
four clipboard payloads. This is not a native Windows installation test.
Pinned Rust 1.97.1 pre-push gates passed: formatting, strict all-targets /
all-features Clippy, workspace tests (1908 passed, 5 explicitly ignored),
workspace build, exact-test guard (10 fixtures), and installer (17 checks).
Tests ran serially with credentials removed from the inherited environment,
canonical `/private/tmp`, `NO_COLOR` unset and two build jobs. No Rust,
dependency, runtime policy or analytics change; no native Windows runtime,
deployment or hosted-CI claim. Retained evidence is in the manager's
`reviews/retsu-weekly-audit-2026-09-25/marketing-delivery-20260926/` packet.
