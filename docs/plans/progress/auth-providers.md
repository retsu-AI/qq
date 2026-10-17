# Ledger — auth and provider credentials

Plan: [auth-providers](../auth-providers.md). Linear: ENG-1066.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| AP0 | Plan and ledger | In review | `docs/eng-1066-auth-provider-plan`, #341 | Five Codex review passes on the plan (27 findings), all addressed |
| AP1 | Shared OAuth helpers | In review | `refactor/eng-1066-ap1-oauth-helpers`, #343 | |
| AP2 | One provider operation lock | In review | `refactor/eng-1066-ap2-refresh-lock`, #345 | |
| AP3 | Audiences defined once | In review | `refactor/eng-1066-ap3-audiences`, #346 | |
| AP4 | State and backends split out of `lib.rs` | In review | `refactor/eng-1066-ap4-store-module`, #347 | |
| AP4b | Request-credential perf fixtures | Planned | | Baseline for AP5/AP6 |
| AP5 | One request-credential provider + bench | Planned | | |
| AP6 | Header-set request credentials | Planned | | |
| AP7 | Table-driven built-in providers (ADR) | Planned | | |
| AP8 | Provider-neutral OAuth grants | Planned | | |

## Log

2026-10-16 — Plan opened from the qq-auth/qq-provider review. Baseline
`origin/main` `3e34eea2`. Vendor terms fetched the same day (see plan).

2026-10-17 — AP1–AP4 opened as a stack. Tests: qq-auth 77 → 79 (AP2 adds
the lock-path and cross-store serialisation tests; the second fails with
the operation lock removed from `set_with_metadata`); `cargo xtask smoke`
passes on each head. Stack rebased onto `origin/main` `a9bff3ef`
(#330–#332); each PR's own diff has the same `git patch-id` before and
after. Open: `cargo xtask check` fails on
`headless::tests::closing_goal_host_drains_accepted_work_without_admitting_successors`
from #331, which also fails on the plan-only branch with no Rust change;
main CI at `a9bff3ef` is red on `goal_model_audit_late_and_stale_confirmation_cannot_achieve`.
Both belong to ENG-1063/ENG-1064.
