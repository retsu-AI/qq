# The TUI

`qq` opens the terminal UI in the current directory. Everything below also
works against a server started with `qq serve`, locally or remote.

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
 running · 12s · first token 0.8s                        ^? help  ^K commands  ^O detail
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
  notice; key hints for the current state at the right.
- **Composer**: type and press `Enter`. `Shift-Enter` or `Alt-Enter` inserts
  a newline. `Alt-E` or `/editor` opens the draft in `$EDITOR`.

## Sessions

| Action | Key | Slash |
| --- | --- | --- |
| new session | `Alt-N` | `/new`, `/clear` |
| new child session (sub-agent under the focused one) | `Alt-C` | |
| open the session list; type to search names | | `/sessions`, `/resume` |
| the focused session's agent tree | | `/agents` |
| focus parent / first child / next / previous sibling | `Alt-Up`* / `Alt-Down` / `Alt-Right` / `Alt-Left` | |
| jump to the next session that needs you | `Ctrl-G` | |
| delete the highlighted session in the list (confirms) | `Ctrl-D` | |
| delete every empty session | | `/prune` |

\* `Alt-Up` edits the newest queued draft when one exists.

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

`y` / `a` / `w` / `n`; `Shift-Y` / `Shift-N` decide and steer with a note.
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

| Area | Slash | Default key |
| --- | --- | --- |
| help | `/help` | `F1`, `?` on empty composer |
| command palette | `/commands` | `Ctrl-K` |
| sessions list | `/sessions`, `/resume` | |
| agent tree | `/agents` | |
| session navigator | | `Ctrl-T` |
| new session | `/new`, `/clear` | `Alt-N` |
| child session | | `Alt-C` |
| compact / rollback | `/compact`, `/rollback` | |
| prune empty sessions | `/prune` | |
| next needs-you | | `Ctrl-G` |
| approve / deny elsewhere | | `Alt-A` / `Alt-D` |
| cancel run | | `Ctrl-X`, `Esc Esc` |
| interrupt and steer | | `Alt-S` |
| queue draft / edit queued | | `Ctrl-Enter`, `Ctrl-Q` / `Alt-Up` |
| model / profile / approval / theme | `/models`, `/profile`, `/approval`, `/theme` | |
| skills and commands | `/skills` | |
| tool detail / select call | | `Ctrl-O` / `Ctrl-Up`, `Ctrl-Down` |
| reasoning | | `Alt-R` |
| sidebar / inspector | | `Ctrl-\` / `Alt-I` |
| mouse capture | `/mouse` | |
| attention / changes | `/attention`, `/changes` | |
| external editor | `/editor` | `Alt-E` |
| prompt history | | `Ctrl-R` |
| quit | `/quit`, `/exit` | `Ctrl-C` |

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

`Ctrl-C` or `/quit`. Runs owned by the background server keep going; the
exit message prints the focused session id and both ways to continue it.
