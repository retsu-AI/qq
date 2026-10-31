# The TUI

`qq` opens the terminal UI in the current directory. It attaches to the
user's running server if there is one — a `qq serve`, or the server another
open `qq` started — and otherwise starts one inside itself that lives
exactly as long as that `qq` does (see [Exiting](#exiting)). Everything
below works the same either way.

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
  `Ctrl-Up` / `Ctrl-Down` select a row; `Enter` expands it (a read shows the
  result head, an edit its diff, a command its output tail, an MCP call its
  arguments). `Ctrl-O` folds each finished turn's routine calls into one
  receipt row (`● 6 calls · read 2 · edited 2 · ran 2   2 files`); edits
  and failures keep their rows. `Alt-R` shows or hides reasoning, which
  otherwise reads `∴ thought for 12s`.
- **Sessions column** (90 columns and wider, with two or more sessions;
  `Ctrl-\` toggles): sessions numbered 1–9 and grouped by what you should
  do — NEEDS YOU, WORKING, RECENT — with `N new` for unread replies and
  `↳N` for child agents. From 160 columns each session gets a second row
  with its activity and cost. Below 90 columns the column is gone and the
  rule carries the counts instead (`8 sessions  ◐ 2  ◇ 2  ^G`).
- **Rule**: the focused session's state and activity (`◐ editing · 14s`),
  or the last notice. Its color follows the session: dim when idle, blue
  while running, yellow when it needs you, red after a failure. On the
  right, `◇ N needs you ^G` counts approvals waiting in other sessions.
- **Panel**: approvals, questions, `/goal`, and every picker (`/sessions`,
  `/models`, `/theme`, …) open here, between the rule and the composer, and
  take at most half the screen. `Esc` closes them with your draft intact.
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
| new session | `Alt-N` | `/new`, `/clear` |
| new child session (sub-agent under the focused one) | `Alt-C` | |
| open the session list; type to search names | | `/sessions`, `/resume` |
| the focused session's agent tree | | `/agents` |
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
transcript. `/btw` without a question reopens the view. `/btw-new QUESTION`
starts a new side thread; otherwise questions continue the existing thread.
`/btw-cancel` cancels only the active side question, not the main agent.

Each question captures bounded committed session context. Unfinished tool
exchanges are omitted explicitly. Only built-in `read_file`, `search`, and
`tree` are available, further restricted by the session's tool policy. Live
file inspection is not a consistent filesystem snapshot. Side questions,
answers, and usage never become main-agent prompts, steering, or goal budget.

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
  ┃ y apply   n deny   Y / N with a note
    Session 0 · gpt-test                                                    /workspace
```

`y` / `n`; `Shift-Y` / `Shift-N` decide and steer with a note.
`Esc` denies, like `n`, except while a note is being typed, when it drops the
note and leaves the prompt open.
`Alt-A` / `Alt-D` answer the oldest waiting call in *another* session.
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
  qq 0.1.11 · ~/qq  main · gpt-5.5 high

  /models  choose a model    @path  mention a file    ^T  switch session    ?  help

  QQ is on the host: commands run as you, with this machine's network and credentials.
  Snapshots on: `qq undo` restores the workspace as it was before each run.
```

The last two lines say where the agent's commands run (the host, or a
container) and whether `qq undo` can restore this workspace. They also show
before the first session exists. To isolate the agent, see
[Running unattended](running-unattended.md).

`/undo` restores the newest run's snapshot of this workspace through the
server, which refuses while any run in the workspace is active, queued or due
from a goal, and starts none until the restore finishes. `qq undo NAME`
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
- `Ctrl-R` searches your prompt history for this session.
- `/compact` summarizes an idle session's history so a long conversation
  keeps fitting; `/rollback` undoes the newest compaction. Stale read-only
  tool results are pruned from the model's context automatically.

## Session goals

`/goal` opens the focused session's durable goal in the dock. It shows the
objective, status, run/token/cost budgets, UTC deadline, audits, check
command, notes, waits, checklist, verification state and last check output. Unknown usage and cost remain labelled unknown. Check
consent distinguishes one execution, repeated checks for this goal, and a used
one-shot grant that needs renewed consent.

The rule keeps the goal's run allowance and next wake visible while active
(`goal run 4/10 · next 5m`), and shows the reason when it stops. After a
user pause, it shows cancellation draining until the pending run settles,
then `/goal resume`.

The view follows committed snapshots and live updates. Scroll to read long
check output; `Esc` or `/goal` returns to the transcript. Opening it does not
start or resume work. An interrupted-check workspace hold stays visible even
when the original goal is cleared or replaced. Resolve its uncertain effects
before acknowledging it through the API.

Use `/goal <objective>` to open the creation form, or `/goal edit` to edit the
current goal. Tab/Shift-Tab or the up/down arrows move between objective,
optional check command and budget preset. Left/right arrows change the preset:
configured defaults, focused (up to one hour and 20 runs), or the workspace
maximum. Editing starts with the existing deadline and limits; changing its
preset updates deadline/run allowance while keeping its other limits.

Enter submits the form. Replacing an existing goal, including editing an
achieved one, first shows a confirmation identifying the previous objective;
only `y` confirms. Enter does not confirm replacement. `n` returns to editing,
and Esc cancels. PgUp/PgDn scroll long specifications. A check command does
not grant execution consent. Editing a stopped goal does not resume it.

The form captures the current goal revision. A changed goal must be reopened
and reviewed. Submission errors retain the draft and refresh authoritative
state; if the refresh fails, Enter retries the refresh before another explicit
submission. No conflict silently retries against a newer revision.

Use `/goal pause`, `/goal resume`, or `/goal clear` to control the observed
goal revision. State changes appear after the server commits them. A conflict
or failed command is reported without automatically retrying a newer goal.
If the session body has not loaded, retry after its snapshot arrives.

`/goal check` opens consent for the exact displayed command and workspace.
Choose **Deny**, **Allow once**, or **Allow repeated checks for this goal**,
then press Enter. Deny is selected initially; Esc cancels. PgUp/PgDn scroll
long command details while the choices stay visible. Changing the goal,
command or workspace invalidates an open panel. Ordinary tool approval settings
do not grant check consent.

## Files the agents changed

`/changes` lists every file edited in this workspace by any session,
flagging files touched by more than one. `Alt-I` shows the inspector pane
with the focused session's details (it stays off until you press it).

## Every command

`?` on an empty composer, `F1`, or `/help` shows this list grouped by area
with your current bindings. `Ctrl-K` or `/commands` opens the same list as a
searchable palette that runs the highlighted command on `Enter`.

| Command | Slash | Default key |
| --- | --- | --- |
| show every command and key | `/help` | `F1`, `?` |
| open the command palette | `/commands` | `Ctrl-K` |
| open sessions | `/sessions`, `/resume` |  |
| open the focused session's agent tree | `/agents` |  |
| toggle the session navigator |  | `Ctrl-T` |
| create a session | `/new`, `/clear` | `Alt-N` |
| create a child session |  | `Alt-C` |
| compact session context | `/compact` |  |
| undo the newest compaction | `/rollback` |  |
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
| toggle tool call detail |  | `Ctrl-O` |
| select the previous tool call |  | `Ctrl-Up` |
| select the next tool call |  | `Ctrl-Down` |
| toggle reasoning detail |  | `Alt-R` |
| toggle the session sidebar |  | `Ctrl-\` |
| toggle the inspector pane |  | `Alt-I` |
| toggle mouse capture | `/mouse` |  |
| show everything that needs you | `/attention` |  |
| show every file agents changed | `/changes` |  |
| view session goal and progress | `/goal` |  |
| ask or view side questions | `/btw`, `/ask` |  |
| ask in a new side thread | `/btw-new` |  |
| cancel only the side question | `/btw-cancel` |  |
| edit the draft in $EDITOR | `/editor` | `Alt-E` |
| search prompt history |  | `Ctrl-R` |
| exit QQ | `/quit`, `/exit` | `Ctrl-C` |

Keys shown beside `?`, `Enter`, `Esc`, and `Esc Esc` apply only in context:
`?` on an empty composer, `Enter` steers only while a run is active, `Esc`
focuses the parent only when there is no overlay, approval, or
question prompt, `@` completion list, transcript selection, open view, error notice,
or active run to claim it, and `Esc Esc` cancels only while running.
Rebind `toggle_navigator`, `create_root_session`, `create_child_session`,
`cancel_run`, and `interrupt_run` in [`tui.ron`](configuration.md#tuiron);
every hint that mentions a rebound key updates.

Scrolling: mouse wheel, `PageUp` / `PageDown`, `Shift-Up` / `Shift-Down`,
`Ctrl-Home` / `Ctrl-End` for top and live tail. Hold `Shift` to select text
with the mouse, or `/mouse` to hand the mouse back to the terminal.

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

`Ctrl-C` or `/quit`. The exit message prints the focused session id and
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
