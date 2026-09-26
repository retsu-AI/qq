# Ledger — MCP tool-set pinning

Tracking: [ENG-939](https://linear.app/retsu-ai/issue/ENG-939).
Owner: QQ, maintainer-directed takeover on 2026-09-25.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| MP1 | Reviewed descriptor pins, dispatch enforcement, durable identity and inspect UX | In review | `feat/eng-939-mcp-pinning-stack` | One integration PR supersedes #163/#165/#171; no journal migration; ADR-0046 |

## 2026-09-25 — takeover and baseline

Base `dc07e29`, rebased onto `ac28bf2` (#172, #191; no overlap with these
files). Original heads preserved as `refs/qq-review/pr-{163,165,171}`.
Maintain attribution for the reused MCP contribution, but derive requirements
from QQ's contracts. Event hashing is deferred: no current consumer/checkpoint/
retention agreement; approval authority receipts are a separate design.
Reserve ADR-0046 (0042/43 are on main, 0044/45 in #187); descriptor version
will change for the pin. Shared-file request: Cargo.lock (existing SHA-256),
doc indexes, architecture, CLI coverage. Main worktree remains untouched.
Baseline `cargo bench -p qq-core --bench plan_compile`: median 24,183 ns
compile, 2,308 ns descriptor digest (5 repeats). Raw baseline and executable:
`target/qq-perf/eng-939/`. Shared host, informational measurement, no tail claim.
Regression-first: queued drift, reconnect, invalid/unlisted tools, cancellation;
then descriptor identity, inspection and full workspace/CI gates.

## 2026-09-25 — MP1 implemented

Ported only the #171 MCP delta (`qq-mcp`, `qq-config` pin key, `src/mcp.rs`
readiness text) via three-way apply; no audit-chain file. Then:

- Digest domain `qq-mcp-tool-set-v1` → `v2` with length-prefixed fields; the
  contributed `\0`-separated encoding was not injective.
- Listing normalization fails the whole listing on an empty, oversized,
  non-`[A-Za-z0-9_.-]`, or duplicate tool name (previously skipped before
  hashing); bounded paginated fetch: 32 pages, 512 tools, 1 MiB, under the
  20 s deadline; a `list_changed` during a fetch discards it.
- Dispatch enforcement: `Listing { tools, digest, client, generation }`
  resolved before the permit, generation/dirty re-check after it, tool must
  be in the listing, single `tools/call` polled under a synchronous
  `dispatch_gate` shared with `on_tool_list_changed` / `bump_generation` /
  `shutdown`; `McpCallFailure::InvalidResult` → `HostCallError::InvalidResult`.
- Durable identity: `McpServerDescriptor.pin`, `DESCRIPTOR_VERSION` 9 → 10,
  fixture digest `bbce3842…e223`, `mcp_servers.pin` in the sensitivity list.
- Inspect UX: `McpManager::inspect`, `ClientSnapshot::mcp_servers`,
  `qq mcp inspect NAME` (blocking config load, one server's credential, JSON
  report with an untrusted-descriptors warning, ctrl-c and 45 s bound).
- Docs: ADR-0046; `tools.md` § MCP; `architecture.md` plan identity;
  `protocol.md` descriptor note; guide `mcp.md` § Pinning and `cli.md`
  `qq mcp inspect`; indexes and decision 10.

Red-first regressions (failed on the ported #171 code, green after):
`queued_pinned_call_refuses_drift_before_dispatch` (`None` vs
`Some(Quarantined)`), `pinned_call_cannot_execute_an_unlisted_tool` (`None`
vs `Some(UnknownTool)`),
`pinned_listing_refuses_duplicates_instead_of_hashing_a_subset` (`None` vs
`Some(Unavailable)`). Six further tests cover replacement connections,
cancellation/shutdown of waiters, notification during discovery, inspection of
a quarantined server, listing bounds, and invalid names.

## 2026-09-25 — MP1 gates (worktree, rebased on `ac28bf2`)

- `cargo fmt --all -- --check`: pass.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: pass.
- `cargo build --workspace --all-targets --locked`: pass.
- `cargo test --workspace --locked`: 1,911 tests; one failure at default
  parallelism (`sessions::tests::delegation::child_mutation_drains_before_steering_or_a_replacement_run_can_write`,
  a 2 s `Elapsed` on the shared host; unrelated to MCP), passes `--exact` and
  in a full `--test-threads=4` run (every crate green: qq 243, qq-core 731,
  qq-mcp 27, qq-config 105, qq-provider 228 + 19, qq-tui 330, …).
- `cargo test -p qq-provider --no-default-features --features test-support`:
  181 + 19 pass (minimal embedding profile; `Cargo.lock` touched).
- `cargo bench -p qq-core --bench plan_compile`, 5 repeats after vs baseline:
  compile median 24,673 ns (baseline 24,183), descriptor digest median
  2,477 ns (baseline 2,308); canonical descriptor 1,371 bytes (was 1,369).
  The `pin` adds one optional field to the descriptor; the difference is
  within this shared host's run-to-run spread (baseline itself ranged
  23,340–30,406 ns). Raw: `target/qq-perf/eng-939/{baseline,after}.txt`.
- Guide coverage (`cli::tests::every_subcommand_and_long_flag_is_documented_in_the_guide`)
  passes with `qq mcp inspect` documented in `docs/guide/cli.md` and
  `docs/guide/mcp.md`.

Not covered here: a live pinned server in the TUI (the fixtures are the
in-process `rmcp` pair and the HTTP fixture), and an independent review of
the dispatch-gate design — requested on the PR.


## 2026-09-26 — Codex review on #194

One P2 finding (`lib.rs` byte bound): `serde_json::to_vec(tool)` buffered a
full encoded copy of each descriptor before the 1 MiB check. The claim that
the bound "does not constrain peak memory" overstates it — `rmcp` decodes a
page before we see it and the stdio transport has no line limit (HTTP SSE
events are capped at 16 MiB by `rmcp`), so the transport layer, not this
check, sets the peak — but the second copy was real and avoidable. Replaced
with a counting `ByteBudget` writer (`serde_json::to_writer`), which rejects
the write that crosses the budget and allocates nothing;
`descriptor_bytes_are_counted_without_an_encoded_copy` covers it. A transport
read limit is an `rmcp` configuration question left out of this slice.