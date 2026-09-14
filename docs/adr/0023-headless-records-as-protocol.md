# ADR-0023 — Headless JSONL records are protocol types, pinned by golden streams per `PROTOCOL_VERSION`

**Status:** Accepted
**Date:** 2026-09-13
**Deciders:** speed-first plan HC4 (headless contract)
**Implements:** [`headless-contract.md` § Gaps](../design/headless-contract.md#gaps-a-supervisor-currently-works-around)
(pinning the contract), [`headless-contract.md` § Compatibility Policy](../design/headless-contract.md#compatibility-policy)

## Context

`qq run --format jsonl` is the boundary a supervisor, CI job, or evaluation
harness builds on. Through HC3 its record shapes (`trial`, `event`,
`outcome`) were a private `serde` enum in the binary's `headless.rs`,
documented by a table in `headless-contract.md` and exercised only by tests
that read `serde_json::Value` fields. A consumer had to re-read QQ source at
every bump to learn what changed, and nothing in the repository failed when a
field was renamed, reordered, or silently defaulted. The wire protocol proper
already had the fix: `qq-protocol` owns every shape and
`tests/wire_fixtures.rs` pins byte-exact goldens per `PROTOCOL_VERSION`.
HC1 and HC3 each changed the records (`correlation`, `output_*`,
`final_output`) and bumped the version without a golden holding the result.

## Decision

1. **The record shapes are protocol vocabulary.** `HeadlessRecord`
   (`Trial | Event { envelope } | Outcome`), `HeadlessTrial`,
   `HeadlessOutcome`, `HeadlessStatus`, and `HeadlessApproval` live in
   `qq_protocol::headless`. Identifiers and hashes are their protocol types
   (`WorkspaceId`, `ContentHash`), not strings; `HeadlessStatus` carries the
   exit table (`code`) so the mapping is one definition. Decoding is strict:
   unknown `type` tags and unknown fields fail.

2. **The binary emits through a borrowing view.** `HeadlessRecordRef<'_>`
   mirrors the enum over references so the streaming path serializes a
   `SessionEventEnvelope` without cloning it; a unit test pins that both
   views encode identically. `src/headless.rs` re-exports `HeadlessStatus`
   and `HeadlessApproval` and owns no record shape of its own.

3. **Golden streams pin the contract per version.**
   `crates/qq-protocol/tests/fixtures/headless/v<PROTOCOL_VERSION>/` holds
   complete `.jsonl` trial streams, one per exit status plus the default
   payload, every optional trial field, and both `final_output` verdicts.
   `tests/headless_fixtures.rs` decodes each line as a `HeadlessRecord`,
   re-encodes it, requires byte equality, and checks framing: one trailing
   `outcome` whose `exit_code` agrees with `status`; unless startup failed
   before a session existed, one leading `trial` with only events between and
   strictly increasing cursors. `QQ_UPDATE_FIXTURES=1` rewrites the current
   directory after an intentional change; earlier directories are never
   rewritten and must still decode (`historical_streams_still_decode`).

4. **The records bump with `PROTOCOL_VERSION` and only additively.** A new
   field is optional and omitted when absent, so a stream an older binary
   wrote is a valid current stream. Changing an existing field's meaning or
   spelling is a version bump with new goldens, and the old directory stays.
   The headless tests in the binary decode every stdout line as a strict
   `HeadlessRecord` and require it to re-encode identically, so the binary
   cannot drift from the crate.

## Consequences

- Positive: a supervisor depends on `qq-protocol` (or on the fixtures) rather
  than on the binary's source; a rename or reorder fails a test in the PR
  that makes it; the exit table has one definition; the `v18` streams
  document exactly what HC3 added.
- Negative / risks: `qq-protocol` now names a CLI-only surface. It is bounded
  to the three record shapes and two enums; invocation options, text-format
  rendering, and approval behavior stay in the binary. The owned
  `HeadlessRecord` boxes its payloads to keep the enum small; the binary
  never constructs it.
- Follow-ups: the harbor adapter (`benchmarks/harbor`) parses these records
  in Python from its own fixtures; it can regenerate them from the goldens
  instead of `make_fixtures.py` when it next changes.

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| Keep the shapes in the binary; add goldens there | The binary's tests run a real runtime per case and cannot pin a byte-exact stream without a scripted provider per status; the protocol crate already has the golden discipline and no runtime |
| Version the JSONL contract separately from `PROTOCOL_VERSION` | Two numbers for one consumer to track; `event` records already carry the protocol's envelope, so the stream cannot be older or newer than the events inside it |
| Record fixtures by running the binary | Timestamps, ids, and provider text vary per run; normalizing them is more code than constructing the records, and the goldens would pin a transcript rather than the shapes |
| Derive the record from `serde_json::Value` schemas | A schema language is a second source of truth the Rust types already are |

## Evidence / references

- `crates/qq-protocol/src/headless.rs` — types, borrowing view, unit tests
  (owned/borrowed parity, fail-closed decoding, omitted optionals, exit table).
- `crates/qq-protocol/tests/headless_fixtures.rs`,
  `crates/qq-protocol/tests/fixtures/headless/{v18,v19}/` — goldens.
- `src/headless.rs` — `HeadlessRecordRef` emission; `parse_records` in its
  tests requires strict round-trip.
- Introducing commits: `17e00d4` (types), `6372cbd` (goldens), on
  `feat/hc4-headless-goldens`.
