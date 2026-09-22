# Transcript Rendering

The transcript is the product. A run that reads as one undifferentiated wall
of text hides what the agent actually did; a run that reads as a story —
"said this, ran that, saw the result, said more" — is auditable at a glance.
This document describes the message model and visual system that make qq's
transcript read as a story.

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
   QQ
   Sure, I'll look into that...

 ● Read   crates/qq-core/src/lib.rs                                 212 lines
 ● Search "ToolGate"                                        14 hits · 3 files

   QQ
   The gate resolves after the turn yields. Checking the store side...

 ● Read   crates/qq-core/src/sessions.rs                            488 lines

   QQ
   Here's what happens: ...
```

The per-turn `QQ` header repeats only when a turn has text; consecutive
call-only turns merge into one call group. Group folding (>3 quiet calls)
applies per contiguous call group, not per run. The header is kept even
for turns whose text is a single short interim line — consistency beats
density.

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

## Turn Headers

Every turn opens with a role header row, and the rows below it share the
header's rail (`transcript::message_presentation`):

- A user turn is `▌ YOU` in `accent` bold on an `accent` rail, and **every
  row of the prompt keeps the `▌` rail**: wrapped rows, later paragraphs,
  list items. The rail is the transcript's structural accent, so a prompt
  reads as one bar down the left edge however long it is.
- An assistant turn is `QQ` in `brand` bold — the one warm mark per turn —
  over a three-cell blank rail. A message that is not complete carries its
  state to the right of the label in the state's status color (`streaming`
  in `accent`, `failed` in `error`, `queued`/`cancelled`/`interrupted` in
  `warning`); `complete` is never written.
- Steering rows keep the user rail and say what they are (`steering
  waiting for the next turn`, `steered`, `steering  run finished first`);
  a pending prompt is `▌ YOU  pending` in `warning` with a `warning` rail.
  Reasoning rows open with `∴` and expand under a `┆` rail. None of these
  differ from a turn header in geometry, only in wording and color.
- The run's completion line (`✓ 42s · 3 tools · 12.3k tok · $0.04`) sits
  under the run's last message at the prose column.

## Column Model

The transcript has two columns: a **rail** in cells 1–2 and **content**
from cell 3. Everything that is not prose lives in the rail; everything
that is prose, or reads like it, starts at the content column:

| Row | Rail | Content column |
| --- | --- | --- |
| User prompt | `▌ ` accent | `YOU`, then the prompt text |
| Assistant | two blanks | `QQ`, then the message body |
| Tool summary | state glyph (`●`, `◐`, `✕`, `◇`, `○`, `◌`) | verb, subject, metric |
| Folded call group | `▸` accent | `Read ×4  a.rs, b.rs` |
| Selected tool row | `▶` in cell 0, glyph in cell 1 | unchanged |

Because tool verbs, prose, and role labels share one column, the eye moves
straight down the transcript; the rail glyphs alone say what kind of row
each is (design principle 4). The transcript cursor marks the selected call
with `▶` in the margin cell rather than shifting the row.

## Spacing

Rhythm rules, applied in the transcript assembler:

- One blank row of top padding ahead of the first transcript row, so the
  body breathes under the top row. It is a body row like any other: it is
  on screen exactly when the viewport reaches the top (short transcripts,
  or scrolled all the way up) and is above the window otherwise. Scroll
  offsets count from the tail, so it never moves the tail anchor.
- One blank line between every block (message body ↔ call group ↔ next
  turn's text). Two blank lines before each `YOU` prompt — the
  prompt/response boundary is the strongest seam in the transcript.
- Message bodies indent under their role header (a 3-column gutter);
  call groups keep their own gutter glyphs so text and calls are
  distinguishable by silhouette alone.
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
 ● Read   crates/qq-client/src/sse.rs                          412 lines  0.4s
 ● Run    cargo test -p qq-client reconnect                     exit 101  3.2s
 ◐ Edit   crates/qq-client/src/sse.rs                            running  1.1s
```

- The state glyph is in the rail; the verb starts at the content column,
  padded to six cells so subjects align down a run.
- **The metric and duration are right-aligned to the content width**, at
  every width and in every pane, so the numbers form a column down the
  right edge the way the verbs do down the left. The right side is `metric
  [· truncated] [· state]  duration`; the state label appears for any call
  that is not complete, in its status color.
- The subject takes whatever is left between the verb and the right side.
  Paths elide from the middle so the file name always survives; other
  subjects truncate with `...`.
- When the row cannot hold a six-cell subject plus the right side, the
  duration is dropped first, then the metric, so the row degrades to verb
  and subject before anything is cut. A row narrower than that is
  truncated like any other line.
- In folded detail a block of more than three quiet completed calls is one
  row, `▸ Read ×4  Search ×2  a.rs, b.rs, c.rs, +1`, with the `▸` in the
  rail and the text at the content column.

### Tool Detail

Everything a call shows beneath its summary row renders in the code panel
(§ Code Blocks): the same `┃` rail in `border` on the `surface` tint, one
padding cell, character-wrapped content with `↪` on continuation rows, and
a blank surface row above and below. Output is visibly output, and it
shares one implementation with fenced code (`markdown::panel_rows`); tool
panels sit at the content column, the margin before the rail on the
terminal background.

```
 ● Run    cargo test -p qq-client reconnect                     exit 101  3.2s
   → 12:04:11 → 12:04:14
   ┃
   ┃  test result: FAILED. 1 passed; 1 failed
   ┃
```

- The **timing line** (`started 12:04:11 → 12:04:14`, or the live elapsed
  clock) stays `muted` above the panel, outside it.
- **Expanded results** (read heads, command tails, MCP `key: value`
  argument rows, then the result) fill one panel per call; argument rows
  and result are separated by one blank surface row when both exist.
- **Error tails** keep the `error` foreground on the surface.
- **Diffs** keep their line numbers; `+`/`-` rows keep the add/remove
  background inside the panel (a span's own background wins over the
  surface, the U3 rule), and the gutter stays surface.
- **Live output** of a running command renders in the same panel from the
  first frame, so the panel does not jump when the call completes.
- Panels carry no label: the summary row above already names the tool and
  subject, and a language tag would need a grammar lookup per frame.
- **Row budgets are unchanged**: each detail kind shows the same number of
  content rows as before; the panel adds exactly its two padding rows
  (`tools::TOOL_PANEL_PADDING_ROWS`) per expanded call. The approval block
  is not a tool panel and renders as before.

## Diffs

Two sources render as unified diffs with per-line coloring:

- `EditPreview.diff` in the approval modal (carried on
  `ToolApprovalRequested`).
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

Coloring: `+` lines green, `-` lines red, `@@` hunk headers in the muted
accent, context lines normal. Diff lines are literal (character wrap, no
reflow). Expanded tool diffs and fenced blocks tagged ` ```diff ` render
inside the code panel with the same coloring; the add/remove backgrounds
win over the panel surface on those rows.

## Streamed Tool Output

Streamed shell tool output arrives as `ToolCallOutputDelta` events and
renders live under the running call, inside the same call-group layout,
so a long build is watchable as it happens rather than only after the
call completes.

Live output is a tail, not a record: the client buffers at most 4 KiB
per running call, dropping the head on a character boundary, and shows
the last few complete lines (up to six rows, muted, character-wrapped,
control characters stripped, inside the tool detail panel) at every
detail level — a running command's
output is the thing the user is waiting for. A trailing partial line
waits for its newline. The buffer is discarded when the call reaches a
terminal state or a snapshot reloads; the bounded result persisted on
the tool call is always authoritative.
