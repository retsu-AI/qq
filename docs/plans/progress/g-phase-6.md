# Gate — Phase 6 (speed-first)

Recorded 2026-09-16 on `main` at `c7fd5c4` (#47 merged). Lead.

## Commands

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace
cargo test --workspace                                        # 1,428 passed, 4 ignored
cargo test -p qq-provider --no-default-features --features test-support   # 169 passed
cargo bench -p qq-provider --bench provider_encode --features test-support
cargo bench -p qq-provider --bench sse_decode --features test-support
cargo bench -p qq-core --bench host_cancel_latency
```

## Acceptance, item by item

| Item | Evidence | Result |
| --- | --- | --- |
| Cancellation ≤100 ms with 256 queued control jobs; no site polls the store | H20 recording `893e582`; zero `sleep(1 ms)` loops (`rg`) | Pass: 23 / 27 ms med / p95 |
| Eight-stream service gap ≤20 ms under mixed load | H20, 30 interleaved pairs | Median 20 ms pass; **p95 33 ms open** (bimodal tail not reproduced by A/A; quiet-host recording pending) |
| Settling a settled run is a no-op on every path; every `PersistenceFault` variant reachable; teardown precedes terminal publication | `settling_a_settled_run_is_a_no_op_on_every_path`, `every_persistence_fault_variant_is_reachable`, `TeardownComplete` | Pass |
| Plan generations obey limits; rejected refresh leaves the previous generation; guards reclaimed; explicit config compared privately | H27 tests in `src/plan.rs` | Pass |
| Ninth context source fails with a typed error; source identity changes the digest | H28 tests; `DESCRIPTOR_VERSION` 6 fixture | Pass |
| 1 MiB request heap ≤2x; encode ≤10 ms; prefix-plus-suffix digest equals full | `provider_encode`: 1.39–1.60x shared, 190–410 µs; digest equality across four capability sets | Pass |
| 1 MiB / 512 KiB ratio ≤2.2x, improving on 1.892x | H0 fixture | **Open**: not recorded on a quiet host |
| H19 improves decoder allocation/latency or documents no-change | `sse_decode`: framing 0.21–0.23x, decode 0.40–0.42x | Pass |
| `sessions.rs` split changes no behavior, own commit | #44 token-multiset diff; 1,401 tests unchanged | Pass |
| H22 route-table equality test passes between client and server | `command_routes_match_the_protocol_table` | Pass |
| Default path within the H0 regression gate vs Phase 5a | — | **Open**: rides with the Phase 5a quiet-host comparison |

## Not tested

- Full native Windows workspace run (targeted `windows-teardown` job only; carried to Phase 7 per decision #3).
- H0 tail gates on a quiet host (shared-host tails are non-repeatable; retained, not waived).
- The seven H22 deferrals were not attempted; reasons in the plan § Bundled Fixes.
