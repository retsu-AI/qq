# Side questions: `/btw` without interrupting work

## Status

- Tracking: [ENG-1011](https://linear.app/retsu-ai/issue/ENG-1011).
- Design only; no side-question command or runtime is shipped.
- Ledger: [progress/side-questions.md](progress/side-questions.md).
- Delivery follows this plan before [goals.md](goals.md); goals do not depend
  architecturally on side questions.

## Agreed behavior

`/btw <question>` and `/ask <question>` are aliases for one non-interrupting
side conversation. They never submit a steering prompt, cancel or pause the
main run, acquire its session execution slot, or mutate its goal.

Each question captures the current authorized session context and run status
at admission, not a moving view of later main-agent messages. The answer and
side-tool results belong only to the side thread: they never enter the main
transcript, compaction summary, goal notes or goal budget. The model receives
an explicit notice distinguishing captured context from live workspace reads.

The initial UX continues the existing side thread; **New thread** explicitly
starts another. Each question refreshes the captured main-session context.
Closing the pane hides it; cancelling its answer affects only that answer.
Sharing with the main agent is out of scope initially; ordinary prompts remain
an explicit way to steer work.

## Authority and limits

Reuse the existing core agent runtime and provider interfaces; do not build a
second agent implementation or use an ordinary child that blocks its parent.
Use the session's selected model with a separately compiled least-authority
plan. A side query has no main-session mutation/control tools.

The initial allowlist is built-in workspace file reads, directory listing and
content/name search. It excludes shell (including apparently read-only commands),
edits, writes, network tools, MCP tools, delegation, approval decisions and goal
controls. Enforce the allowlist at execution as well as model advertisement;
unknown tools fail closed. Ordinary workspace confinement and managed hard
denies still apply. A tool's generic read-only annotation is not sufficient.

Proposed shipped ceilings, independently configurable downward:
- one active question per source session; a second submission returns typed busy;
- global side concurrency: `max(1, max_active_runs / 4)`, separate from main slots;
- 8 model turns, 120 seconds wall time including permit waits, 16,384 output tokens;
- 32 KiB captured main context, 32 KiB retained side history and 8 KiB question;
- bounded tool output and event queues using existing tighter runtime limits.

Never truncate a tool-call/result pair into an invalid provider history. Reject
an oversized question; shorten captured context on complete-message boundaries
with an explicit omission notice. Context capture must not hold a synchronous
lock across await or copy an unbounded transcript. Track known cost, tokens and
uncertain interrupted spend separately, without hiding it in aggregate session
cost. No automatic restart/retry of an interrupted side query or uncertain tool
call. Durable completed history survives reconnect; recovery marks active work
interrupted rather than replaying it.

Read-only access does not imply a consistent filesystem snapshot. Reads may
observe edits in progress; results carry file hashes where available. Side
queries yield to any workspace-exclusive completion check. Questions during a
check can answer captured context but must not bypass workspace exclusion.

## Slices and stack

| Slice | Scope / owned areas | Acceptance |
| --- | --- | --- |
| SQ0 | This plan, goals delivery order, ledgers | Agreed aliases/context/authority recorded; stack and gates explicit; no runtime claims |
| SQ1 | Runtime: `qq-core` side-query admission/store/scheduling, bounded context, restricted tools; protocol commands/events/snapshot; shared client reducer; root composition as needed | Scripted main run continues while side response streams; main transcript/request/goal unchanged; one-query and global bounds; injected forbidden tools rejected; context captured once; cancellation/timeout isolated; cost separate; reconnect and crash recovery deterministic; persisted events precede publication |
| SQ2 | `qq-tui`, client port, guide/config surfaces | Both aliases reserved and equivalent; side pane follows separate stream; New thread and independent cancellation; reconnect states rendered; docs-truth and TUI snapshots; no steering prompt emitted |

SQ1 is one runtime PR, with acceptance subsets SQ1.1 etc. recorded in the
ledger if necessary, not placeholder layers. Any wire change uses its own
explicit protocol bump; G0 retains the single goal-specific bump. SQ1 needs an
independent sessions/authority review. Record context-capture latency and main
streaming latency before/after on a scripted concurrent run; no avoidable main
latency or unbounded allocations. Use the performance recording runbook and
record raw evidence under `target/qq-perf/`, never in Git.

The requested stack order is **SQ0 → SQ1 → SQ2 → G0 → G2 → G3 → G4 → G5**.
Each PR targets its immediate predecessor branch until that predecessor merges;
then rebase and retarget. Do not merge children first. G1 remains dropped.
G4's read-only panel could follow G0 but stays together after G3 initially.
G5 is an evaluation receipt, not permission to spend without ENG-809's budget.

Before each implementation slice: confirm dependency heads and current ledger
ownership, capture applicable baseline, create/update its Linear issue, add
failing behavior tests, then implement. Run narrow tests followed by workspace
format, lint, tests and build before pushing. PRs use the slice template and
state exact evidence and outstanding acceptance; incomplete work stays draft.

## Non-goals

Arbitrary tools, edits, shell execution, external mutations, multiple concurrent
side questions per session, automatic sharing with the main agent, independent
model selection, or consuming a goal's budget.
