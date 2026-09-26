# ADR-0046 — Pin an MCP server's advertised tool set in configuration and plan identity; enforce the pin at dispatch, not only at discovery

**Status:** Accepted
**Date:** 2026-09-25
**Deciders:** MCP pinning takeover (ENG-939), superseding the contributed
stack GitHub #163 / #165 / #171
**Implements:** [`tools.md` § MCP](../design/tools.md#mcp);
[`architecture.md` § compiled plan identity](../design/architecture.md);
ledger [`progress/mcp-pinning.md`](../plans/progress/mcp-pinning.md)

## Context

An MCP server's tool descriptors — names, descriptions, input schemas, and
hints — are untrusted text that the model reads as instructions, and the
server may change any of them at any time (a `list_changed` notification, a
redeploy behind an HTTP endpoint, a reconnect to a different binary). QQ
already freezes a run's catalog at admission (ADR-0004) and classifies
approval from the catalog's effect class rather than the server's hints
(ADR-0007), but nothing let an operator say *which* tool set they reviewed,
and nothing stopped a server from swapping descriptors between the review and
the call. The contributed stack proposed a `pin` (SHA-256 of the listing) with
quarantine on drift. Review of that stack found that a pinned call which
passed the pin check, then waited for the server's concurrency permit, still
executed after the server was quarantined; that the pin was absent from the
plan descriptor, so pinned and unpinned plans shared a digest; that a listing
with a malformed or duplicate tool name was silently reduced before hashing;
and that the only way to learn a digest was to configure a wrong pin and read
the error. A per-event audit hash chain shipped in the same stack had no
concrete consumer, checkpoint, or retention contract and conflicted with the
proposed ADR-0038 event deletion, so it is not adopted here.

## Decision

A server declaration may carry `pin`: 64 lowercase hex digits validated at
configuration load. `qq-mcp` reduces every listing to an `McpToolSetDigest`
(SHA-256 under the domain `qq-mcp-tool-set-v2\0` over the tools in name order,
each field length-prefixed: namespaced name, description, compact sorted-key
input schema, hints). A pinned server whose listing digests differently is
quarantined: it contributes no tools, `McpCatalog.quarantined` names both
digests, and every call to it fails closed as `McpCallFailure::Quarantined`
(`HostCallError::Refused`).

The pin is enforced twice. At discovery, as above. At dispatch, a pinned
call resolves the cached `Listing { tools, digest, client, generation }`
*before* acquiring a permit; after the permit it re-checks that no
`list_changed` is pending and that the listing generation is still current
(else it releases the permit and re-resolves), requires the tool to be present
in that listing (`UnknownTool` otherwise), and sends exactly one `tools/call`
whose enqueue poll runs under a synchronous `dispatch_gate` that
`on_tool_list_changed`, `bump_generation`, and `shutdown` also hold while
flipping their flags. A drift observed at the gate refuses the call; a
response other than `CallToolResult` is `InvalidResult`. A `list_changed`
during a fetch discards that fetch. Listings are taken whole or not at all:
at most 32 pages, 512 tools, and 1 MiB of descriptors, with any empty,
oversized, non-`[A-Za-z0-9_.-]`, or duplicate name failing the listing for
every server.

The pin is part of plan identity: `McpServerDescriptor.pin` is recorded and
`DESCRIPTOR_VERSION` moves 9 → 10 (digest domain
`qq-agent-plan-descriptor-v10\0`). Inspection is separate from admission:
`McpManager::inspect` returns the raw listing and digest without enforcing the
pin or calling anything, and `qq mcp inspect NAME` prints it as JSON with a
warning that the descriptors are untrusted and the digest grants nothing.

## Consequences

- Positive: an operator can review a server's descriptors once and have QQ
  refuse the server — every tool on it — when they change; a queued call can
  no longer execute against a tool set the pin did not cover; a run's durable
  identity names the pinned tool set; unpinned servers are unchanged; no
  protocol, store schema, or dependency change beyond `sha2` in `qq-mcp`.
- Negative / risks: the pin attests to *advertised* metadata only — a server
  can keep its descriptors identical and change its behavior, and the docs
  say so. The domain bump (`v1` → `v2`) means any pin computed by the
  unmerged contributed build fails loudly, which is intended. The dispatch
  gate is a `std::sync::Mutex` held only across a non-blocking poll and flag
  flips; it must never be held across an `.await`. Pinned calls forgo `rmcp`'s
  implicit continuation handling, so a server that answers `tools/call` with
  anything but a `CallToolResult` gets `InvalidResult` rather than a retry.
- Follow-ups: binding an approval decision to the exact call arguments and
  tool identity at `start_tool_call` (deferred; no minimal design agreed);
  a per-event audit chain only once a consumer, an external checkpoint, and
  the retention contract (ADR-0038) are settled; `qq doctor` could report
  quarantine like it reports unresolved bearers.

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| Per-tool pins instead of a per-server digest | A server that changed one tool cannot be trusted about the others; one digest per server is one review unit and one line of config |
| Pin only at discovery (the contributed design) | Reproduced: a call admitted before a `list_changed` executed after quarantine once it obtained a permit |
| Hash `\0`-separated fields (contributed `v1` domain) | Ambiguous when a field contains the separator; length prefixes make the encoding injective |
| Skip malformed or duplicate names and hash the rest | A pin that approves a silently reduced listing approves something the operator never saw |
| Learn the digest by configuring a placeholder pin and reading the quarantine error | Teaches operators to copy digests out of error text; `qq mcp inspect` shows the descriptors *before* they are pinned |
| Ship the per-event audit hash chain (#165) alongside | No consumer, checkpoint, or retention agreement; verifier accepted a cursor rewrite and a fresh-store unhashed row; pages bounded by count, not bytes |

## Evidence / references

- `crates/qq-mcp/src/lib.rs`: `TOOL_SET_DIGEST_DOMAIN`, `McpToolSetDigest`,
  `Listing`, `ServerHandle::{listing, fetch_listing, execute}`,
  `McpManager::inspect`; `crates/qq-core/src/plan/descriptor.rs`
  (`DESCRIPTOR_VERSION = 10`, `McpServerDescriptor.pin`);
  `crates/qq-config/src/document.rs` (`valid_mcp_tool_set_pin`);
  `src/mcp.rs` (`inspection_settings`, `inspect_server`).
- Tests (`crates/qq-mcp/src/tests.rs`):
  `queued_pinned_call_refuses_drift_before_dispatch`,
  `pinned_call_cannot_execute_an_unlisted_tool`,
  `pinned_listing_refuses_duplicates_instead_of_hashing_a_subset`,
  `queued_pinned_call_revalidates_a_replacement_connection`,
  `pinned_call_cancellation_and_shutdown_do_not_dispatch_waiters`,
  `a_notification_during_discovery_never_certifies_the_old_listing`,
  `inspection_exposes_quarantined_descriptors_without_authorizing_them`,
  `listings_bound_pages_tool_count_and_bytes_without_partial_pins`,
  `invalid_tool_names_are_not_omitted_from_a_pin_candidate`;
  `plan::tests::canonical_encoding_and_digest_are_stable` (v10 fixture).
- Review of the contributed heads `4dcb062` / `448d6bd` / `932f953` is
  recorded in [`progress/root.md`](../plans/progress/root.md) (2026-09-25).
