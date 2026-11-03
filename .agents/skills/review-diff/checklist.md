# QQ review checklist

The invariants a generic review misses. Work through every section the diff
touches; name the sections you skipped.

## Persistence and events (`qq-core::sessions`, `store`)

- Is every event persisted in the same transaction as the state it
  describes, and published only after that transaction commits? Look for a
  `publish`/`send` before a `commit`, or between two writes that should be
  one transaction.
- Does a retried command replay its stored receipt instead of re-executing?
  A new command needs a `command_id` path through the journal.
- Does a failure between write and publish leave state that recovery handles?
  Check `open` / recovery marks the abandoned row.
- Are `synchronous=NORMAL` uses limited to the documented reservation
  transaction, with `FULL` restored before return?

## Concurrency (any `async`, `spawn`, `Mutex`, channel)

- `std::sync::Mutex` guard alive across an `.await`? Blocker.
- Blocking I/O (`std::fs`, `read_dir`, `canonicalize`, SQLite) on a Tokio
  worker outside `spawn_blocking`? Blocker.
- New channel or queue: what is its bound, and what happens at the bound
  (backpressure, typed refusal, or drop)? A drop needs a stated reason.
- New spawned task: who owns it, who cancels it, and what does the shutdown
  path wait for?
- `select!` on a cancellable future: does the dropped branch leave
  side effects the transcript must record (an interrupted tool result, a
  killed process group)?

## Bounds (external input, provider output, tool results)

- Every `String`/`Vec` built from provider, MCP, filesystem, or HTTP input has
  a byte or count ceiling before it is stored or forwarded.
- Truncation happens at a UTF-8 boundary (`is_char_boundary` or
  `truncate_utf8`), never a raw byte index.
- New constant limits are `pub const`, documented, and, if a client needs
  them, advertised in `ServerCapabilities`.

## Plan and catalog (`qq-core::plan`, `catalog`, `workspace`)

- Anything read at compile time that can change on disk is fingerprinted
  into the plan's stale check; a warm `plan_for` still does zero directory
  listing and zero host round trips.
- Descriptor changes bump `DESCRIPTOR_VERSION` and regenerate the golden
  digest; the descriptor stays secret-free (no secret values, hashes of
  secrets, or live handles).
- New static tool: added to the effect classification, the catalog digest,
  and the exposure decision; static tools are never excluded.

## Protocol (`qq-protocol`)

- Inbound types keep `deny_unknown_fields`; outbound response/capability
  types tolerate unknown fields.
- New optional field has `#[serde(default, skip_serializing_if = ...)]` and a
  sentence in `docs/design/protocol.md` version history.
- Would an existing client fail to decode? Then `PROTOCOL_VERSION` bumps and
  goldens move to a new `fixtures/vN/` directory.

## Provider (`qq-provider`)

- No provider identity branch in the request hot path; differences live in
  the compiled recipe.
- Retry stays inside `qq-provider`; nothing above it retries provider or tool
  calls a second time.
- Feature-gated code compiles in the minimal profile.

## Tools and approval (`qq-core::tools`, `approval`)

- New tool: classified read-only / mutating / shell / external; paths
  resolved through the workspace capability; output bounded.
- Approval is decided by name and mode, never by host-supplied hints.
- Shell changes preserve process-group kill on cancel.

## Errors

- New failure: a `thiserror` variant with source preserved, not a string.
- `match` is exhaustive; a `_ =>` arm on a domain enum is a finding unless
  the enum is `#[non_exhaustive]` from another crate.
- No `unwrap`/`expect` on data from outside the process.

## Tests

- Regression test present for a bug fix; it fails without the fix (ask the
  author to confirm, or reason from the assertion).
- Failure path tested, not only the success path.
- No live network, real credentials, fixed ports, shared temp paths, sleeps
  used as synchronization.
- Fixture or golden churn is inspected, not regenerated blind.

## Docs and hygiene

- `docs/design/*.md` updated when a boundary, bound, or wire shape moved.
- No `mod.rs`. Doc comments on new public items. No narration comments.
- Commit message is Conventional Commits with a focused scope; `!` present if
  a wire or on-disk format changed.
