# Ledger — v0.1.8 release preparation

Plan: [`../release-0.1.8.md`](../release-0.1.8.md). One writer per file.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| R018-prep | Retire v0.1.7 publication docs, reconcile plans, record gates | In review | `docs/release-0.1.8-prep` | No version, changelog, pin or tag change |
| R018-gates | Close the plan's pre-bump gates | Open | — | Flake, host timing, size and ADR decisions |
| R018-bump | `cargo xtask release 0.1.8` with the drafted Upgrading block | Planned | — | Only after R018-gates |
| R018-publish | Tag merged main and verify assets/install routes | Planned | — | Then guide pins via `release --docs` |

## Entries

### 2026-10-18 — preparation

- Base: main `7750ec7f` (#333 merged). Contracts read from source:
  protocol 35, capabilities 1, descriptor 15, store schema 58.
- v0.1.7 publication verified and its temporary plan/ledger retired; receipt
  is in [root](root.md#2026-10-18--v017-publication-receipt-and-v018-preparation).
- Goals plan/ledger/README updated to the merged stack; ADR allocation table
  repaired (0062 row added, 0063 marked implemented, table no longer split).
- Release-size diagnostic, local `cargo build --locked --release` on a loaded
  host (load average about 20). Sizes are deterministic for the locked source;
  this is a diagnostic, not hosted H0 qualification:

  | Build | v0.1.7 (`dc41c332`) | main `7750ec7f` | Budget `maximum_p95` |
  | --- | --- | --- | --- |
  | Minimal (`--no-default-features`) | 40,957,104 | 43,866,448 (+2,909,344) | 41,000,000 |
  | Full (default) | 45,026,544 | 48,667,088 (+3,640,544) | 48,000,000 |

  Earlier main `a9bff3ef` (goals stack, before tracing and auth) measured
  43,041,968 minimal. v0.1.7 passed both ceilings; main now exceeds both. No
  budget is widened; the decision is a pre-bump gate.
- Local `cargo xtask check` on the loaded host reproduces three deterministic
  timeouts in goal/compaction/soak tests that hosted CI passes (plan gate
  G-host); the goal audit shutdown flake remains G-flake.
