# Transcript Rendering

The transcript is the product. A run that reads as one undifferentiated wall
of text hides what the agent actually did; a run that reads as a story —
"said this, ran that, saw the result, said more" — is auditable at a glance.
This document describes the message model and visual system that make qq's
transcript read as a story.

A prompt with a captured active goal receives a bounded runtime notice before
its accepted prompt and immediately after each in-run compaction summary.
The notice renders the captured goal verbatim, including its bounded check
tail, outside the system prompt. It is derived context rather than a user
instruction; the window's context measurement includes its bytes. Goal-less
runs add no notice.

At turn boundaries, a claimed goal run reads the committed current goal for
display. A changed revision adds one notice in that window, and summaries
carry the latest display snapshot. Display refresh accepts only the same
active goal; it cannot borrow a replacement goal or refresh the immutable
claim's model-control authority. Cancellation and the original deadline
bound the goal-only store read. Goal-less runs perform no such read.

The claim freezes its exact initial text. Compaction markers retain their
post-summary notice, and completed model turns retain pending revision
notices in request order. Reopening replays those stored bytes: initial text
before the accepted prompt, marker text after its summary, and revision text
after steering, merged with child deliveries by their persisted positions.
Compaction drops the first retained turn's notices just as the live splice
does. Notice reads share the bounded replay allowance with turn envelopes;
side-question scans include their bytes. An active task's frozen initial goal
notice and framed prompt are captured as one bounded unit, even after a live
goal edit; if they cannot fit, both are omitted. Already reserved notice prefixes
at run start or compaction are persisted without another capacity debit.

Goal-bound turn replay merges exact framed boundary notices with child
deliveries using each notice's persisted preceding-delivery count. A child
wait can deliver before a fresh goal notice; an empty reactive retry can
retain a checkpoint between two revisions without committing an assistant
turn. Both sequences retain their live order after reopening. Static notices
retain their typed kind in the turn-notice column for report discovery, even
when their exact positioned text also appears among goal atoms. Replay renders
that text once. Goal-less runs retain their existing notice rendering.

A client cancelling a live run bound to the current active goal pauses that
same goal as a user stop, including after a goal revision edit. Replacement
goals keep their identity and state. Queued preparation and already running
paid work retain their reservations until they drain, and Resume refuses
cancelled obligations that have not drained. Runtime settlement cancellation
remains neutral. Repeating a cancellation restores signals for durably
cancelled work without repeating the goal mutation.

Replayed goal commands retain their scheduler wake-up. Their durable receipts
remain identical, while a restarted caller can restore the wake-up lost after
the original commit. Provider replay data and reasoning lifecycle markers
establish model work even without text bytes or a usage receipt; an ensuing
API fault keeps unknown spend and cannot become a zero-work rejection.

## Why The Turn Is The Unit

The runtime originally created one assistant message per run and appended
every model turn's text into that message's flat `output` string. Tool calls
carried `turn_ordinal`/`call_ordinal`, but text carried no turn markers, so
no client could reconstruct the interleaving: a run rendered as all assistant
text concatenated, followed by every tool call in the run. The model's actual
sequence — preamble text, two calls, interim text, another call, final
answer — was flattened into "text blob, then call list", even though the
`model_turns` table already persisted per-turn assistant content for context
assembly.

The unit of assistant output is therefore the model turn, not the run:

- `MessageSnapshot` carries `turn_ordinal: u16`. User messages use 0.
- The runtime starts a new assistant message when a model turn begins
  (first delta after the previous turn's tool results), emitting
  `AssistantMessageStarted` per turn. `TextAppended` targets the current
  turn's message.
- The run row's `assistant_message_id` is the *current* assistant
  message; run claiming and crash recovery interrupt only that message.
- Persistence: the message row commits in the same transaction as
  `persist_model_turn` (turn row + tool_call rows + events), preserving
  the atomic-persist invariant. Empty turns (calls with no text) persist
  no message row.
- Migration: pre-existing rows keep `turn_ordinal = 0`; old runs render
  as one message with calls after. `SnapshotRequest.message_limit`
  semantics are unchanged (the limit counts messages, and per-turn
  messages are messages).

Rejected alternative: segment markers inside the single message's output
string. A flat string with positional markers is fragile under streaming
appends and pushes parsing into every client.

### Model Context Replay

Model-context replay identifies each tool result by run, turn ordinal, and
provider call ID. Providers may reuse IDs in later turns or runs; a later
result cannot fill an earlier missing result. A missing stored result is
reconstructed as an interrupted error without rerunning the tool.

Pruning uses that result's stored effect and its own turn's tool name and
arguments. Explicit non-read-only effects preserve the output. Only results
without stored effect metadata use the legacy built-in-name fallback.
These are assembly rules: persisted history is unchanged, including when
it is retrieved through history search or supplied to a compaction request.

Each turn's results also pass through the per-turn output budget
(`TurnOutputBudget`, `tools.md` § Per-turn budget) in block order — the
same function the live run applied before the results entered context — so
the assembled turn carries exactly the bytes the model saw, not the larger
stored rows. The projection is deterministic over the stored `result`,
call id, and spill digest; it is reconstructed, never persisted.

### Client Assembly

The TUI orders a run's items by `turn_ordinal`, rendering each turn's
message followed by that turn's calls (`call_ordinal` order):

```
  ┃ make the sse reconnect test deterministic

  ● Read     crates/qq-client/src/sse.rs  412 lines
  ✕ Ran      cargo test -p qq-client reconnect  exit 101
  │ running 4 tests
  │ test reconnect_replays ... FAILED
  │ test result: FAILED. 3 passed; 1 failed
  ◐ Editing  crates/qq-client/src/sse.rs

    The test slept on wall-clock time; pausing the runtime clock removes the
    race. Applying the edit now. More.▍
```

Answers carry no label, so a turn's text and its calls alternate with one
blank row between them; consecutive call-only turns merge into one call
group. Folding into a turn receipt (§ Tool Rows) applies per contiguous
call group, not per run.

Call groups do not require an assistant message as an anchor. The runtime
deliberately persists no message row for a call-only turn, so the client renders
those calls directly after the run's user prompt until a later text turn gives
the run an assistant-message anchor. Fold/Focus likewise includes the focused
run's calls; a compressed layout must not turn active work into a generic
"working" label.

## Long Messages

A completed assistant message is fully reachable through transcript scrolling.
Render caches and terminal viewports may bound work, but they must not discard
the beginning of authoritative output. While a message is still streaming, QQ
may render a bounded tail to keep per-frame work predictable; when it does, it
shows an explicit omission notice and restores the full message when the turn
reaches a terminal state.

Completed messages inside the styled-markdown bounds cache their rendered rows.
Oversized messages use a sparse plain-text row index: only the requested
terminal viewport is reconstructed, checkpoints are bounded, and the
authoritative string remains the source. This trades rich markdown styling on
exceptionally large output for complete access and predictable frame work.

## Messages

No message has a role label; the rail says whose it is
(`transcript::message_presentation`):

- Your prompt is `┃` in `accent` followed by bold `text`, and **every row of
  the prompt keeps the `┃` rail**: wrapped rows, later paragraphs, list
  items. The rail is the transcript's structural accent, and the composer
  uses the same one.
- An answer is unlabelled markdown indented two columns. While it streams,
  a `▍` caret in `accent` marks the tail.
- A message's state, when it is not plain, is one row under it
  (`transcript::message_note`). Steering prompts read `waiting for the next
  turn`, `steered`, or `not sent · the run finished first` in `faint`;
  other messages read `queued` in `faint`, `cancelled` or `interrupted` in
  `warning`, or `✕ failed` in `error`. A pending prompt keeps a `faint` rail
  with a `pending` row.
- Reasoning is one `faint` row, `∴ thought for 12s`, after it finishes;
  while it streams it shows only on the rule (`◐ thinking · 6s`). `Alt-R`
  expands it under a `faint` `│` rail.
- A completed run prints no completion line. Only a run that ends
  `cancelled`, `interrupted`, `paused`, `budget exhausted` (`◌` in
  `warning`) or `failed` (`✕` in `error`) gets one row after its last
  message, with its duration when over one second.

```
    the model turn
  ┃ pending steer: also check the tests
    waiting for the next turn
  ┃ applied steer: prefer edit_file
    steered
  ┃ late steer: never mind
    not sent · the run finished first
```

## Column Model

Content starts after the pane's 2-column margin and fills the pane width;
there is no reading measure. Inside it, the first two cells are the **rail**
and the rest is **content**:

| Row | Rail | Content column |
| --- | --- | --- |
| User prompt | `┃ ` accent | the prompt text, bold |
| Answer | two blanks | the message body |
| Tool summary | state glyph (`●`, `◐`, `↻`, `✕`, `◇`, `○`, `◌`) | verb, subject, metric |
| Turn receipt | `●` faint | `6 calls · read 2 · ran 2 …` |
| Tool tail or detail | `│ ` faint | output, diff, or error rows |
| Agent lane | `├ ` / `└ ` faint | glyph, title, activity |

The rail glyphs alone say what kind of row each is. The transcript cursor
marks the selected call by painting its glyph in `accent` and its verb bold
rather than shifting the row.

## Spacing

Rhythm rules, applied in the transcript assembler:

- One blank row of top padding ahead of the first transcript row. It is a
  body row like any other: it is on screen exactly when the viewport
  reaches the top (short transcripts, or scrolled all the way up) and is
  above the window otherwise. Scroll offsets count from the tail, so it
  never moves the tail anchor.
- One blank row between every block (prompt ↔ reasoning ↔ call group ↔
  answer ↔ next prompt). A steering prompt follows the message before it
  without a gap. There is no blank row between a tool row and its tail, or
  between agent lanes.
- Answers indent two columns; call groups keep their rail glyphs so text
  and calls are distinguishable by silhouette alone.
- Inside a message, markdown blocks are separated by exactly one blank
  row: paragraph, heading, list, quote, code panel, table, rule, and
  footnote definition each begin one row below the previous block. Items
  within one list and rows within one table stay tight. No blank row opens
  or closes a message; the turn spacing above owns that. A streaming
  message's settled prefix lays out to the same rows as the whole
  (`markdown::settled_prefix_end`), so the gap that follows a block is part
  of the prefix that ends with it.

## Markdown Blocks

Block markers, applied in `view/markdown.rs`:

- **Headings** differ by level: H1 is `text` bold with a `─` rule in
  `border` beneath it the width of the title; H2 is `accent` bold; H3–H6
  are `text` bold.
- **Bullets** are `•` in `accent`, nested bullets `◦`, indented two cells
  per level. **Ordered** items keep their numbers, right-aligned to the
  widest number the list reaches (` 9.` / `10.`). **Task** items show `☐`
  or `☑` in `accent` in place of the bullet.
- Wrapped list rows hang: continuation text aligns under the item's first
  word, not under the marker. The item body wraps at the width left after
  the marker, and later paragraphs inside a loose item start at the same
  column.
- **Quotes** carry a `▎ ` rail in `muted` on every row, wrapped rows
  included, and the body is `muted` italic. Nested quotes stack rails.
- A **rule** is `─` across the full content width in `border`.
- Rows never end in the whitespace a word wrap broke on.

## Code Blocks

Fenced code is a visually distinct panel instead of tinted prose
(`markdown::layout_code_panel`):

- Every panel row is padded to the full content width on the `surface`
  tint, so the block reads as one solid slab. The transcript's block gap
  supplies the blank row above and below; the panel adds none of its own.
- The rail is `┃ ` in `border` on the surface, then one cell of padding,
  then the content. A source line that wraps (character-exact, literal
  wrap) shows `↪` in place of `┃` on its continuation rows, so a wrapped
  line is distinguishable from a new one. Leading indentation is kept
  exactly; the gallery goldens pin it.
- The top padding row carries the rail and the fence's language label
  right-aligned in `muted` (` rust `, one space each side, ending at the
  content width); it is a blank surface row when the fence has no tag. The
  bottom padding row is always present and blank, so the rail is continuous.
- `diff` fences keep add/remove tints inside the panel: a span that brings
  its own background (`diff_add_bg` / `diff_del_bg`) keeps it, and only
  spans with no background take the surface. The rail and the trailing
  padding stay on the surface.
- A fence that is still streaming lays out the same rows as the finished
  block would: the label row appears as soon as the fence opens and the
  bottom padding row is present while the block is open, so the panel does
  not jump when the closing fence arrives.
- Inline code is `text` on the same `surface` tint as the panel, not bold
  and not colored: a `name` in prose shares the panel's voice instead of
  borrowing `warning`. Only fenced blocks get panels.

## Inline Styling

- Emphasis is italic and strong is bold, layered on whatever style the
  enclosing block set (a bold word in a quote stays `muted`).
- Link text is `accent` underlined; the URL is not rendered and no OSC 8
  hyperlink is emitted, so the underline is the only mark. A link replaces
  the enclosing emphasis rather than combining with it.
- Footnote references (`[1]`) are `accent`; inline and display math are
  `muted` so an unrendered formula does not read as an alert.
- `Style` carries its attributes (bold, dim, italic, underline) as one
  packed byte; `write_line` emits an attribute only when a span turns it on
  and resets when any attribute turns off, so the attribute set never
  drifts across a row.

Rendering note: the TUI paints with its own `Style`/`Span`/`Line` primitives
and a whole-row diff (`crates/qq-tui/src/render.rs`), not Ratatui. Background
tint means setting the surface background on every span of the block's lines
and padding each line to full width so the tint reads as a panel, not ragged
highlights. The surface color, like every other color in the transcript, is a
role from the active theme (`docs/design/theme.md`).

## Tool Rows

Each tool call is one summary row (`tools::tool_summary_line`), built from
the call's cached `ToolRow` (verb, subject, metric) plus the live clock:

```
  ● Read     crates/qq-client/src/sse.rs  412 lines
  ✕ Ran      cargo test -p qq-client reconnect  exit 101
  │ running 4 tests
  │ test reconnect_replays ... FAILED
  │ test result: FAILED. 3 passed; 1 failed
  ◐ Editing  crates/qq-client/src/sse.rs
```

- The state glyph is in the rail; the verb starts at the content column,
  padded to nine cells (`tools::TOOL_VERB_WIDTH`) so subjects align down a
  run. A settled call reads in the past tense (`Read`, `Listed`,
  `Searched`, `Edited`, `Wrote`, `Ran`, `Spawned`); a running one in the
  present (`Reading`, `Editing`, `Running`) with the shared `◐◓◑◒` spinner
  in `info`.
- The subject follows in `muted`, then **the metric two columns after it**
  in `faint` (`412 lines`, `+4 −1`, `exit 101` in `error`), then the
  duration when it is over one second. Nothing is right-aligned to the pane
  edge. A call waiting on approval reads `waiting` in `warning`; the
  approval itself is in the dock.
- Paths elide from the middle so the file name always survives; other
  subjects truncate with `…`.
- When the row cannot hold a six-cell subject plus the tail, the duration
  is dropped first, then the metric, so the row degrades to verb and
  subject before anything is cut. Error labels are never dropped.
- A shell or exec call that exits non-zero is `✕` with `exit N`, and keeps
  a `faint` `│` tail of at most three rows (`MAX_TOOL_ERROR_ROWS`) under it.
- **Turn receipt.** In folded detail (`Ctrl-O`), once every call in a group
  has settled, its routine calls (completed, not failed, not edits, not
  expanded or selected) collapse into one `faint` row; edits and failures
  keep their own rows under it:

  ```
  ● 6 calls · read 2 · edited 2 · ran 2   2 files
  ● Edited   crates/qq-tui/src/file_1.rs  6 B
  ● Edited   crates/qq-tui/src/file_4.rs  6 B
  ```

  The receipt counts every call in the group by verb, then the files
  edited with `+a −b` when the diffs report them, then the group's
  duration when over one second.
- **Agent lanes.** A session's child agents render as one header row and a
  lane per agent. More than six fold to the six most urgent plus
  `└ +N more  ^T`:

  ```
  ● Spawned  1 agent   ◇ 1
  └ ◇ Deploy helper   wants to run a command
  ```

  The header tallies lanes by state (`◐ n  ◇ n  ● n  ✕ n`); each lane is
  its agent's glyph, title, and live activity, or `done · N calls`.
- **Error rows are graded, not uniform** (`ToolErrorKind`, tool-layer D9).
  The kind comes from the built-in tool name and result's leading error
  code (`ToolErrorKind::of`); external-tool errors remain failures regardless
  of their text. Every stored row grades the same with no wire field, and
  the model's text never changes:

  | Kind | Codes | Glyph | Tail | Under the row |
  | --- | --- | --- | --- | --- |
  | Correction | `invalid_*`, `bad_glob`, `cursor_invalid`, `not executed:`, `not_read:`, … | `↻` `faint` | `corrected` `muted` | nothing; the text is on expand |
  | Outcome | `path_not_found`, `stale_file`, `not_found`, `range_out_of_bounds`, non-zero `shell`/`exec` exit, … | `✕` `error` | the code in `warning`, or `exit N` | one row of the error tail |
  | Failure | anything else, including unlisted codes | `✕` `error` | `failed` | up to three rows of the error tail |

  A correction followed later in the block by a successful call to the
  same tool is the routine "model fixed its own argument" case: its row
  folds away and the retry is the record. Selecting or expanding it still
  shows it. A new error code is a `Failure` until it is listed, so nothing
  becomes quieter by accident.

### Tool Detail

Everything a call shows beneath its summary row hangs on the same `faint`
`│` rail as a failed command's tail, at the content column, on the
terminal background. Fenced code in answers keeps its `surface` panel
(§ Code Blocks); tool output does not.

```
  ● Read     crates/qq-tui/src/file_0.rs  20 lines
  │ fn main() {}
  │ fn main() {}
  │ fn main() {}
  │ … 17 more lines
  ● Edited   crates/qq-tui/src/file_1.rs  6 B
  │ edited
  ● Ran      cargo test -p crate_2
  │ test result: ok. 12 passed
```

- **Expanded results** (read heads, command tails, MCP `key: value`
  argument rows, then the result) show at most three rows
  (`MAX_TOOL_RESULT_ROWS`) in `muted`, with a `faint` `… N more lines` or
  `…` marking what was cut. Argument rows and result are separated by one
  blank rail row when both exist.
- **Error tails** keep their `error` or `warning` foreground.
- **Diffs** show line numbers in `faint` and `+`/`-` signs and text in the
  diff colors, the same rows the approval panel draws
  (`overlay::dock_diff_lines`).
- **Live output** of a running command renders on the same rail from the
  first frame, so nothing jumps when the call completes.
- There is no timing line; the summary row carries the duration.
- Detail carries no label: the summary row above already names the tool
  and subject.

## Diffs

Two sources render as unified diffs with per-line coloring:

- `EditPreview.diff` in the dock's approval panel (carried on
  `ToolApprovalRequested`), at most 8 rows.
- Completed `edit_file`/`write_file` calls at the expanded tool detail
  level, from the call's `display` payload — an extensible tagged
  snapshot field (first variant: diff) persisted alongside the result,
  populated on successful completion, and excluded by construction from
  model context and the session context budget. The model sees the
  summary result string; the transcript sees the diff. The approval
  preview and the persisted display share one diff builder with two
  bounds (2 KiB per side for previews, 32 KiB per side persisted, same
  truncation marker). Diff-shaped results without a payload (shell
  output, legacy stores) still color via shape detection.

Coloring: `+` lines green, `-` lines red, context lines normal, line
numbers `faint`; `@@` hunk headers set the line numbers and are not drawn.
Diff lines are literal (no reflow). Fenced blocks tagged ` ```diff ` render
inside the code panel instead, where the add/remove backgrounds win over
the panel surface.

## Streamed Tool Output

Streamed shell tool output arrives as `ToolCallOutputDelta` events and
renders live under the running call, inside the same call-group layout,
so a long build is watchable as it happens rather than only after the
call completes.

Live output is a tail, not a record: the client buffers at most 4 KiB
per running call, dropping the head on a character boundary, and shows
the last few complete lines (up to three rows, muted, control characters
stripped, on the `│` tail rail) at every detail level — a running command's
output is the thing the user is waiting for. A trailing partial line
waits for its newline. The buffer is discarded when the call reaches a
terminal state or a snapshot reloads; the bounded result persisted on
the tool call is always authoritative.

## Run Activity

While a run is working, the rule and the session's sessions-column row say
what it is doing. The words come from the running tool (`editing`,
`running`) or the run's latest `RunActivity` (`waiting for model`,
`thinking`, `writing`, `preparing a tool call`, `compacting context`), and
the rule adds the elapsed time once it passes one second (`◐ editing ·
14s`).
The activity belongs to the session's active run, and only that run's
reports change it (`qq-client` `state::reduce`):

- A prompt run compacting its own earlier turns reports `compacting` before
  the summarizer is asked and `waiting for provider` at its next turn. The
  internal compaction run it starts reports `compacting` as well. That
  report, and the compaction run's own finish, leave the prompt run's
  activity as it is, because the prompt run still holds the session.
  The reducer remembers a step observed outside the active slot, from its
  start event or a running snapshot. Its finish remains bookkeeping if
  cancellation/recovery settles the prompt first: only the prompt raises
  completion attention or unread state. Unfinished step stats survive the
  prompt's transcript cleanup and expire when the step settles.
- In the sessions column (its Wide second row) and on agent lanes, a
  running session shows the first of: an approval wait, the
  running tool's verb, the streamed text tail, then the activity. The one
  exception is `compacting context`, which wins over the tail. The text a
  compacting run streamed before compacting is stale until the summary
  lands. The `sessions` golden pins this.
- A compaction produces no transcript row of its own. The summary replaces
  history in the model's context, not on screen; the run's earlier turns
  stay rendered as they were.
