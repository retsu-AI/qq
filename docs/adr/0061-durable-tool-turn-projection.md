# ADR-0061: Persist the tool-turn projection before the next request

- Date: 2026-10-14
- Status: Proposed (action-bias AB2, ENG-1039)
- Supersedes: Reconstruction of tool-turn projection solely from per-call rows

## Context

A tool call's complete bounded result differs from the text actually sent to
the model after turn budgeting and checkpoint review. Changing the projector
can therefore change replay without changing the persisted assistant turn or
its per-call results. Prompt 20 was published with several projectors; that
prompt identity alone cannot identify which text a historical model saw.

## Decision

1. The core runtime emits one internal projection barrier after all tool
   outcomes and checkpoint annotations are complete. It contains the ordered
   model-facing replies and the optional correction notice immediately after
   them. Session execution awaits durable persistence before polling the
   runtime stream again and sending the next provider request. A failed write
   stops/drains the run and settles a persistence failure.
2. Schema 49 adds nullable `model_turns.tool_results_projection_json`, after
   receipt schema 47. All earlier guards recognize 49 and still validate their
   structures. AB5's notice-only migration follows at 50. No wire change is
   needed. Experimental schema 48 belongs to the published AB5 notice build;
   this AB2 reader refuses it because it lacks that notice codec. The final
   AB5 schema 50 reader migrates both 48 and 49 with the notice codec present.
   The barrier is internal, and direct runs retain their existing live
   projection without session persistence.
3. The write validates run/turn ownership, at most 64 replies, ordered IDs
   exactly matching the assistant ToolCall blocks, and one result per call.
   IDs are at most 1,024 bytes; aggregate result text is at most 96 KiB;
   correction notice is separately bounded to 32 KiB. The reply flags/text
   include checkpoint consequences and need not equal the earlier per-call
   row. Replies and notice commit together. An identical retry succeeds;
   a conflicting retry fails without overwriting the first projection.
4. The serialized ceiling is 2 MiB, checked before writing and enforced by
   SQLite. Sixfold JSON escaping of 96 KiB replies, 64 KiB IDs and 32 KiB notice
   exceeds 1 MiB; 2 MiB leaves bounded room for framing. Replay charges stored
   JSON against its assembly allocation budget before copying/decoding it.
5. Descriptor 15 records explicit strict projection identity. Historical
   descriptor 13/14 canonical domains and omitted-field encodings remain
   unchanged. Live output uses the compiled policy. Replay uses a stored
   exact projection verbatim, preserving effects, order and correction notice.
6. A known historical pre-AB2 descriptor 10–14 with matching identity and
   prompt 1–19 uses the preserved original generic
   projector, including its 4 KiB remainder floor. An incomplete future turn
   before the barrier is recoverable using its explicit strict policy and
   interrupted results. That reconstruction is distinct from a committed
   projection. Ambiguous unversioned history requires immutable policy
   evidence or an actionable unresolved-projection result; present code or
   configuration cannot assign its history retrospectively.

## Consequences

One additional bounded write per tool turn preserves the exact next-request
context, including checkpoint annotations. Its latency and allocation impact
must be measured in the existing turn/store/context gates. Failed persistence,
rollback, retry, escaping bounds, ordered-ID validation, reopen, and crash
boundaries require tests. Historical bytes that were never recorded cannot
be recovered merely by adding a new descriptor version.
