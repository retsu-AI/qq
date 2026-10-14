# ADR-0058 — Edit freshness: a unique exact match proves the region was seen

**Status:** Accepted (2026-10-13, #281)
**Date:** 2026-10-11
**Deciders:** lead; second reviewer required (changes a safety rule)
**Implements:** [`tool-layer.md`](../plans/tool-layer.md) § D10, T19 (ENG-1030). Amends [`tools.md`](../design/tools.md) § Edit Semantics and § Optimistic Concurrency step 2

## Context

Before this decision, `edit_file` required a recorded read of the file's
current content in the same session, or an `if_hash` matching it. Without
one it refused with `not_read`, or with `stale_file` when a recorded read was
of other content. The D10 audit (2026-10-08 → 2026-10-11, v0.1.6) found 9 of
61 `edit_file`/`write_file` calls refused this way. In every traced case the
model had seen the text: through shell `grep`/`sed -n`, in its own earlier
edit, or because it had just rewritten the file with its own `python3`
script (two `stale_file` refusals came straight after one). Over the same
window there were 59 shell writes against 51 `edit_file` calls. A refusal
costs a `read_file` round trip and pushes the model toward
`python3 - <<EOF` and `sed -i`. Those are held for approval, invisible to the
diff UI, and bypass the matching cascade's checks. The freshness rule is
meant to stop the model overwriting text it never saw. A required read is
not the only evidence of that.

## Decision

An edit to a file with no recorded read of its current content and no
`if_hash` applies when its `old` occurs **exactly once, byte for byte,
counting overlapping occurrences**
(`Strategy::Exact`). It must be unique both in the content read at plan time
and in the text as earlier edits in the batch left it. The header says
`note=unread` (no recorded read) or `note=changed_since_read` (a recorded
read of other content), joined with other notes by a comma.

Still refused:

- an anchor-only insert (`insert_before`/`insert_after` with no `old`);
- an `old` that matches only through the fuzzy cascade, an absent `old`,
  and a repeated `old`, including under `replace_all`. These return
  `not_read`/`stale_file`, not `ambiguous`/`not_found`;
- a `write_file` overwrite of an existing file: no `old` vouches for what
  it replaces;
- an `if_hash` that does not match. The model asserted a version, and every
  `if_hash` in a batch is checked.

A batch fails whole. One qualifying file does not carry another.
Refusals do not name the file's current hash, and that includes an `if_hash` mismatch on `edit_file` or `write_file`. Passed back as `if_hash`, it
would apply the refused fuzzy edit or overwrite on the next call without the
model seeing any content. The message names the cheapest fix instead: extend
`old` until it is exact and unique, or read the lines around the edit. The
`if_hash` path itself is unchanged (see Consequences). Phase 2 is unchanged:
under the exclusive section each file is re-hashed against its plan-time
hash, and an applied edit records the new hash in `FileState`.

`Freshness` in `crates/qq-core/src/tools/edit.rs` encodes the three states.

## Why a unique exact match is evidence

To produce an `old` that occurs once, the model must reproduce the exact
bytes it replaces, and those bytes identify one place in the file. It cannot
have guessed that span without seeing it somewhere. What it may not have
seen is the rest of the file, but the edit does not touch the rest. A
fuzzy match shows the model's copy differs from the file. A repeated `old`
does not say which copy it saw. An anchor names where to insert, not what
is there. None of these meet the bar, so they keep the old rule.

## Consequences

- Positive: the traced refusals apply without a round trip. Models have
  less reason to edit through the shell. A refused edit can usually be
  retried at once by extending `old`.
- Concurrent sessions: suppose session B has read a file and session A
  then changes it. B's edit now applies if its `old` still occurs once in
  A's content. That is a clean rebase of a disjoint change, and
  `note=changed_since_read` tells B. Before, B was refused and re-read.
  Other edits from B still fail `stale_file`. A writer that lands between
  B's plan and apply still fails B's edit, because the apply re-hashes
  against the planned bytes. A change is never applied over content it
  was not matched against.
- Negative / risks: an edit can now land in a file whose other regions
  changed since the model last looked. The edit itself is still correct for
  the text it names, but the model's wider picture may be stale. The note
  makes that visible. A short, common `old` that happens to be unique (for
  example `x = 1`) is weak evidence. The rule accepts it because the
  replaced bytes are what the model named.
- Unchanged weakness: `if_hash` still proves currency with no content seen
  (`read_file mode=info` returns `h:`, and 44 stored calls sent the
  placeholder `h:000000000000`). This decision neither widens nor narrows
  it; decisions-needed row 12 asks whether to.
- Follow-ups: the D10 rerun after T18–T20 (`not_read`+`stale_file` below
  3 %, a falling shell-write to `edit_file` ratio).

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| Keep the rule; improve the refusal text only | Still a round trip per refusal, and the audit shows the round trip is what pushes models to the shell |
| Record a read when shell output shows file text | Parsing `grep`/`sed` output to infer file state is fragile and does not cover the model's own `python3` rewrite |
| Accept any match the cascade finds | A fuzzy or ambiguous match is the case where the model's view differs from the file |
| Return the current `h:` in every refusal (the first D10 draft) | The hash is the whole proof: the next call could pass it as `if_hash` and apply the refused fuzzy edit or overwrite blind |
| Drop freshness entirely | Loses the guard against overwriting text the model never saw, and `write_file` would overwrite whole files blind |

## Evidence / references

- `crates/qq-core/src/tools/edit.rs` `edit_file` (phase 1 freshness check,
  phase 2 re-hash); `crates/qq-core/src/tools/write.rs` refusals.
- Regressions in `tools/edit.rs` tests: the traced shapes (a)–(i) of the
  plan, including `a_write_between_plan_and_apply_is_stale_and_not_clobbered`
  and `a_duplicate_old_on_an_unread_file_is_not_read`.
- D10 audit: [`progress/tool-layer.md`](../plans/progress/tool-layer.md)
  2026-10-11.
