# Security Kernel

Status: in progress. Decision: [ADR-0042](../adr/0042-security-kernel-inside-qq.md).
Ledger: [`progress/security-kernel.md`](./progress/security-kernel.md).

Port the fail-closed guarantees of `retsu-AI/axiom-rs` ("the model may
propose; only the kernel may commit") into qq as library code in the crates
that already own the governed state. qq stays one embeddable binary with one
store; the supervisor above it (ADR-0009) owns sinks, tenancy, and scale.

Source of truth for the guarantees is Axiom's `ARCHITECTURE.md`. The port is
an adaptation, not a dependency: identities, signatures, sandbox profiles,
and tenancy are deliberately left out (see the ADR's alternatives).

## Non-Goals

- No `axiom-*` dependency, new crate, service, or socket.
- No signed capability tokens or external issuer keys.
- No change to the five approval modes or to what `Forbidden` and blocked
  hosts refuse; every slice may only *record more* or *refuse more*.
- Not A4–A6 of the integration plan (skills import, `/v1` façade, doctor);
  each is a separate plan if taken.

## Task Index

| ID | Goal | Status |
| --- | --- | --- |
| SK0 | ADR-0042, this plan, ledger | In review |
| SK1 | Hash-linked event history and exportable audit stream (A3) | Planned |
| SK2 | MCP tool-set digest pinning and drift quarantine (A2) | Planned |
| SK3 | Approval ledger: recorded authority, digest-bound commit, at-most-once execution (A1) | Planned |

Order: SK1 → SK2 → SK3. SK2 is independent in code but stacks on SK1 so the
ledger and schema-version edits never conflict; SK3 depends on SK1's store
migration step.

## Slices

### SK1 — Hash-linked events and audit export
**Inputs:** SK0
**Owned paths:** `crates/qq-core/src/sessions/events.rs`,
`crates/qq-core/src/sessions/store/schema.rs` (one appended step),
`crates/qq-core/src/sessions/store.rs`, `crates/qq-core/src/sessions/runtime.rs`,
`crates/qq-core/src/sessions/audit.rs` (new), `docs/design/architecture.md`
§ Persistence.
**Gates:** `persistence` bench unchanged within noise (one extra hash and one
`UPDATE workspaces` per event).
**Acceptance:**
- Schema 36 → 37 adds `events.previous_hash`, `events.record_hash`,
  `workspaces.audit_head`; a store from 36 opens, its old rows stay NULL, and
  new rows chain from the workspace genesis.
- `record_hash = sha256("qq-audit-v1\0" ‖ previous_hash_hex ‖ "\0" ‖ envelope_json)`;
  genesis `previous_hash = sha256("qq-audit-genesis-v1\0" ‖ workspace_id)`.
- `SessionRuntime::export_audit(workspace, after, limit)` returns records
  with the stored bytes and both hashes; `SessionRuntime::verify_audit`
  walks the chain and reports `Intact { records, unhashed_prefix }` or the
  first `sequence` that fails (`HashMismatch` / `LinkBroken`).
- Tests: chain continuity across restart; a row edited in place is reported
  with its sequence; a row deleted mid-chain is reported as a broken link;
  migration from 36 leaves the prefix unhashed and chains afterwards; export
  pages by sequence with a bounded limit.
**Docs:** `architecture.md` § Persistence (audit chain paragraph); ADR-0042
evidence.

### SK2 — MCP tool-set pinning and quarantine
**Inputs:** SK1 (ledger only)
**Owned paths:** `crates/qq-mcp/`, `crates/qq-config/src/document.rs` and
`lib.rs` (`pin` on the MCP server declaration), `src/mcp.rs`,
`docs/design/tools.md` § MCP.
**Gates:** none named (digest computed once per listing, off the call path
after the first use).
**Acceptance:**
- `McpToolSetDigest` = sha256 over the server name and the sorted, normalized
  tools (name, description, canonical input schema, hints); identical
  listings in different order give one digest.
- `pin: "<digest>"` on a server declaration. A pinned server whose listing
  digest differs is quarantined: `McpCatalog.quarantined` names it with
  expected and actual digests, its tools are absent from `tools`, and
  `McpManager::call` returns `McpCallFailure::Quarantined` for every tool on
  it — including one whose schema did not change — until the pin matches.
- An unpinned server behaves as today; `McpCatalog.servers` carries every
  connected server's current digest so an operator can pin it.
- A `list_changed` that drifts a pinned server quarantines it before the next
  call executes (the call path re-verifies against the cached listing).
- Tests: deterministic digest; drift quarantines catalog and call; matching
  pin passes; unpinned unchanged; invalid pin text rejected at config load.
**Docs:** `tools.md` § MCP Configuration (`pin`), § MCP (quarantine).

### SK3 — Approval ledger and commit gate
**Inputs:** SK1
**Owned paths:** `crates/qq-core/src/approval.rs`,
`crates/qq-core/src/sessions/approvals.rs`,
`crates/qq-core/src/sessions/tool_calls.rs`,
`crates/qq-core/src/sessions/store/schema.rs` (one appended step),
`docs/design/tools.md` § Approval Policy.
**Gates:** none named (one extra column write per executed call).
**Acceptance:**
- `approval::evaluate` returns `Execute(ExecuteAuthority)`: `ReadOnly`,
  `Mode(ApprovalMode)`, `ClassifierAllow`, or `Grant { kind, value, source }`
  naming the exact grant that covered the call and who recorded it (human,
  delegate, jev). Schema 37 → 38 adds `tool_calls.authorized_by` (JSON), set
  when the gate lets a call run without a hold; a held call's row keeps its
  `approval_resolution` and gains `authorized_by` on the approving verdict.
- `start_tool_call` binds the commit to the durable row: the `UPDATE` to
  `running` requires `name` and `arguments_json` to equal the call the
  executor is about to run; a mismatch fails closed with
  `SessionRuntimeError::ApprovalBindingMismatch` and the call is denied,
  never executed.
- At-most-once: a second `start_tool_call` for the same id, or a start after
  `denied`/`completed`, is refused; recovery after a crash mid-execution
  never re-enters `running` for the same row (regression test).
- Tests: authority recorded for each `Execute` path; delegate grant receipt
  names `source = jev`/`delegate`; binding mismatch denies; double start
  refused; existing approval tests unchanged.
**Docs:** `tools.md` § Approval Policy → "Approval ledger"; ADR-0007 and
ADR-0041 evidence appended (no decision change).

## Decisions Needed

Recorded in [`progress/decisions-needed.md`](./progress/decisions-needed.md)
when a slice raises one. Known at planning:

- Grant attenuation (expiry, use budgets) for delegate grants — Axiom has it;
  qq's delegate grants are session-scoped and unbounded in time. Not in SK3;
  needs a product decision on defaults.
- Anchoring the audit chain head outside the store (supervisor sink) — out of
  qq's boundary by ADR-0009; the export format is the contract.
