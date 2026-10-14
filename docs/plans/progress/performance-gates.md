# Performance gate repairs

| Slice | Goal | Status | Branch/PR | Evidence |
| --- | --- | --- | --- | --- |
| ENG-834 | Repair inherited size/retry failures and qualify unchanged speed limits | In progress | `fix/eng-834-performance-gates` | User authorizes follow-up after #306 merged; ceilings remain 48/41 MB |

## 2026-10-14 — baseline and ownership

- Merged input: `9868038c994cc0c0b64795731d1402ca21ff6348` (#306).
  Root is the sole writer in `.worktrees/perf-eng-834-gates`; shared-file
  ownership is requested in the root ledger.
- Hosted prior-head H0 run `38646957393`, artifact `11934628926`: main
  `62c65930` default/minimal sizes 52,961,016/46,294,328 bytes exceed the
  unchanged 48,000,000/41,000,000 ceilings. AB6 adds 32,864/24,672 bytes.
  Candidate passes relative limits; same-binary control fails HTTP-ack and
  one-subscriber fanout tails. Retry baseline/candidate/control are 6,000
  milli, the ADR-0040 expectation; external budget remains 1,050 milli.
  No failed gate is waived. Current AB6 H0 run `38649895250` remains active.
- Pre-change optimized local size diagnostic at exact AB6 source `e0d0fb94`
  (same source tree as merged input, except merge identity): symbol-preserving
  build and separate stripped copy retained under the AB6 worktree's
  `target/qq-perf/ab6-2026-10-14/size-diagnostic/`. Linked parser symbols total
  10,545,999 bytes, of which 10,024,360 are static data. These diagnostic
  sizes are not hosted qualification. Fat-LTO experiment remains live;
  no production profile change yet.
