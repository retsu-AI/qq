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

\* `Esc` focuses the parent only when nothing else claims it. At an
approval prompt it denies the call and at a question it declines, so it
never moves focus there. Otherwise it first closes an open `@` completion
list, clears a transcript selection, closes a workspace view, dismisses an
error notice for the focused session, and while a run is active arms
`Esc Esc` to cancel it. Informational notices do not claim it. `Alt-Up` is not a
focus key; it pulls back the newest queued draft.

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

## Approvals and questions

```
◇ approval needed
$ cargo test  (in ~/my-project)
asks because: unlisted_program
y once   a session   w workspace   n deny
```

`y` / `a` / `w` / `n` (`Esc` also denies); `Shift-Y` / `Shift-N` decide and steer with a note.
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
| approval delegate | `/delegate` | the focused session from its next held call, running or not: `configured` (the workspace's `approval_delegate`), `by_mode`, `on`, or `off` — the "stop delegating" switch. Nothing is written to config; children spawned afterwards inherit it. Top row shows `MODE · delegate off` while an override is set |
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

- **Project not yet trusted**: the transcript is the trust prompt (below);
  nothing else loads until you answer.
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

A repository that ships `.qq/config.ron` (or `qq.ron`, `.qq/config.d/`) with
a model, providers, MCP servers, grants, or packs is loaded only after you
accept it. Until then the transcript is the prompt and the composer is
disabled (`✎ Answer the trust prompt above`):

```
◇ this project's configuration needs your trust
  /home/you/repo/.qq/config.ron
    model anthropic/claude-sonnet-5
    MCP linear → https://mcp.linear.app/mcp
    grants: 2 tools, 3 shell prefixes
  t trust   s this session   q quit
```

| Key | Effect |
| --- | --- |
| `t` | record the files, exactly as `qq trust` does; the next launch does not ask |
| `s` | load them for this process only; nothing is written and the next `qq` asks again |
| `q`, `Esc` | quit without loading them |

Every other key is ignored while the prompt is up. After `t` or `s` the
notice reads `trusted N file(s)` or `trusted for this session`, and the
empty state continues as above (`no model`, a credential remedy, or `Alt-N
creates the first session.`). If this `qq` attached to a server on another
host, `t` and `s` say to run `qq trust` there instead: trust is a decision
about files on the host that runs the server. Either answer covers only the
content the prompt showed: if a listed file's sensitive sections change while
the prompt is open (a `git pull`, an editor save), nothing is trusted and the
prompt redraws with the new content to answer again. Details and what "sensitive"
means in [Permissions and trust](permissions.md#project-trust).

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
  skill from `.qq/skills/name/SKILL.md`; `/skills` lists them with their
  source. Typing after the slash filters by subsequence (`/mdl` finds
  `/models`).
- `Ctrl-R` searches your prompt history for this session.
- `/compact` summarizes an idle session's history so a long conversation
  keeps fitting; `/rollback` undoes the newest compaction. Stale read-only
  tool results are pruned from the model's context automatically.

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
| choose who settles held approvals | `/delegate` |  |
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
| edit the draft in $EDITOR | `/editor` | `Alt-E` |
| search prompt history |  | `Ctrl-R` |
| exit QQ | `/quit`, `/exit` | `Ctrl-C` |

Keys shown beside `?`, `Enter`, `Esc`, and `Esc Esc` apply only in context:
`?` on an empty composer, `Enter` steers only while a run is active, `Esc`
focuses the parent only when there is no approval or question prompt, `@`
completion list, transcript selection, open view, error notice, or active
run to claim it, and `Esc Esc` cancels only while running.
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
