# Permissions and trust

QQ separates two questions:

- **Trust** — may this project's configuration influence QQ at all?
  Enabled explicitly for reviewed content with `qq trust`; task startup
  ignores sensitive project settings until then.
- **Approval** — may the agent take this action right now? Answered by the
  session's approval mode. Only Ask asks you.

## Project trust

QQ starts tasks using trusted configuration and non-sensitive project settings.
It does not ask you to approve a project before Auto can work. Untrusted project
providers, models, MCP servers, and packs stay disabled;
QQ neither starts their commands nor records trust automatically.

Your global configuration, `QQ_CONFIG`, `QQ_CONFIG_CONTENT`, and managed
configuration remain trusted sources. Configure your model and connections
there when you want them available across projects.

A project can ship `.qq/config.ron`, `qq.ron`, `.qq/config.d/*.ron`, and
`.qq/packs/`. To enable its sensitive settings, review the content and explicitly
run `qq trust` in that project. Trust applies to the exact sensitive content;
changing it disables the changed settings until you explicitly trust them again.
Project pack entries must stay inside the project, including through symlinks.

```sh
qq trust
trusted /home/you/repo/.qq/config.ron
  declares: model, mcp
```

`qq config sources` reports files awaiting trust. Explicit configuration inspection
commands still report pending trust so you can see which settings are withheld.
Task startup continues with the trusted settings instead of displaying a prompt.
Trust is stored in your QQ data directory, outside the project.

Sensitive settings are `organization`, `model`, `worker_model`,
`delegation`, `audit`, `goals`, `reasoning_effort`, `providers`, `mcp`,
`profiles`, and `packs`. A project file that only sets
`policy.exposed_tools` or `max_output_tokens` loads without trust. Syntax, path boundaries, and managed policy still apply.

## Protected task files

Built-in `write_file` and `edit_file` refuse project configuration
(`.qq/config.ron`, `.qq/config.d/`, `.qq/packs/`, and `qq.ron`) and Git
trust files under `.git`, including nested repositories and symlink aliases.
Resolved configured pack directories inside the workspace (for example,
`vendor/reviewer`) and their aliases are protected too, even when that pack
is not selected for the current run. Unconfigured vendor directories remain
ordinary task files.
This applies in every approval mode; approving a file tool does not let it
rewrite its own policy. Ordinary task files, `.github/`, and
`.qq/skills/` remain writable when the mode permits writes. A batch rejected
during planning changes no files; apply failures report any completed files.
Atomic writes pin the resolved parent directory and refuse a changed directory
namespace at apply-time validation. They do not follow replacement symlinks.
These checks cover only the built-in file tools. Shell commands and trusted
MCP servers can still write these files, and another process can rename a
directory after the final validation.

## Approval modes

Every session runs under one mode. Set the default in a profile
(`approval_mode`), change it in the TUI with `/approval`, or pass
`qq run --approval`.

| Mode | Reads | Edits, shell, MCP tools and `fetch` |
| --- | --- | --- |
| `read_only` | run | denied |
| `ask` | run | your approval, each call |
| `auto` **(default)** | run | run |
| `full` | run | run, fixed stops included |

Every mode except Full denies the [fixed stops](#fixed-stops) without asking.
`deny_shell_prefixes` from any configuration layer, and managed
`deny_hosts`, apply in every mode, Full included.

Auto never asks a human, including for actions taken by write sub-agents.
Auto questions return a result telling the agent to decide without human
input. A denied operation returns a tool error so the agent can choose
another approach. Every denial reads
`permission_denied: <who>: <reason>; <what to do instead>`, followed by
"Do not retry it in another form." `<who>` is
`fixed_stop`, `user` (you said no, or nobody answered in time), `mode` or
`config`. Children remain subject to their parent's
permissions, including later revocation.

Ask requires your approval for each non-read call, and an approval applies
to that call only. Headless Ask denies immediately because no human is
present.

### Where commands run

`shell` and `exec` run in the session's working directory, which is your
checkout, as you, with QQ's full environment, your network and your real
HOME. Edits land in the checkout immediately. QQ does not copy the project,
isolate the network or apply an operating-system sandbox. Each command runs
in its own process group (a job object on Windows). A timeout, a
cancellation, or QQ itself exiting stops that group. A process that moves
itself to a new group or session (`setsid`, some daemons, `git gc --auto`, or
GNU `timeout`) leaves that group, but QQ still tears it down: on Linux a
subreaper reaps the escaped tree, on macOS QQ kills the descendants a process
snapshot finds (one that double-forks and exits first can still slip away),
and Windows jobs follow every descendant. A command that prompts on the
terminal (an SSH passphrase,
a Git credential prompt) waits until its timeout or a cancel, so set
`GIT_TERMINAL_PROMPT=0` or use non-interactive credentials. QQ reserves no
disk space for commands; a command that fills the disk can stop the session
store from writing.

That means a command can do anything you can do from your terminal,
including reading your credentials and pushing with your Git identity. For
stronger isolation, run QQ inside a container, a VM or a dedicated account;
[Running unattended](running-unattended.md) shows how. `qq doctor` says
which one you are in.

### Fixed stops

A few shell actions can't be undone and are almost never intended. Every mode
except Full denies them before running anything, and the agent gets a tool
error telling it to find another way:

- force-pushing or deleting remote refs (`git push --force`, `-f`,
  `--force-with-lease`, `--delete`, `+ref`, `:ref`);
- `rm -r`, `mv`, or `chmod -R`/`chown -R` on `/`, your home directory, or a
  directory directly under either;
- `sudo`, `doas`, `su`, `pkexec`;
- formatting or writing to a disk device; shutting down or rebooting;
- naming a credential file: `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.netrc`,
  `~/.config/gh`, `~/.docker/config.json`, `~/.kube`, `~/.git-credentials`,
  or QQ's own credential store.

The stops see through `cd x && …`, `env A=b …`, `timeout`, `nice`, pipes,
subshells and `sh -c '…'`. Switch to Full if you mean it.

This is an accident net, not a security boundary. It reads literal
commands. A script, an interpreter, or a command built at run time can do any
of these things without matching. `deny_shell_prefixes` add literal
prefixes that are denied in every mode, Full included.

## Undoing a run

Before a run first changes files in a Git work tree, QQ snapshots the
workspace. `qq undo` restores the newest run's snapshot, and the undo can
itself be undone. See [`qq undo`](cli.md#qq-undo---list-snapshot).

## The approval prompt

Only Ask puts a call on hold for a human. An Ask prompt looks like this:

```
◇ approval needed
$ npm install left-pad  (in ~/repo)
y once   n deny   ask: approval applies once
```

| Key | Effect |
| --- | --- |
| `y` | run this call once |
| `n` | deny; the model receives the denial as a tool error and continues |
| `Shift-Y` / `Shift-N` | decide and then steer the run with a note |
| `Esc` | deny, like `n`; after `Shift-Y` / `Shift-N` it first cancels the note and leaves the prompt open |

Edits show a diff; `fetch` shows the URL; MCP calls show the server, tool,
and arguments.

When several sessions run at once, `Alt-A` / `Alt-D` approve or deny the
oldest waiting call in another session without leaving yours, and
`/attention` lists everything waiting.

A prompt waits for you. There is no server-side timer that denies it while
you are away: the hold ends when you answer, when the run's own deadline
(`--max-duration` or `RunLimits`) cancels the run, or when you cancel. If
you want a bound anyway, for example on a shared server, set
`approval_timeout_seconds` in configuration and the call is denied
`denied_timeout` after that many seconds, counted from when you were
actually asked.

## Tool catalog vs. approval

Approval decides whether a call *runs*. `policy.exposed_tools` and a pack
profile's `tools: (allow: …, deny: …)` decide whether the model *sees* a
tool at all; the model does not know a removed tool exists. Use the
catalog for "never", approval for "ask me".

## Network

`fetch` is refused for private, link-local, and cloud-metadata addresses
under every mode, and for hosts in a managed `deny_hosts`. Shell commands
use your network directly; these rules do not apply to them.

## Where the rules live

The effect classes and execution details are in
[`../design/tools.md`](../design/tools.md). Historical ADRs record earlier
classifier, sandbox and reviewer designs; the current behavior is described
above.

Parent permissions cap child authority. An Ask parent requires fresh human
approval for each child action, while a read-only parent prevents mutation.
Changing the task or parent policy invalidates pending approval authority;
a late response cannot execute an action that is now denied.
