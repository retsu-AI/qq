# Review checklist

Report **blocking** items first, then **should-fix**, then **nits**, each with
`file:line`. Do not rewrite the code; if the *spec* is wrong, escalate through
`docs/plans/progress/decisions-needed.md`.

## 0. Setup

- [ ] PR head checked out in an isolated worktree; workspace gates run
      locally and green.
- [ ] Read the slice section in the plan, `AGENTS.md`, and the design docs the
      slice names.

## 1. Spec conformance (blocking if failed)

- [ ] Every acceptance item is demonstrated by a test or command in the PR
      body, and I ran it.
- [ ] Every named performance gate has a before and after measurement in the
      ledger, recorded per `docs/runbooks/perf-recording.md`; tail gates have a
      same-binary A/A control.
- [ ] Nothing outside owned paths changed except the ledger and named docs.
- [ ] No scope creep; no drive-by refactors or formatting churn.
- [ ] Inputs actually merged; no dependence on unmerged branches.
- [ ] Design docs amended in this PR; ADR present when required and uses the
      template with a real alternatives table.
- [ ] `PROTOCOL_VERSION`, `DESCRIPTOR_VERSION`, store schema, and fixture
      changes are declared and tested when wire or persisted meaning changed.

## 2. Standards conformance

### Rust and runtime

- [ ] `#![forbid(unsafe_code)]` retained; stable idiomatic Rust.
- [ ] Errors are domain enums with actionable variants and preserved sources;
      no `Box<dyn Error>` in library interfaces; no `map_err(|_| …)` that
      erases a source without a stated reason.
- [ ] Expected failures handled explicitly; `?` used only where propagation is
      the correct behavior and context is preserved.
- [ ] Exhaustive `match`; no silent fallbacks.
- [ ] No synchronous lock across `.await`; no blocking I/O on Tokio workers;
      blocking work is bounded and cancellable.
- [ ] Every queue, channel, task, cache, retry, output, and concurrency
      dimension is bounded.
- [ ] No provider-name branch in request or stream hot paths.
- [ ] No unnecessary allocation, clone, box, or dynamic dispatch on the
      streaming or request path.
- [ ] No `mod.rs`; module layout is sibling file plus directory.
- [ ] Helpers extracted only for reuse or a meaningful interface.

### Invariants

- [ ] Persist-before-publish preserved; a failed write never presents as
      durable.
- [ ] Retries remain provider-owned; no second retry owner.
- [ ] Tool approval derives from catalog effect, never from name matching.
- [ ] Secrets never enter descriptors, digests, events, traces, snapshots, or
      diagnostics.
- [ ] Settlement follows execution teardown; child ownership is retained
      across failure and cancellation.
- [ ] Tool paths stay within the workspace; destructive actions require
      approval.

### Tests

- [ ] Regression test for every bug fix; focused tests for new behavior.
- [ ] Tests assert public behavior and failure modes, not private trivia.
- [ ] Deterministic; no live network or real credentials.
- [ ] Concurrency, cancellation, bounds, and replay/idempotency cases present
      when the change touches those guarantees.

## 3. Security quick pass (blocking if failed)

- [ ] No credentials, `.env`, local absolute paths, or generated evidence
      committed.
- [ ] New SQL is parameterized.
- [ ] No new network egress from tools or context sources without explicit
      capability.

## 4. Verdict

`Approve` | `Request changes` | `Escalate`, with a two-to-four-line summary the
implementer can act on.
