# The TUI

`qq` opens the terminal UI in the current directory. It attaches to the
user's running server if there is one — a `qq serve`, or the server another
open `qq` started — and otherwise starts one inside itself that lives
exactly as long as that `qq` does (see [Exiting](#exiting)). Everything
below works the same either way.

## Layout

```
 qq  my-project › fix parser                    anthropic/claude-sonnet-5  12% ctx  $0.04
 ─────────────────────────────────────────────────────────────────────────────────────────
 │ NEEDS YOU                 │  you › Fix the failing test in parser.rs
 │   ◇ add-tests   approval  │
 │ WORKING                   │  ● Read   src/parser.rs                          0.1s
 │   ● fix parser            │  ● Search "fn parse_duration"                    0.0s
 │ IDLE                      │  ● Edit   src/parser.rs  +4 −1                   0.3s
 │   refactor cli            │  ● Shell  cargo test -p parser                   4.1s
 │                           │
 │                           │  The test expected seconds but the parser…
 ─────────────────────────────────────────────────────────────────────────────────────────
 running · 12s · first token 0.8s                         ? help  ^K commands  ^O detail
 › Ask QQ...
```

- **Top row**: workspace and session breadcrumb; on the right the model,
  `as <profile>` when not default, the approval mode when not `auto`,
  context used, cost, and `connecting` / `offline` when relevant.
- **Sidebar** (100 columns and wider; `Ctrl-\` toggles): sessions grouped by
  what you should do — NEEDS YOU, WORKING, IDLE, DONE — with unread counts.
  Below 100 columns a one-line agent strip above the composer carries the
  same counts.
- **Transcript**: your prompts, the model's text, and one row per tool call
  with live elapsed time. `Ctrl-Up` / `Ctrl-Down` select a row; `Enter`
  expands it (a read shows the result head, an edit its diff, a command its
  output tail, an MCP call its arguments). `Ctrl-O` folds finished blocks
  to one row each; `Alt-R` shows or hides reasoning.
- **Rule**: running activity, elapsed time, time to first token, or the last
  notice; key hints for the current state at the right. The help hint reads
  `? help` while the composer is empty and `F1 help` once you have typed.
- **Composer**: type and press `Enter`. `Shift-Enter` or `Alt-Enter` inserts
  a newline. `Alt-E` or `/editor` opens the draft in `$EDITOR`.

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

Steering appears in the transcript as a `steering` row until applied.

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
session switched to `ask` with `/approval`, every non-read call waits:

```
◇ approval needed
$ cargo test  (in ~/my-project)
y once   n deny   ask: approval applies once
```

`y` / `n`; `Shift-Y` / `Shift-N` decide and steer with a note.
`Esc` denies, like `n`, except while a note is being typed, when it drops the
note and leaves the prompt open.
`Alt-A` / `Alt-D` answer the oldest waiting call in *another* session.
`/attention` lists everything waiting across the workspace. Details in
[Permissions and trust](permissions.md#the-approval-prompt).

When the agent asks *you* a question (`◇ question`), type an answer and
press `Enter`, press a number to pick an option, or `Esc` to decline.

## Choosing model, profile, mode, theme

| Picker | Slash | What it applies to |
| --- | --- | --- |
| model | `/models` | the focused session, or the default for the next `/new`; `Ctrl-N` inside the picker creates a session with the highlighted model |
| agent profile | `/profile` | the focused idle session, or the default for new sessions; top row shows `as NAME` |
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
  sensitive project settings stay disabled until you run `qq trust`.
- **No `model` configured**: the top row reads `no model` and the composer
  rule reads `choose a model with /models` until you pick one. `Enter` in
  the picker creates the first session with the highlighted model, which
  also becomes the default for `Alt-N`.
- **Model configured, provider has no credential**: the empty transcript
  reads `openai needs a credential: run qq auth login openai or set
  OPENAI_API_KEY` (naming your provider) above `Alt-N creates the first
  session.`; `Alt-N` repeats the same line as a warning. Add the credential
  in another terminal and start `qq` again — the credential check runs at
  startup.

## Trust

The TUI starts without a project-trust prompt. Sensitive project models,
providers, MCP servers, profiles, and packs stay disabled until you explicitly
review and accept them with `qq trust` on the server host. Configure your model
and connections globally to make them available before project trust.

Trust covers the exact sensitive content. Changing that content disables the
changed settings on subsequent loads until you trust it again. See
[Permissions and trust](permissions.md#project-trust).

## Your first session

The first session in a workspace opens with a short list in the transcript:

```
  Try one of these:
    /models    choose a model
    /approval  choose an approval mode
    /skills    list workspace commands and skills
    @path      mention a file in your prompt
```

It disappears as soon as you send your first prompt and never shows for a
second session; those read `Ask QQ to begin this session.` instead. When the
configured provider still needs a credential, that remedy is printed above
the list.

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

`/goal` opens the focused session's durable goal. It shows the objective,
checklist, notes, run/token/cost budgets, UTC deadline, waits, verification state
and last check output. Unknown usage and cost remain labelled unknown. Check
consent distinguishes one execution, repeated checks for this goal, and a used
one-shot grant that needs renewed consent.

The composer status line keeps the goal's run allowance, time left and next
wake visible while active, and shows the reason when it stops. After a user
pause, it shows cancellation draining until the pending run settles, then
`/goal resume`.

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
flagging files touched by more than one. `Alt-I` opens the inspector pane
with the focused session's details.

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
focuses the parent only when there is no overlay, trust, approval, or
question prompt, `@` completion list, transcript selection, open view, error notice,
or active run to claim it, and `Esc Esc` cancels only while running.
The keys for creating sessions, the navigator, cancel, and interrupt are
defaults; rebind them in [`tui.ron`](configuration.md#tuiron).

Scrolling: mouse wheel, `PageUp` / `PageDown`, `Shift-Up` / `Shift-Down`,
`Ctrl-Home` / `Ctrl-End` for top and live tail. Hold `Shift` to select text
with the mouse, or `/mouse` to hand the mouse back to the terminal.

Rebind `toggle_navigator`, `create_root_session`, `create_child_session`,
`cancel_run`, and `interrupt_run` in [`tui.ron`](configuration.md#tuiron);
every hint that mentions a rebound key updates.

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
