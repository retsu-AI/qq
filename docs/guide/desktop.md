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

## Sessions and run controls

Search sessions in the sidebar. Names and archive flags marked **locally** are
desktop preferences; other QQ clients still see canonical session names/history.
Archive is reversible under Archived locally. Working sessions and sessions that
need you continue to surface. Delete permanently removes an idle server session
after confirmation; cancel an active run first.

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

History follows streamed output until you scroll upward. Latest resumes following.
Focus the history region and use Ctrl Home/Ctrl End for its beginning/end.
Rendering mounts a bounded window of the native retained snapshot. Older history
outside that snapshot is not deleted, but a new paging protocol is not provided.
Diffs, tool output, reasoning and child-session links retain their existing controls.

## Window, menus and settings

The title bar holds back/forward through recently viewed sessions (Alt ←/→),
the sidebar toggle (Ctrl B) and the File, Edit, View and Help menus (Alt F/E/V/H).
View switches theme and zooms text (Ctrl +, Ctrl -, Ctrl 0). Drag the empty part
of the bar to move the window; double-click it to maximize.

Ctrl , opens theme, transcript/composer text size, optional notifications,
keyboard shortcuts, connection diagnostics and reported external-tool status.
Notifications appear for approval/completion while the app is unfocused; Windows
requires an installed app. Notification text includes session names. MCP/runtime
configuration stays in QQ config.ron and uses the normal project trust workflow.

Ctrl K opens desktop commands: new/root/child sessions, context compaction,
rollback, pruning empty idle sessions, provider setup and settings. Compaction
changes model context while leaving the displayed transcript intact. The desktop
palette exposes implemented desktop actions; it does not execute arbitrary CLI text.

Ctrl N starts a new session, and Ctrl Shift A moves to a session needing attention.
Opening QQ again focuses the existing window and forwards project-folder arguments.
Closing a project disconnects its client without deleting server history.

## Installation and updates

Build instructions and NSIS packaging are in
[`apps/desktop/README.md`](../../apps/desktop/README.md). The installer is per-user
and carries qq.exe. Manual upgrades use the same application identity; automatic
updates await a signed distribution channel. Local builds are unsigned; signing
requires a certificate supplied by the release owner. See the README's signing
procedure before distribution.
