# Gate — Phase 5b (headless contract, HC1–HC4)

Recorded retroactively 2026-09-16 for `main` at `43caaea` (#34, HC4 merged
2026-09-13); re-run on `c7fd5c4`. Lead.

## Commands

```sh
cargo test -p qq-protocol --test headless_fixtures   # goldens v18 (decode-only), v19, v20
cargo test -p qq-protocol --test wire_fixtures       # v17–v19 decode-only, v20 current
cargo test -p qq --bin qq -- headless                # `qq run` JSONL contract
cargo run -- config check                            # model-less
```

## Acceptance

| Item | Evidence | Result |
| --- | --- | --- |
| `--correlation`, `--session` resume behind a per-store owner lock | HC1 #30 `abad2de`; ADR-0022; `PROTOCOL_VERSION` 18 | Pass |
| `--output-schema` / `--output-repair-turns`; typed `FinalOutput` on `RunFinished`, snapshot, outcome | HC3 #33 `24b6e5c`; ADR-0014; `PROTOCOL_VERSION` 19, schema 27 | Pass |
| Headless record types in `qq-protocol`; golden JSONL per protocol version with framing checks | HC4 #34 `43caaea`; ADR-0023; `tests/fixtures/headless/v{18,19,20}/` | Pass |
| Optional `policy.exposed_tools` intersects across layers; grants cannot restore an excluded tool | HC2 `893e582` | Pass |
| No supervisor-only mode; QQ acquires no new authority; new fields additive | Reviewed per `headless-contract.md` boundary rules | Pass |
| Every gap row in `headless-contract.md` reads Shipped with its commit | `headless-contract.md` gap table | Pass |

## Not tested

- HC3's enabled-path schema compilation/validation/repair cost was measured in the HC3 receipt (`target/qq-perf/hc3-2026-09-12/`) but not re-recorded here.
