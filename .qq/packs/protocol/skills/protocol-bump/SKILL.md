---
name: protocol-bump
description: Change a QQ wire type, plan descriptor, or store schema safely: every serialization site, version constant, golden, fixture, and doc that must move together. Load before editing qq-protocol, plan/descriptor.rs, or sessions/store/schema.rs.
---

# Protocol, Descriptor, And Schema Changes

Three things in QQ outlive the process and are pinned by tests: the wire
(`qq-protocol`), the plan descriptor (`qq-core::plan::descriptor`), and the
SQLite schema (`qq-core::sessions::store::schema`). A change to any of them
touches a fixed list of sites. Do them all in one commit or the tests will
tell you, one at a time, in the slowest possible order.

## Step 0: decide which versions move

| Change | Bumps |
| --- | --- |
| New optional field on an outbound type (`default` + `skip_serializing_if`) | nothing; add to version history as additive |
| New enum variant an old client would receive | `PROTOCOL_VERSION` |
| New required field, renamed tag, removed variant, changed cursor format | `PROTOCOL_VERSION` (and `!` on the commit) |
| New field in `ServerCapabilities` | nothing (`CAPABILITIES_VERSION` only for shape changes) |
| Anything in `AgentPlanDescriptor` | `DESCRIPTOR_VERSION` + `DIGEST_DOMAIN` + golden digest |
| Prompt text or ordering of system prompt sections | `AGENT_PROMPT_VERSION` |
| New column, table, or index | schema version (next integer) + migration |

Current values: `PROTOCOL_VERSION = 15` (`crates/qq-protocol/src/lib.rs:53`),
`CAPABILITIES_VERSION = 1` (`capabilities.rs:17`), `DESCRIPTOR_VERSION = 3`
with domain `qq-agent-plan-descriptor-v3\0` (`crates/qq-core/src/plan/descriptor.rs:13-16`),
`AGENT_PROMPT_VERSION = 9` (`crates/qq-core/src/runtime/prompt.rs:12`), schema
`21` (`crates/qq-core/src/sessions/store/schema.rs`, the last
`UPDATE metadata SET value = '21'`). Verify these before trusting them; this
file goes stale.

## Wire change checklist (`qq-protocol`)

1. **The type.** Inbound (`SessionCommand`, `InputPart`, requests): keep
   `#[serde(deny_unknown_fields)]`. Outbound (`ServerInfo`,
   `ServerCapabilities`, snapshots): no `deny_unknown_fields`. New optional
   field: `#[serde(default, skip_serializing_if = "Option::is_none")]` (or
   `Vec::is_empty`). Doc comment on every new pub item.
2. **Re-export** from `crates/qq-protocol/src/lib.rs` if the type is public.
3. **Goldens.** Add or extend a case in
   `crates/qq-protocol/tests/wire_fixtures.rs` that exercises the new field
   with a non-default value (a default value proves nothing). Then:
   ```sh
   cargo test -p qq-protocol --test wire_fixtures            # expect failure
   QQ_UPDATE_FIXTURES=1 cargo test -p qq-protocol --test wire_fixtures
   git diff --stat crates/qq-protocol/tests/fixtures/       # read every changed file
   cargo test -p qq-protocol --test wire_fixtures            # now green
   ```
   If `PROTOCOL_VERSION` bumped: `git mv` the directory
   `fixtures/v14` → `fixtures/v15`, update the path constant in
   `wire_fixtures.rs`, and update every golden's `protocol_version`.
4. **Harbor fixtures.** `benchmarks/harbor/tests/fixtures/*.trace.jsonl`
   carry `protocol_version`; `benchmarks/harbor/tests/make_fixtures.py`
   generates them. Update the version and regenerate if the trace shape
   changed.
5. **Server and client.** `crates/qq-server/src/lib.rs` for routes and
   capability assembly; `crates/qq-client/src/lib.rs` for typed calls. A new
   command needs a route, a `SessionCommandKind` entry, and a capabilities
   `commands` entry.
6. **Consumers of the shape.** `grep` the field or variant name across
   `src/`, `crates/qq-tui`, `crates/qq-core`, and `xtask`. Exhaustive matches
   on the enum will fail to compile; that is the point.
7. **Docs.** `docs/design/protocol.md`: the `PROTOCOL_VERSION` block, one
   sentence in the version history paragraph stating what an older client
   sees, the route or type section, and the `fixtures/vN/` path near the end.
8. **Tests in the root crate.** `src/runtime.rs` and `src/headless.rs` assert
   `prompt_identity.version` and the descriptor prefix; update if those moved.

## Descriptor change checklist (`qq-core::plan::descriptor`)

1. Add the field to `AgentPlanDescriptor` (and its nested structs) in
   declaration order; the canonical encoding is declaration order.
2. Bump `DESCRIPTOR_VERSION` and change `DIGEST_DOMAIN` to match
   (`...-v4\0`). Both, always.
3. Populate it in `CompiledAgentPlan::compile_*` and in the test
   `golden_descriptor()` (`crates/qq-core/src/plan.rs`, ~line 850) with a
   fixed non-default value.
4. Run `cargo test -p qq-core plan::` and copy the new golden digest from the
   failure into the assertion. Record the digest in the commit body.
5. Confirm the field is secret-free: no values from `auth`, no `sha256(secret)`,
   no handles. Names and references only. The
   `descriptor_is_secret_free` style tests must still pass.
6. Update `RunPlanIdentity.descriptor_version` examples in
   `docs/design/protocol.md` and the architecture paragraph in
   `docs/design/architecture.md` that lists descriptor contents.
7. `cargo bench -p qq-core --bench plan_compile` and note
   `descriptor_canonical_bytes` before/after.

## Schema change checklist (`qq-core::sessions::store::schema`)

1. Add the next version step at the end of the migration chain: a guard of
   the form `if schema_version != Some("N")`, the `ALTER`/`CREATE` statements,
   and `UPDATE metadata SET value = 'N'`, all inside the existing transaction.
   Also add `"N"` to every earlier `matches!(..., Some("18" | "19" | ...))`
   guard so those steps skip on a current database.
2. New columns are nullable or have a default. Never rewrite existing rows
   to mean something new; if old rows cannot be interpreted, store an explicit
   unknown marker (see `version_one_migration_..._marks_historical_cost_unknown`).
3. Add a migration test beside the others in `crates/qq-core/src/sessions.rs`
   (search `fn version_ten_migration_`): build a database at version N-1 by
   hand, open it, assert the version is N and the old rows read correctly.
4. Every `INSERT`/`SELECT` that touches the table: update the column list.
   `grep` the table name.
5. If the column carries JSON (`*_json`), the type it decodes must tolerate
   the previous shape (`#[serde(default)]`), because rows written by the old
   build stay in the file forever.
6. Update the schema number in the receipt table of the current plan
   document and the "schema version" mention in `docs/design/protocol.md` if
   present.

## Before you finish

- `cargo test -p qq-protocol && cargo test -p qq-core plan:: && cargo test -p qq-core migration`
- Full gates: `cargo xtask check --full` (see the `qq-mode` skill).
- Commit message: `feat(protocol)!: ...` if the wire version moved,
  `feat(protocol): ...` for additive; body states the compatibility
  consequence in one sentence and the new golden digest if the descriptor
  moved.
- Report three lists: changed on the wire, changed in the descriptor, changed
  on disk. Each with its version consequence.
