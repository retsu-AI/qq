# Decisions needed

Append-only. Any agent or reviewer may add a row; only the lead resolves.
Reference an entry in code as `TODO(decision:<n>)`. Take the conservative
default and keep working; do not block on a resolution unless the slice cannot
proceed without it.

| # | Raised by | Date | Question | Conservative default taken | Resolution (lead) |
| --- | --- | --- | --- | --- | --- |
| 1 | ROOT-1 | 2026-09-08 | Phase 5a full H0 tail gates fail on the shared host in both A/B and A/A. Accept Phase 5a on focused gates plus medians, or hold for a quiet-host run? | Phase 6 proceeds; the quiet-host run stays an open ledger row (`5a-accept`) and is not waived | Phase 6 first (user, 2026-09-08); quiet-host run remains required before the 5a row is marked complete |
| 2 | ROOT-1 | 2026-09-08 | The plan's "Trend At Phase Boundaries" table dropped Phase 1–3 columns during compression. Restore them? | Dropped; recoverable from `1bae5c2` | |
| 3 | ROOT-1 | 2026-09-08 | Native Windows: is the targeted `windows-teardown` CI job sufficient for H23 acceptance, or is a full Windows workspace run required before Phase 6 closes? | Targeted job counts as "passes in CI"; full run stays an open ledger row (`5a-windows`) with no claim | |
| 4 | ROOT-1 | 2026-09-08 | HC1 (`u32` turns) and HC3 (`final_output`) each need a `PROTOCOL_VERSION` bump. Land together as one bump (17) or separately (17, 18)? | Separate unless both are ready in the same integration window | |
