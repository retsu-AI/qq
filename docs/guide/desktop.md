# QQ desktop on Windows

QQ desktop connects to the same local QQ server as the terminal. Sessions,
credentials, trust and agent execution belong to that server. When needed, the
app starts a bundled engine and stops that supervised engine when it exits.
An engine already running independently remains independent.

## Start working

Open a project folder with Open project or Ctrl O. Connect a provider using
Providers, select a model, and review any project-configuration trust prompt.
Trust grants use the same host configuration path as `qq trust`; never trust a
project whose declared commands or services you do not accept.

Write a task and press Enter; Shift Enter adds a newline. A failed or uncertain
submission retains your draft. On an acknowledgement timeout, inspect session
history before retrying, because the server may already have accepted the task.
Disconnected/replaying states pause sending. Engine startup errors include a
Retry connection button; Settings exposes connection and tool-host details.

A new or empty session offers starting points: explain the project, review
your changes (`@diff`), fix a bug, or add tests. Choosing one fills the composer
for you to finish; nothing is sent until you send it.

## Goals

A goal keeps QQ working on a session across runs until it is achieved, a
budget runs out, or it needs you (see [Session goals](tui.md#session-goals)
for how the server pursues one). The **Goal** button in the session header
opens the goal panel: objective, why it stopped, checklist, notes, runs,
deadline, tokens and cost used (unknown stays labelled unknown), waits, the
completion check and its last output. Pause, Resume, Edit and Clear act on the
revision you are looking at; if the goal changed meanwhile, the command is
refused instead of applied to the newer goal.

Set a goal from the panel or with `/goal <objective>`; `/goal` alone opens the
panel and `/goal edit`, `/goal pause`, `/goal resume` and `/goal clear` do what
they say. The dialog takes the objective, an optional completion check command
and a budget: the workspace's configured defaults, Focused (up to one hour and
20 runs) or Extended (the workspace maximum); editing can keep the current
deadline and limits. Replacing an existing goal, including an achieved one,
asks you to confirm which goal it replaces. Budgets come from the `goals`
section of the project's configuration and are capped by its ceilings.

A check command never runs on an ordinary approval. When the goal needs it,
the panel asks for consent to that exact command: **Deny**, **Allow once** or
**Allow for this goal**. Changing the goal or the command asks again. If a
check is interrupted (for example by a restart), a banner holds new work in
the workspace until you review what it may have changed and acknowledge it;
acknowledging does not resume the goal.

## Sessions and run controls

Search sessions in the sidebar (Ctrl F; Esc clears). Each session's ⋯ menu
renames it, archives it, or deletes it; double-click a name to rename it in
place. Names and archive flags marked **locally** are desktop preferences; other
QQ clients still see canonical session names/history. The archive button beside
search lists archived sessions, where Restore reverses it. Working sessions and
sessions that need you continue to surface. Delete permanently removes an idle
server session after confirmation; cancel an active run first. The project name
at the top of the sidebar switches, opens and closes projects, and shows how many
sessions in other projects need you.

While an agent works, choose how the next message is delivered:

- Queue follow-up creates a later run.
- Steer at next boundary supplies input to the current run at its next safe point.
- Interrupt and steer aborts the current model/tool operation before continuing
  with your input. Previously produced output remains in history.
- Stop cancels the run. Escape in the composer also stops it.

Existing sessions have profile, model, effort and approval controls. Profile,
model and effort apply to the next claimed run. Approval delegate applies to the
next held tool call; Human only disables the reviewer override. Configured default
restores configuration behavior. Only advertised effort values are offered.

Approval cards show the requested action and its preview. Use the buttons or
Y (once), S (session), W (workspace), N (deny) when a card is selected and no
dialog or editable control owns focus. Multiple cards do not all respond to one
shortcut. Answer question cards using their own controls.

## File context and history

Type `@` in the composer (or press its @ button) to pick a file or folder from a
list that filters as you type; arrow keys move, Enter or Tab inserts, Escape
closes. Choosing a folder keeps the list open inside it. Suggestions are the
TUI's `@` completions (ignore-aware, ranked, time-bounded), and `@diff`,
`@web:` and `@sha:` are offered too. The shared TUI mention syntax, including
line ranges such as `@src/main.rs:10-40`, also works when typed by hand.
QQ enforces workspace containment and file/output bounds. Unresolvable references
retain your draft with an explanation. This app currently accepts text and
workspace files; arbitrary binary/image uploads are outside this release.
Drag files from Explorer onto the window to mention them at the cursor. Only
files inside the project can be mentioned, and mention paths cannot contain
spaces; skipped files are listed under the composer. The ring beside Send shows
how much of the model's context the session uses (hover for tokens and cost);
compact the session from Ctrl K when it fills up.

History follows streamed output until you scroll upward. Latest resumes following.
Focus the history region and use Ctrl Home/Ctrl End for its beginning/end.
Rendering mounts a bounded window of the native retained snapshot. Older history
outside that snapshot is not deleted, but a new paging protocol is not provided.
Diffs, tool output, reasoning and child-session links retain their existing controls.

Code blocks in replies are syntax highlighted and have a Copy button. Hover a
message to copy it; your own messages can also be put back in the composer to
edit, and Retry sends a prompt again as a new turn (history is kept, nothing is
replaced). A failed run offers Retry next to its error. Finished tool calls fold
into one summary line, such as "Ran 3 commands, read 2 files"; select it to see
each call. Edits stay visible with their diff.

Changes (the header button, or Ctrl Shift D) opens a side panel listing every file
the session's file tools edited, with added and removed line counts and each
diff. Files changed by shell commands are not tracked there.

Type `/` at the start of a message to pick one of the workspace's commands or
skills (`.qq/commands/<name>.md`, `.qq/skills/<name>/SKILL.md`); it is sent as
`/name` and resolved by the runtime, as in the TUI. `/btw QUESTION` (or the
speech-bubble button) asks a side question while the agent keeps working: the
answer appears above the composer and never becomes part of the run.
`/btw-new` starts a fresh side thread and `/btw-cancel` stops the running one.
The same limits as the TUI apply (read-only file tools, 8 model turns, 120 s).

The header's code button (or Ctrl Shift E) opens the project in VS Code,
Cursor, Windsurf or Zed, whichever is found first on `PATH`; the Changes panel
opens single files the same way.

The Git chip in the header shows the branch, how many files changed, commits
to push or pull, and the branch's pull request when the GitHub CLI (`gh`) finds
one. Its menu commits everything (optionally pushing), pushes or publishes the
branch, and creates a pull request with `gh`, each after a confirmation. Status
refreshes every 15 seconds, on window focus and when a run settles; Git hooks
and credential helpers run as usual, and prompts are disabled so nothing hangs.

New worktree session (Ctrl Shift N, the File menu, the Git menu or the project
switcher) gives a task its own checkout and branch so several agents can work
on one repository at once. It runs `git worktree add` into
`<repo>.worktrees/<branch>` beside the repository (never inside it, where the
main checkout's agent would see it) and opens that folder as a project with
the task waiting in its composer. The branch name is derived from the task and
can be changed; an existing branch is checked out as it is. Worktrees appear
under their repository in the project switcher. "Remove this worktree" deletes
the folder and keeps the branch; Git refuses while it has uncommitted changes
unless you choose to discard them. The new folder may ask for trust again
because QQ trusts configuration per folder.

## Window, menus and settings

The title bar holds back/forward through recently viewed sessions (Alt ←/→),
the sidebar toggle (Ctrl B) and the File, Edit, View and Help menus (Alt F/E/V/H).
View switches theme and zooms text (Ctrl +, Ctrl -, Ctrl 0). Drag the empty part
of the bar to move the window; double-click it to maximize. The window reopens
at the size and position it closed with, maximized if it was.

Ctrl , opens theme, transcript/composer text size, optional notifications,
keyboard shortcuts, connection diagnostics and reported external-tool status.
Notifications appear for approval/completion while the app is unfocused; Windows
requires an installed app. Notification text includes session names. MCP/runtime
configuration stays in QQ config.ron and uses the normal project trust workflow.

Ctrl K opens desktop commands: new/root/child sessions, context compaction,
rollback, pruning empty idle sessions, provider setup and settings. Compaction
changes model context while leaving the displayed transcript intact. The desktop
palette exposes implemented desktop actions; it does not execute arbitrary CLI text.

Ctrl N starts a new session, Ctrl Shift A moves to a session needing attention,
and Ctrl Shift D shows or hides the Changes panel.
Opening QQ again focuses the existing window and forwards project-folder arguments.
Closing a project disconnects its client without deleting server history.

## Installation and updates

Build instructions and NSIS packaging are in
[`apps/desktop/README.md`](../../apps/desktop/README.md). The installer is per-user
and carries qq.exe. Manual upgrades use the same application identity; automatic
updates await a signed distribution channel. Local builds are unsigned; signing
requires a certificate supplied by the release owner. See the README's signing
procedure before distribution.
