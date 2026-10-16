# Side questions

A side question (`/btw` or `/ask` in the TUI) asks the session's model about
the work in progress without interrupting it. It never submits a steering
prompt, cancels or pauses the main run, takes the session's execution slot,
or changes its goal. Its question, answer, tool results and spend stay in the
side thread: none of them enter the main transcript, a compaction summary, or
a goal budget.

The runtime is `crates/qq-core/src/sessions/side_questions.rs`; the wire
commands, snapshots and projection updates are protocol 32
([`protocol.md`](protocol.md)); rows live in the `side_questions` table
(store schema 44). Usage is in the [TUI guide](../guide/tui.md#side-questions-without-interrupting-work).

## Context

Each question captures the session's committed context once, at admission,
in the same transaction that records the question. It does not follow later
main-agent messages. Captured context is at most 32 KiB and is shortened on
whole-message boundaries with an explicit omission notice; unfinished tool
exchanges are omitted so the side history is always a valid provider history.
The model is told which part is captured context and which part is live
workspace reads.

Questions continue the session's latest side thread unless the client starts
a new one. A continued thread replays earlier completed questions and final
answers only (at most 32 KiB, newest first; older exchanges are replaced by
an omission notice). Side tool calls and results are not retained.

## Authority

A side question runs on the core agent runtime with the session's selected
model and a separately compiled least-authority plan. Only the built-in
`read_file`, `search` and `tree` tools are advertised, further restricted by
the session's tool policy, and the same allowlist is enforced again at
execution: any other call is denied. Workspace containment and managed hard
denies apply as usual. Shell, edits, writes, network and MCP tools,
delegation, approval decisions and goal controls are unavailable.

Live reads are not a consistent filesystem snapshot; they may observe edits
the main agent is making.

## Bounds

| Bound | Ceiling |
| --- | --- |
| Active questions per session | 1 (a second submission fails busy) |
| Concurrent side questions per runtime | `max(1, max_active_runs / 4)`, separate from run slots |
| In-memory side tasks per runtime | 64 (beyond that, overloaded) |
| Question size | 8 KiB |
| Model turns | 8 |
| Wall time, including permit and ownership waits | 120 s |
| Output tokens | 16,384 |
| Tool output | 96 KiB |
| Answer | 128 KiB |

An embedder may lower each per-question ceiling (`SideQueryLimits`) and the
concurrency limit (`max_active_side_queries`), never raise them.

## Durability and recovery

Admission, partial answers (persisted at least every 1 KiB of streamed
growth), final state and cost are written before they are published, and
survive reconnects. Cancelling a side question affects only that question;
cancelling a finished one returns `side_question_already_finished` and
changes nothing. Known cost and tokens are recorded per question, separately
from session cost. A side question interrupted by a restart is marked
interrupted on recovery; it is never restarted or retried automatically.
