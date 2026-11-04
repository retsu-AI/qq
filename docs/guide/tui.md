# The TUI

`qq` opens the terminal UI in the current directory. It attaches to the
user's running server if there is one — a `qq serve`, or the server another
open `qq` started — and otherwise starts one inside itself that lives
exactly as long as that `qq` does (see [Exiting](#exiting)). Everything
below works the same either way.


The first frame appears before configuration and server attachment finish.
You can type immediately; prompts submitted while connecting wait for the
initial session, without consuming a newer draft. File mention resolution
and `@` completion run in the background. Highlighting a cold session in
the session list for 100 ms starts loading its history before you open it.
## Layout

```
  ┃ make the sse reconnect test deterministic                                               WORKING
                                                                                          ▎ 1 ◐ Session 0
  ● Read     crates/qq-client/src/sse.rs  412 lines
  ✕ Ran      cargo test -p qq-client reconnect  exit 101                                    RECENT
  │ running 4 tests                                                                         2 ○ Session 1
  │ test reconnect_replays ... FAILED                                                       3 ○ Session 2
  │ test result: FAILED. 3 passed; 1 failed
  ◐ Editing  crates/qq-client/src/sse.rs

    The test slept on wall-clock time; pausing the runtime clock removes the race.
    Applying the edit now. More.▍

  ◐ editing ──────────────────────────────────────────────────────────────────────────
  ┃ Ask QQ…
    Session 0 · gpt-test                                                    /workspace
```

There is no header row; everything you control sits in the dock at the
bottom.

- **Transcript**: your prompts on an `┃` rail, the model's answers below
  them, and one row per tool call: verb, subject, then a metric such as
  `412 lines`, `+4 −1` or `exit 101`, and the duration once it passes a
  second. A failed command keeps its last three output rows under it.
  Click a tool row, or select it with `Ctrl-Up` / `Ctrl-Down` and press
  `Enter`, to open it in place: every line the client holds, wrapped
  rather than cut, scrolled with the transcript. A read shows the file
  text, an edit its whole diff, a command its output (and the full command
  when the row had to shorten it), an MCP call every argument and its
  result. Where qq cut a long result for the model, the cut reads
  `⋯ 312 lines omitted · stored for read_tool_result`. A body of twelve
  rows or more ends in a `╰ Read src/lib.rs · click or Enter folds` row;
  clicking it, or the header, folds the call again. Clicking inside the
  output selects text as usual. While a turn runs, its newest edit also
  shows up to eight rows of its diff without being opened. Once a turn
  settles, three or more routine calls fold into one receipt row
  (`● 6 calls · read 2 · edited 2 · ran 2   2 files`); edits and failures
  keep their rows. Click the receipt, or press `Ctrl-O`, to open every
  receipt.
  `Alt-R` shows or hides all reasoning, which
  otherwise reads `∴ thought for 12s`; the block cursor also stops on that
  row, and `Enter` opens just that one.
- **Sessions column** (90 columns and wider, with two or more sessions;
  `Ctrl-\` toggles): sessions numbered 1–9 and grouped by what you should
  do — NEEDS YOU, WORKING, RECENT — with `N new` for unread replies and
  `↳N` for child agents, which fold into their session's row. Click a row,
  or press `Alt-1`…`Alt-9`, to focus it. From 160 columns each session gets a second row
  with its activity and cost. Below 90 columns the column is gone and the
  rule carries the counts instead (`8 sessions  ◐ 2  ◇ 2  ^G`).
- **Rule**: the focused session's state and activity (`◐ editing · 14s`),
  or the last notice. Its color follows the session: dim when idle, blue
  while running, yellow when it needs you, red after a failure. On the
  right, `◇ N needs you ^G` counts approvals waiting in other sessions, and
  `↓ N new  End` counts rows that arrived below while you read further up.
- **Panel**: approvals, questions, and every picker (`Ctrl-T`,
  `/models`, `/theme`, …) open here, between the rule and the composer, and
  take at most half the screen. `Esc` closes them with your draft intact.
  The switcher (`Ctrl-T`) takes the rule and the composer too: the rule
  reads `○ switch ─── 8 sessions` and you type the search on the `┃` rail.
- **Composer**: type and press `Enter`. `Shift-Enter` or `Alt-Enter` inserts
  a newline. `Alt-E` or `/editor` opens the draft in `$EDITOR`. The `┃`
  rail is dim for a read-only session, the accent color for `ask` and
  `auto`, and yellow for `full`. Queued drafts sit above it on a `┋` rail.
- **Status line**: the session title, model and effort, context used, cost,
  and the approval mode when it is not `auto`; the workspace on the right,
  or `connecting` / `reconnecting` / `offline` when relevant. A picker shows
  its keys here instead.

## Sessions

| Action | Key | Slash |
| --- | --- | --- |
| new session | `Alt-N` | `/new` |
| new child session (sub-agent under the focused one) | `Alt-C` | |
| switch session: type to search, `Enter` opens, `Tab` unfolds a session's agents, `Ctrl-A` lists every agent | `Ctrl-T` | `/sessions`, `/resume` |
| swap to the previous session | `Ctrl-^` (also `Ctrl-6`) | |
| focus the Nth row of the sessions column | `Alt-1`…`Alt-9` | |
| switch session, scoped to the focused session's agents | | `/agents` |
| focus parent / first child / next / previous sibling | `Esc`* / `Alt-Down` / `Alt-Right` / `Alt-Left` | |
| jump to the next session that needs you | `Ctrl-G` | |
| delete the highlighted session in the list (confirms) | `Ctrl-D` | |
| delete every empty session | | `/prune` |
| restore files to before the newest run changed them | | `/undo` |

\* `Esc` focuses the parent only when nothing else claims it. An open
picker, palette, or overlay (`/models`, `/sessions`, `Ctrl-K`, …) closes
first, and a pending approval or question claims it (see below). Otherwise it first closes an open `@` completion list, clears
a transcript selection, closes a workspace view, dismisses an error notice for
the focused session, and while a run is active arms `Esc Esc` to cancel it.
Informational notices do not claim it. `Alt-Up` is not a focus key; it pulls
back the newest queued draft.

QQ names a session from its first prompt. Sessions persist in SQLite; quit
and come back with `qq --session ID` (the exit message prints it) or pick
from `/sessions`. One session is on screen at a time; open two `qq` clients
in a terminal multiplexer to watch two.

## While a run is executing

| Action | Key |
| --- | --- |
| steer: inject the draft at the next model/tool boundary | `Enter` |
| interrupt the current turn or tool, then steer | `Alt-S` |
| queue the draft as the next prompt instead | `Ctrl-Enter`, `Ctrl-Q` |
| pull the newest queued draft back | `Alt-Up` |
| cancel the run | `Ctrl-X`, or `Esc Esc` |

Steering appears in the transcript as your prompt with a dim note under it:
`waiting for the next turn` until applied, then `steered`, or `not sent ·
the run finished first`.

## Side questions without interrupting work

Use `/btw QUESTION` or `/ask QUESTION` while the main agent works. Answers
appear in a separate scrollable side-question view; `Esc` returns to the main
transcript. `/btw` without a question reopens the view. `/btw new QUESTION`
starts a new side thread; otherwise questions continue the existing thread.
`/btw cancel` cancels only the active side question, not the main agent.
The old spellings `/btw-new`, `/btw-cancel` and `/clear` (now `/new`) say
what replaced them for one release.

Each question captures bounded committed session context. Unfinished tool
exchanges are omitted explicitly. Only built-in `read_file`, `search`, and
`tree` are available, further restricted by the session's tool policy. Live
file inspection is not a consistent filesystem snapshot. Side questions,
answers, and usage never become main-agent prompts or steering.

One query per session may run, for at most 8 model turns. Its execution,
including waits, is cut off at 120 seconds; the final state appears once the
server has recorded it, which can take a moment longer under heavy load.
Questions are limited to 8 KiB. Partial answers and independent cost/status
are durable and restored after reconnect; interrupted queries are not
restarted automatically. The same typed submit/cancel commands and side
projection are available through the session API to embedded and remote
clients.

## Approvals and questions

Under the default `auto` mode the agent does not stop for approval. In a
session switched to `ask` with `/approval`, every non-read call waits. The
transcript row reads `waiting`, and the dock shows the call and its keys:

```
  ◇ Edit     crates/qq-tui/src/view/wrap.rs  waiting

  ◇ apply this edit? ─────────────────────────────────────────────────────────────────
    40     let mut column = 0;
    41 -   while column <= width {
    41 +   while column < width {
    42         column += 1;
  ┃ y apply   n deny   Y / N with a note   e full diff
    Session 0 · gpt-test                                                    /workspace
```

`y` / `n`; `Shift-Y` / `Shift-N` decide and steer with a note. On an edit,
`e` opens the whole diff in the dock, `PageUp` / `PageDown` page it, and `e`
or `Esc` returns to the 8-row preview without deciding.
`Esc` denies, like `n`, except while a note is being typed, when it drops the
note and leaves the prompt open, or while the full diff is open, when it
closes the diff.
`Alt-A` / `Alt-D` answer the oldest waiting call in *another* session, but
only while that session's row is on screen (in the sessions column or as an
agent lane). Otherwise, and always for a question from `ask_user`, they focus
that session so you can read the call and answer it there.
`/attention` lists everything waiting across the workspace. Details in
[Permissions and trust](permissions.md#the-approval-prompt).

When the agent asks *you* a question (the rule reads `◇ answer the
question`), type an answer and
press `Enter`, press a number to pick an option, or `Esc` to decline.

## Choosing model, profile, mode, theme

| Picker | Slash | What it applies to |
| --- | --- | --- |
| model | `/models` | the focused session, or the default for the next `/new`; `Ctrl-N` inside the picker creates a session with the highlighted model |
| agent profile | `/profile` | the focused idle session, or the default for new sessions; the status line shows `as NAME` |
| approval mode | `/approval` | the focused session from its next held call, or the default for new sessions |
| reasoning effort | `/effort` | the focused idle session's next run, or the default for new sessions; rows are the levels the model's catalog entry advertises (plus `default` and `none`), or every level when it advertises none; a pin outside an advertised ladder fails the run at plan time naming the accepted levels |
| theme | `/theme` | live preview; `Enter` keeps it for the session, `Esc` restores; the notice shows the `tui.ron` line to make it permanent |

`/models` lists only models your credentials unlock. When no built-in
provider has a credential, the picker instead lists each one greyed as
`needs credential` with the fix (`run qq auth login openai or set
OPENAI_API_KEY`); `Enter` on such a row repeats the fix and creates nothing.
Custom providers appear once their `auth` reference resolves — see
[Providers](providers.md).

## Starting without a model or credential

`qq` opens even when the configuration is incomplete; only `qq ask` and `qq
run` refuse to start.

- **Project not yet trusted**: startup continues with trusted configuration;
  sensitive project settings stay disabled until you run `qq trust` on the
  server host. See [Permissions and trust](permissions.md#project-trust).
- **No `model` configured**: the status line reads `no model` and the rule
  reads `choose a model with /models` until you pick one. `Enter` in
  the picker creates the first session with the highlighted model, which
  also becomes the default for `Alt-N`.
- **Model configured, provider has no credential**: the empty transcript
  reads `openai needs a credential: run qq auth login openai or set
  OPENAI_API_KEY` (naming your provider) above `Alt-N creates the first
  session.`; `Alt-N` repeats the same line as a warning. Add the credential
  in another terminal and start `qq` again — the credential check runs at
  startup.

## Your first session

The first session in a workspace opens with a short banner in the
transcript:

```
  qq 0.1.10 · ~/qq  main · gpt-5.5 high

  /models  choose a model    @path  mention a file    ^T  switch session    ?  help

  QQ is on the host: commands run as you, with this machine's network and credentials.
  Snapshots on: `qq undo` restores the workspace as it was before each run.
```

The last two lines say where the agent's commands run (the host, or a
container) and whether `qq undo` can restore this workspace. They also show
before the first session exists. To isolate the agent, see
[Running unattended](running-unattended.md).

`/undo` restores the newest run's snapshot of this workspace through the
server, which refuses while any run in the workspace is active or queued,
and starts none until the restore finishes. `qq undo NAME`
restores an older one.

It disappears as soon as you send your first prompt and never shows for a
second session; those read `Ask QQ to begin this session.` instead. When the
configured provider still needs a credential, that remedy is printed above
the banner.

## Prompts

- `@src/parser.rs` attaches a file; `@src/parser.rs:10-40` attaches those
  lines. Completion opens as you type the path.
- `/name` runs a workspace command from `.qq/commands/name.md` or loads a
  skill from `.qq/skills/name/SKILL.md` (also `.agents/skills/` and
  `~/.config/qq/skills/`); text after the name replaces `$ARGUMENTS`.
  `/skills` lists them with their source. Typing after the slash filters by subsequence (`/mdl` finds
  `/models`).
- `/shell reload` invalidates the runtime's cached login environment;
  the next shell command captures it again. It does not call a model.
- `Ctrl-R` searches your prompt history for this session.
- `/compact` summarizes an idle session's history so a long conversation
  keeps fitting; `/rollback` undoes the newest compaction. Stale read-only
  tool results are pruned from the model's context automatically.

## Turn checkpoints, rewind and fork

Click a turn receipt to inspect that model turn's file diff in the dock.
`/rewind` opens a checkpoint picker. Choosing one restores its
files and folds later turns as rewound; they remain in the transcript but
are excluded from future model context. The restore prints a `qq undo NAME`
that reverses the file restore. Rewind refuses while overlapping work is
active or queued.

`/fork` opens a prompt picker. It forks **before** the selected prompt,
keeps earlier history, opens an isolated worktree at the preceding
checkpoint and places the selected prompt in the composer for editing.
The draft is saved, so opening a CLI-created fork also restores it. Press
Enter on a folded rewound transcript block to unfold it without reactivating
it in model context.
With the block cursor on a prompt, `f` forks from that prompt directly.
The original session and files are unchanged. Git 2.32+ and a retained
checkpoint are required. Ignored files and untracked files over 16 MiB are
not captured. External edits during a tool turn cannot be separated from
agent edits; edits between turns are excluded from that turn's diff.

## Files the agents changed

`/changes` lists every file edited in this workspace by any session,
flagging files touched by more than one.

## Every command

`?` on an empty composer, `F1`, or `/help` shows this list grouped by area
with your current bindings. `Ctrl-K` or `/commands` opens the same list as a
searchable palette that runs the highlighted command on `Enter`.

| Command | Slash | Default key |
| --- | --- | --- |
| show every command and key | `/help` | `F1`, `?` |
| open the command palette | `/commands` | `Ctrl-K` |
| open the session switcher | `/sessions`, `/resume` |  |
| switch to an agent of the focused session | `/agents` |  |
| switch session |  | `Ctrl-T` |
| swap to the previous session |  | `Ctrl-6`, `Ctrl-^` |
| create a session | `/new` | `Alt-N` |
| create a child session |  | `Alt-C` |
| compact session context | `/compact` |  |
| undo the newest compaction | `/rollback` |  |
| reload the login shell environment (/shell reload) | `/shell` |  |
| restore files to a turn checkpoint | `/rewind` |  |
| fork before a prompt into a worktree | `/fork` |  |
| delete every empty session | `/prune` |  |
| restore files to before the newest run changed them | `/undo` |  |
| focus the parent session |  | `Esc` |
| focus the first child session |  | `Alt-Down` |
| focus the next sibling session |  | `Alt-Right` |
| focus the previous sibling session |  | `Alt-Left` |
| jump to the next session that needs you |  | `Ctrl-G` |
| approve the waiting call in another session |  | `Alt-A` |
| deny the waiting call in another session |  | `Alt-D` |
| cancel the active run |  | `Ctrl-X`, `Esc Esc` |
| steer the active run with the draft |  | `Enter` |
| interrupt the active run and steer it with the draft |  | `Alt-S` |
| queue the draft until the run finishes |  | `Ctrl-Enter`, `Ctrl-Q` |
| edit the newest queued draft |  | `Alt-Up` |
| choose a model | `/models` |  |
| choose an agent profile | `/profile` |  |
| choose an approval mode | `/approval` |  |
| choose reasoning effort | `/effort` |  |
| list workspace commands and skills | `/skills` |  |
| choose a theme | `/theme` |  |
| open or close turn receipts |  | `Ctrl-O` |
| move the block cursor up; y copies the block |  | `Ctrl-Up` |
| move the block cursor down; y copies the block |  | `Ctrl-Down` |
| copy the last answer; /copy code copies its last code block | `/copy` |  |
| find in the transcript, including collapsed blocks |  | `Ctrl-F` |
| toggle reasoning detail |  | `Alt-R` |
| toggle the rail |  | `Ctrl-\` |
| toggle mouse capture | `/mouse` |  |
| show everything that needs you | `/attention` |  |
| show every file agents changed | `/changes` |  |
| ask or view side questions; /btw new starts a thread, /btw cancel stops one | `/btw`, `/ask` |  |
| edit the draft in $EDITOR | `/editor` | `Alt-E` |
| search prompt history |  | `Ctrl-R` |
| exit QQ | `/quit`, `/exit` | `Ctrl-C`* |

Keys shown beside `?`, `Enter`, `Esc`, and `Esc Esc` apply only in context:
`?` on an empty composer, `Enter` steers only while a run is active, `Esc`
focuses the parent only when there is no overlay, approval, or
question prompt, `@` completion list, transcript selection, open view, error notice,
or active run to claim it, and `Esc Esc` cancels only while running.
Rebind `toggle_navigator`, `previous_session`, `create_root_session`,
`create_child_session`, `cancel_run`, and `interrupt_run` in [`tui.ron`](configuration.md#tuiron);
every hint that mentions a rebound key updates.

Scrolling: mouse wheel, `PageUp` / `PageDown`, `Shift-Up` / `Shift-Down`,
`Ctrl-Home` / `Ctrl-End` for top and live tail; `End` on an empty composer
also returns to the tail. Scrolled away from the tail, streaming never moves
the view.

## Finding text

`Ctrl-F` opens a find row in the dock. Typing searches the focused
transcript's prompts, answers, tool calls (their commands and full output,
folded or not) and reasoning, ignoring case; the row shows `2 of 5` or `no
matches`. The nearest match at or below the top of the view is chosen first.
`Enter` or `Down` moves to the next match and `Shift-Enter` or `Up` to the
previous one, wrapping at either end. A match inside a folded tool call or
reasoning opens it in place and scrolls the matching row into view; moving
to another match folds it again. `Esc` closes find
and leaves the current match open. While find is open, the wheel,
`PageUp` / `PageDown` and `End` still scroll, and `Ctrl-C` closes find
rather than copying the match.

\* `Ctrl-C` copies while a mouse selection or the block cursor is active,
and clears a draft in the composer. Otherwise the first press says `Ctrl-C
again to quit` (and how many runs will stop when this `qq` owns the server);
a second press within two seconds exits.

## Copying

QQ draws in the terminal's alternate screen, so quitting leaves your shell
as it was. On exit it prints the focused session's last answer, as its Markdown source
with escape sequences stripped, then the resume line, into the normal
scrollback, where native copy, search and tmux copy mode reach it.

| Action | How |
| --- | --- |
| copy a span of the transcript | drag across it; release copies |
| copy a word | double-click it |
| copy a paragraph, code block or tool output | triple-click it |
| copy one block by keyboard | `Ctrl-Up` / `Ctrl-Down` to the block, then `y` (or `Ctrl-C`) |
| copy the last answer as Markdown | `/copy` |
| copy the last code block in the last answer | `/copy code` |

A drag stays inside the pane it started in. The copy comes from the rows
QQ laid out, not from screen cells: it leaves out the message rail, code
gutters, list markers, quote rails and panel padding, rejoins lines that
wrapped only to fit the pane, and keeps real line breaks and wide or
joined glyphs whole. A click without a drag copies nothing. Any key, a new
click, a resize, or the selected rows changing clears the selection.

The block cursor visits your prompts, each answer, each code block inside
an answer, and each tool call. `y` copies an answer as its Markdown
source, a code block as its raw code, and a tool call as its command and
output. `Enter` still expands a tool call, and a pending approval or
question still claims `y`, `n` and `Esc` first.

QQ always sends OSC 52, wrapped for tmux passthrough when needed, and also
tries a local helper (`pbcopy`, `wl-copy`, `xclip`, `xsel`, or `clip.exe`
under WSL; none over SSH) and then `tmux load-buffer -w -` inside tmux,
stopping at the first that succeeds. The notice says `copied 12 lines`
only when a helper or tmux confirmed the write; otherwise it says `sent 12
lines to the terminal clipboard`, because QQ can't tell whether the
terminal accepted OSC 52. Copies are capped at 1 MiB, and the notice says
when one was cut. Escape and control sequences are stripped from
everything copied.

Hold `Shift` to use the terminal's own selection instead. `/mouse` turns
mouse capture off altogether: the sessions column hides and the
transcript is drawn flush left with no margin, so a native selection of a
whole row is clean text. The wheel then belongs to the terminal; scroll
with `PageUp` / `PageDown`. `/mouse` again turns capture back on.

## Notifications

When the terminal is unfocused, an approval request or a finished run rings
the bell and posts a desktop notification (OSC 9) where the terminal
supports it.

## Running inside herdr

In a [herdr](https://herdr.dev) pane, `qq` reports its state to herdr
itself; there is nothing to install. herdr's sidebar and `herdr agent list`
show the pane as `qq` with one state for every session the window follows:

| herdr state | When |
| --- | --- |
| `blocked` | any session has an approval or question waiting for you (the message names it) |
| `working` | otherwise, any session is running, queued, or answering a side question |
| `idle` | nothing is running |

herdr's notifications and `herdr agent wait` follow these states. The
focused session's root is reported for restore: after a herdr server
restart the pane runs `qq --session ID` and reopens it, with a sub-agent
session reopening as its tree. Quitting `qq` clears the pane.

Reports go to the socket herdr names in `HERDR_SOCKET_PATH`, only when
`HERDR_ENV=1` and `HERDR_PANE_ID` are set, and never delay the TUI: each
waits at most half a second and a failed one is dropped. `qq serve` and
`qq run` never report. Reporting needs a Unix socket, so it is off on
Windows.

## Exiting

`Ctrl-C` twice or `/quit`. The exit message prints the focused session id and
both ways to continue it.

What happens to running work depends on who owns the server:

- **This `qq` started it** (no server was running when it opened): quitting
  stops the server and cancels every queued and running run on it —
  including runs started from other `qq` windows attached to it — children
  included. Those windows lose their connection. The sessions, their
  history, and every tool result already recorded stay; resume with
  `qq --session ID` and send the next prompt.
- **It attached to an existing server** (a `qq serve`, or another open
  `qq`): quitting only disconnects. Runs keep going on that server.

To keep long runs alive while you close the terminal, start `qq serve` in
another terminal (or a multiplexer) first, then open `qq`.
