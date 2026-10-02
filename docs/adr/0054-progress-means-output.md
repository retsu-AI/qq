# ADR-0054 — Progress means output: a run reports when it stops changing things, a sub-agent answers its brief, and delegation does not block the parent

**Status:** Proposed
**Date:** 2026-09-30
**Deciders:** lead; second reviewer required (run loop, sub-agent admission, tool catalog)
**Implements:** [`autonomous-core.md`](../plans/autonomous-core.md) AP1–AP5 (AP0 is the measurement runbook). Complements ADR-0049 § 8 (loop guard) and takes over ADR-0048 § 2's empty-checkpoint fault

## Context

The session store on the lead's machine has 30 days of real use. The numbers
come from the [`progress-report.md`](../runbooks/progress-report.md) queries,
for the 30 days ending 2026-10-01, on store schema 39. They show agents doing plenty of work but
producing almost nothing:

- **Sub-agents read and do not answer.** There are 85 child runs of 20 turns
  or more. None made a change, because children are read-only. In 41 of
  them, the first visible text was the final answer or there was none. In 22,
  a stretch of 128 or more executed calls passed with no text at all. The
  largest made 1 193 calls.
  - One child, which was asked to plan a slice, made 690 calls in 76
    minutes: 461 searches and 213 reads, 680 of them distinct. It was
    cancelled with nothing to show.
  - It read one file 47 times, in overlapping windows.
  - Of 124 `spawn_agent` calls, 34 returned an error rather than an
    answer. 12 of those were children that read until their context was
    over its limit.
- **Parents wait.** `spawn_agent` holds the parent's turn until the child
  settles (`lib.rs:3288–3298`, which awaits `subagents.rs:303`; the owner
  task's wait loop is `subagents.rs:558–606`). The 41 runs that delegated
  had a `spawn_agent` call open for 1 722 of their 4 351 wall minutes
  (40 %), with overlapping calls merged.
  - The v0.1.6 planning run was blocked for 209 of its 267 minutes.
  - Its first file edit came at minute 99.
- **The existing checkpoint is ignored.** At 256 calls, the slice checkpoint
  asks for "a concise checkpoint" through the system prompt
  (`lib.rs:106–122`, `1712–1731`). It fires once 241 or more calls
    have run, because the next turn may add 16.
  - The store has 30 runs that reached it. 20 of them answered it at least
    once with tool calls instead, across 25 checkpoint turns. Those calls
    are rejected (RR1) and the slice resets anyway. 23 of the 25 turns had
    no text, so they recorded no checkpoint. This count comes from the
    rejection results; reconstructing checkpoints from the per-turn call
    counts gives the same 23.
  - Changing the system prompt for one request also invalidates the provider
    prompt cache for that request and for the continuation that follows it.
- **Children get no brief about answering.** A child gets the root system
  prompt, including "Implement requested changes rather than stopping at
  analysis unless the user requested analysis-only work". A child has no
  user, so the exception never applies. Its first message is the parent's
  task text. Nothing tells it that only its final message reaches a
  waiting parent, or when to stop (`runtime/prompt.rs:181–209`,
  `commands.rs:115–121`).
- **Pruned reads invite re-reads.** A pruned `read_file` result loses its
  header. `header_line` looks for the tool name, but the read header starts
  with `read`, not `read_file` (`tools/output.rs:436–440`, `read.rs:409–411`).
  The stub then says "call it again if needed". If the model passes
  `if_changed_since` with a hash it remembers, the re-read returns no body
  at all (`read.rs:147–154`).

None of this is a spending problem. Capping cost or time would stop the
waste, but it would also stop the multi-day work the product is for. The
missing idea is **what counts as progress**. The runtime counts every new
read as activity, so hours of reading look healthy. ADR-0049's loop guard
does not change that, because it treats a novel call-and-result pair as
progress. It catches repetition, not a model that reads new things forever
without producing anything.

## Decision

1. **Progress is output, not activity.** A *progress event* is one of:
   - a successful `ToolClass::Mutating` or `ToolClass::External` call;
   - a `ToolClass::Shell` command not accepted by `read_only_shell_command`
     (`approval.rs:477`) that actually ran, **whatever its exit status**. A
     failing build or test is work. Unlike the audit trigger
     (`lib.rs:352–379`), a non-zero exit still counts;
   - a sub-agent's answer reaching the parent: a successful blocking
     `spawn_agent` result, or a delivered answer (decision 4);
   - an applied steer;
   - a *report* (decision 2) that contains text;
   - a goal checklist change, once ADR-0049's `update_goal` exists.

   Reads, searches, listings, history searches, spill reads and fetches are
   never progress, whether or not they are novel. Every run keeps
   `calls_since_progress`. It counts every settled call except runtime
   rejections (over the per-turn cap, made in a report turn, or rejected by
   the loop guard). Denied calls count, so a child that keeps asking for
   denied calls still reaches its report. The count resets on a progress
   event. It is harness state in a new *stall* reset scope, never a caller
   budget. Audit children (`SessionPurpose::Audit`) are exempt, because they
   are already bounded at 8 turns.
2. **A run that stops changing things reports.** When
   `calls_since_progress` reaches `STALL_REPORT_CALLS = 64`, the next
   request is a *report turn*.
   - It carries a runtime notice asking for what is established (with
     `path:line` evidence), what is still unknown, and the one next action.
   - The notice is appended as a message framed
     `[QQ runtime notice; not a user instruction]`. It is not appended to
     the system prompt, so the cached prefix is kept.
   - Tools stay declared, as RR1 requires. A call made in the report turn is
     admitted with a not-executed result, so the model can re-issue it next
     turn.
   - A report turn never settles the run, even when it has text and no
     calls. It takes the path the slice checkpoint takes today
     (`lib.rs:2729–2743`), so it never reaches the final-answer, audit or
     Jev final-review paths.
   - A report with text is a progress event. A report without text is a
     *missed* report. It is not a failure: it resets the count and is
     counted toward decision 3.
   - The notice a turn answered is persisted with the turn as a store
     column, `model_turns.notice`, on the first turn row whose request
     carried it: `report` (the slice checkpoint), `stall_report`,
     `continuation`, or `final_answer` (decision 3). Replay renders that
     fixed notice before the turn, the way it renders the truncation notice
     (`sessions/transcript.rs:1149–1151`), so live and restart assembly
     stay byte-identical. This is a store schema change (39 → 40); AP3b's
     two values landed before 40 shipped in a release.
   - The stall report asks for the same report as the slice checkpoint,
     under its own opening line ("The last 64 tool calls changed nothing and
     produced no answer"), so the model is told why it is reporting. A
     stall report leaves the slice count alone: its calls still ran in
     that slice.

   The 256-call slice checkpoint becomes the same kind of turn with the same
   notice, and its continuation notice moves out of the system prompt too.
   A run that keeps working therefore still records a checkpoint every
   256 calls. An empty checkpoint no longer fails the run with
   `provider returned an empty slice checkpoint`. It is a missed report,
   which replaces ADR-0048 § 2's "empty checkpoint → placeholder".
3. **A sub-agent answers its brief.** A child run counts reports, slice
   checkpoints included, since its last other progress event. After
   `MAX_CHILD_REPORTS_WITHOUT_WORK = 3`, its next report turn is the **final
   answer turn**.
   - The notice says that this reply ends the run and must answer the brief
     from what the child has.
   - **The turn settles the run whatever it returns.** That is the
     guarantee, not the model's obedience: models answered 23 of 25
     checkpoint turns with calls. A call made on it is admitted with a
     not-executed result and never runs, and the run completes once those
     results are durable. Jev final review, the audit hook and steering do
     not redirect it.
   - Its tools stay **declared**, with `ToolChoice::None` (ENG-1001): the
     request asks for no calls where the API can (OpenAI and Anthropic
     `tool_choice: none`, Gemini `mode: NONE`). *Amended 2026-10-01:* this
     decision first said the turn "declares no tools", like the budget-final
     turn. Native Bedrock Converse rejects any request whose history holds
     tool calls when it declares no tools, which also broke the
     budget-final turn on `bedrock/` (fixed in ENG-1001). Keeping the tools
     also keeps the cached tool prefix: the system text and tools are
     unchanged on the child's last turn.
   - The run completes with that reply. If the reply has no text, the parent
     receives the child's latest report, labelled as an interim report. That
     text is the child's own durable output, never runtime-written. If
     there is none, the parent gets today's "completed without producing
     any text" error.

   A read child cannot do work, so for it the rule reduces to "the fourth
   report is the final answer". That comes within about 4 × (64 + 15) = 316
   executed calls of pure reading. A write child that keeps making changes
   is not affected. A child whose own children keep delivering answers is
   making progress by decision 1. That is intended: an orchestrating child
   is bounded by its children's output.
   Root runs are never ended by this rule. They report and continue. If
   they stay stuck, they are paused by ADR-0049 § 8, or by the goal driver
   through its own rules.
4. **Delegation does not block the parent.**
   - A read-only `spawn_agent` returns as soon as the child is durably
     admitted, with the child's session id. A spawn beyond the concurrency
     cap (`MAX_CONCURRENT_CHILDREN_PER_RUN`) still waits for a slot, as it
     does today.
   - **Runs with a finite token or cost limit keep blocking spawns.** The
     run loop already serializes spawns in that case, because each child is
     granted the parent's whole remainder (`lib.rs:3455–3467`). Overlapping
     children would overspend it. Goal runs are always clamped, so they keep
     today's behaviour until the remainder can be split across children.
     That split is a follow-up, not part of this decision.
   - A settled child's answer enters the parent's context exactly once, as a
     runtime notice message at the parent's next turn boundary. A delivered
     mark on the child's spawn row and the notice are committed in **one
     transaction**, before the next request is built. Delivery has its own
     path and its own byte bound; it is not steering. It is not given to Jev
     as user input, and it does not use the steering queue.
   - The child's spend is charged to the parent exactly once, at delivery or
     at cancellation. Today it is charged when the tool call returns.
   - If the parent settles before delivery (interrupted, cancelled or
     failed), the undelivered answer is committed as a notice in the parent
     session. The next run's assembly includes it, whether that run is a
     prompt, a continuation or a goal run.
   - Two tools come with it:
     - `wait_agents { ids?, timeout_seconds }` blocks the turn until the
       named children, or any child, settle or the timeout passes, and
       returns their answers;
     - `cancel_agent { id }` cancels one child. The existing cancellation
       path applies, and the parent receives whatever report the child had.
   - A parent turn with no tool calls while children are running does not
     settle the run. The runtime waits for the next answer, delivers it, and
     runs another turn. The wait also wakes on steering, cancellation and the
     parent's deadline. A budget-final turn still settles the run: it cancels
     running children and charges their spend.
   - Write children stay blocking. There is one write slot, and
     `architecture.md`'s rule that a parent "cannot start another write
     while a supervised child is draining" is unchanged, so parent and child
     never edit concurrently.
   - Each child's interim reports are also delivered to the parent at its
     next boundary as bounded notices, so the parent can act on partial
     findings or cancel early. Only a final answer resets the parent's
     stall count; an interim report does not.
5. **The prompt states the contract.**
   - A child's system prompt gains a sub-agent section:
     - a parent is waiting, and only the final reply returns to it;
     - answer the brief's question, and stop as soon as it can be answered;
     - give the answer first, then the evidence as `path:line`, then open
       questions;
     - do not re-read text already in context.
   - The root-oriented "implement rather than stop at analysis" line is
     dropped for read children. The choice is keyed on being a child, not on
     the read-only tool filter, so a read-only root keeps the line.
   - The parent's delegation guidance says a brief must carry a question,
     what the answer is for, and the expected answer shape. It also says to
     prefer several narrow children over one broad one.

The constants are code constants, not configuration. They are re-measured
with the AP0 runbook and changed with evidence.

## Consequences

- **Contract changes.**
  - There is no `PROTOCOL_VERSION` change for decisions 1–3 and 5. A
    report is an ordinary assistant turn, so clients already render it.
  - Decision 2 adds a store schema bump: the turn's notice.
  - Decision 4 adds two built-in tools, so `DESCRIPTOR_VERSION` bumps once.
    It is coordinated with the goal PR's bump, and whichever lands second
    takes the next number. It also adds a store schema bump for the
    delivered mark.
  - Decisions 4 and 5 change the prompt, so `AGENT_PROMPT_VERSION` bumps.
  - A "waiting for sub-agents" run activity and a distinct rendering for
    delivered answers are client work. They are deferred to AC14, which
    may bump the protocol.
- **Cost for roots is small.** Over the same 30 days, 12 of 95 long root
  runs had a stretch of 64 calls with no change or command, and 3 had one of
  128. Each such stretch now costs one short report turn. Cached prefixes are
  kept.
- **A read-only child is bounded by its lack of output, not by money or
  time.** A broad question gets a partial answer within about 316 calls,
  plus a named next step. The parent can spawn a narrower follow-up. It
  cannot get an unbounded reader.
- **A non-blocking parent sees a workspace its read children are also
  reading.** A child can observe a file mid-edit by the parent. Answers are
  evidence to check, as before. Write children stay serial.
- **Existing fixtures change behaviour.** AC0's
  `empty_checkpoint_characterizes_the_single_shot_fatal_fault` flips, as its
  owning slice intended. Scripted providers that run more than 64 read-only
  calls in a row must answer a report turn. The AP3a and AP3b slices own updating
  them.
- **This refines ADR-0049 § 8.** Identical-call rejection and the
  no-novelty pause still apply to every run. However, text written in a
  runtime-requested report turn does **not** count as § 8's "new assistant
  text". Otherwise a stuck root that answers every report would never
  pause. The AC4 slice implements the guard with this rule, and the goal
  driver's `blocked { no_progress }` is unchanged.
- **This supersedes ADR-0048 § 2's empty-checkpoint clause.** An empty
  checkpoint is a missed report, not a placeholder turn.
- **A parent's final answer can wait on its slowest read child**, up to
  about 316 of that child's calls, unless the parent cancels it.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| A per-child budget (turns, tokens, cost or time) by default | Stops waste by stopping work. Multi-day runs need no spending cap, and a budget cannot tell a stuck reader from a productive writer |
| Novelty by file content (lines already seen) | Needs per-content state and still counts reading as progress. The stall rule catches the observed failure, 680 distinct reads, without it |
| A wall-clock stall timer | Non-deterministic and untestable with a scripted provider. A long build or test is work, not a stall |
| A `report_progress` tool | A tool-free reply is already a report. A tool adds catalog bytes to every run and can be called with nothing in it |
| A model judge that scores progress | A second paid judge on the hot path. ADR-0049 already rejects one for completion |
| Keep checkpoint notices in the system prompt | Invalidates the cached prefix twice per slice, and the data shows the model ignores it |

## Evidence / references

- Store queries: [`../runbooks/progress-report.md`](../runbooks/progress-report.md);
  baseline recorded in `plans/progress/root.md` (2026-09-30).
- QQ source:
  - `crates/qq-core/src/lib.rs:106–122`, `352–379`, `1712–1731`,
    `2722–2743`, `3288–3298`, `3455–3467`;
  - `sessions/subagents.rs:303`, `387–412`, `558–651`;
  - `sessions/store.rs:1881–1926`;
  - `sessions/transcript.rs:761–785`;
  - `tools/output.rs:436–440`, `tools/read.rs:147–154`, `409–411`;
  - `runtime/prompt.rs:164–209`, `approval.rs:477`;
  - `sessions/commands.rs:115–121`.
- Related: RR1 (#108) kept tools declared on the checkpoint turn. ADR-0048
  § 2 and ADR-0049 § 8.
