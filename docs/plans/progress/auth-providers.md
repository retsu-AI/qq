# Ledger — auth and provider credentials

Plan: [auth-providers](../auth-providers.md). Linear: ENG-1066.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| AP0 | Plan and ledger | Shipped (`cdbe4a5c`) | #341 | Six Codex review passes (31 findings); the last four fixes (`fe1fa5c7`) were not re-reviewed after Codex hit its usage limit |
| AP1 | Shared OAuth helpers | Shipped (`4c0c4829`) | #343 | |
| AP2 | One provider operation lock | Shipped (`92554c96`) | #345 | |
| AP3 | Audiences defined once | Shipped (`332e8a62`) | #346 | |
| AP4 | State and backends split out of `lib.rs` | Shipped (`b1c66be4`) | #347 | |
| AP4b | Request-credential perf fixtures | In review | `perf/eng-1066-ap4b-credential-fixtures`, #351 | Baseline recorded (30 samples, MAD ≤ 12%); `fixture_version` 6 |
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

#### AP0–AP4 receipt — 2026-10-17
Commits: `cdbe4a5c` (#341), `4c0c4829` (#343), `92554c96` (#345),
`332e8a62` (#346), `b1c66be4` (#347), merged in that order.
Tests: qq-auth 77 → 79; PR CI (affected gate) green on each head; smoke
8/8 after the rebase. Codex 👍 on the merged heads of #343–#347.
Deviations: AP4 splits into `state.rs` and `backend.rs` rather than one
`store.rs`. AP3 keeps root stored-name literals until AP7.
Docs: `docs/design/providers.md` (AP3), `docs/design/architecture.md`
qq-auth source map.
Open: AP4b next.

#### AP4b receipt — 2026-10-18
Commits: `112fbde9`, `b29fdd47` (#351).
Tests: `perf::tests::credential_fixtures_resolve_the_stored_bearer_without_refresh`;
the bearer check fails with a wrong expected token (0 of 6 requests).
Gates: `cargo xtask check` (qq, xtask) 440/440. Local smoke
`cargo xtask perf baseline --machine-class local-smoke --samples 5
--warmups 0`: fixture 6, 80 metrics;
`xai_request_credential_resolution_ns` median 26.3 µs,
`xai_request_credential_concurrent_resolution_ns` median 137.4 µs,
`request_time_authorized_stream_ns` median 124.6 µs; both credential
checks pass. Five samples on a loaded dev host: execution receipt, not a
qualified baseline.
Evidence: target/qq-perf/ap4b-smoke.json (untracked).
Open: AP5 records its candidate against this fixture.

#### AP4b baseline receipt — 2026-10-18
Head `8180225b` (offline token clients, self-contained loopback).
`cargo xtask perf baseline --machine-class local-dev --samples 30
--warmups 3`: fixture 6, 80 metrics, 30 samples each.
- `xai_request_credential_resolution_ns`: median 23.3 µs, p95 26.0 µs,
  MAD 120 ns (0.5%).
- `xai_request_credential_concurrent_resolution_ns`: median 82.7 µs,
  p95 146.5 µs, MAD 5.1 µs (6.1%).
- `request_time_authorized_stream_ns`: median 52.2 µs, p95 93.6 µs,
  MAD 6.4 µs (12.3%).
All three are under the budget file's 50% MAD limit. Both credential
checks pass: 33/33 requests carried the stored bearer, 0 token-client
calls. This is the AP5/AP6 baseline; dev host, not a dedicated machine.
Evidence: target/qq-perf/ap4b-30-run2.json (untracked).
