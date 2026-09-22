# Ledger — Onboarding UX

Plan: [`../onboarding-ux.md`](../onboarding-ux.md). Only the agent working
this plan edits this file. Current state on top; dated entries appended
below, newest last.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| OB0 | Audit, plan, user guide, community files, P0 error text | In review | `feat/onboarding-ux` | ENG-859 shipped separately as #119 |
| OB1 | TUI opens without a model | Planned | | ENG-860 |
| OB2 | TUI opens without a credential; empty state names the remedy | Planned | | |
| OB3 | Request-time credential errors name provider and remedy; `GOOGLE_API_KEY` alias | Planned | | |
| OB4 | `qq doctor` | Planned | | |
| OB5 | `qq init`; `config paths` marks existing files | Planned | | |
| OB6 | `install.sh`, Homebrew tap, Nix package, binstall | Planned | | |
| OB7 | In-TUI trust prompt | Planned | | needs ADR + protocol row in root |
| OB8 | First-session guidance; `qq run` denial hint | Planned | | |
| OB9 | Missing MCP credential degrades the server | Planned | | ENG-861 |
| OB10 | Docs-truth test; CHANGELOG at release | Planned | | |
| OB11 | Wiki mirror workflow | Planned | | |

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
