# Permissions and trust

QQ separates two questions:

- **Trust** — may this project's configuration influence QQ at all?
  Enabled explicitly for reviewed content with `qq trust`; task startup
  ignores sensitive project settings until then.
- **Approval** — may the agent take this action right now? Answered by the
  session's approval mode and the grants in effect. Only Ask asks you.

## Project trust

QQ starts tasks using trusted configuration and non-sensitive project settings.
It does not ask you to approve a project before Auto can work. Untrusted project
providers, models, MCP servers, packs, and permission grants stay disabled;
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
  declares: model, policy.allow_tools, policy.allow_shell_prefixes
```

`qq config sources` reports files awaiting trust. Explicit configuration inspection
commands still report pending trust so you can see which settings are withheld.
Task startup continues with the trusted settings instead of displaying a prompt.
Trust is stored in your QQ data directory, outside the project.

Sensitive settings include `model`, `worker_model`, `reviewer_model`,
`organization`, `providers`, `mcp`, `packs`, `profiles`, `delegation`,
`audit`, `approval_delegate`, `reasoning_effort`, and policy grants
(`allow_tools`, `allow_shell_prefixes`, `allow_hosts`, `shell_env`).
A project file that only sets `policy.exposed_tools` or `max_output_tokens`
loads without trust. Syntax, path boundaries, and managed policy still apply.

## Protected task files

Built-in `write_file` and `edit_file` refuse project configuration
(`.qq/config.ron`, `.qq/config.d/`, `.qq/packs/`, and `qq.ron`) and Git
trust files under `.git`, including nested repositories and symlink aliases.
Resolved configured pack directories inside the workspace (for example,
`vendor/reviewer`) and their aliases are protected too, even when that pack
is not selected for the current run. Unconfigured vendor directories remain
ordinary task files.
This applies in every approval mode; approving a file tool cannot grant it
permission to rewrite its own policy. Ordinary task files, `.github/`, and
`.qq/skills/` remain writable when the mode permits writes. A batch rejected
during planning changes no files; apply failures report any completed files.
Atomic writes pin the resolved parent directory and refuse a changed directory
namespace at apply-time validation. They do not follow replacement symlinks.
These checks do not lock out outside processes: another process can still
rename a directory after the final validation. They provide no OS isolation
for concurrent namespace changes, shell commands, or trusted MCP servers.

## Approval modes

Every session runs under one mode. Set the default in a profile
(`approval_mode`), change it in the TUI with `/approval`, or pass
`qq run --approval`.

| Mode | Reads | Edits and writes | Shell | MCP tools | `fetch` |
| --- | --- | --- | --- | --- | --- |
| `read_only` | run | denied | denied | denied | denied |
| `ask` | run | human approval each call | human approval each call | human approval each call | human approval each call |
| `auto` **(default)** | run | run in the private [task copy](#task-workspaces), published when the run completes | run within the native task boundary | run | public hosts run |
| `full` | run | run | run without Auto containment | run | public hosts run |

Every mode except Full denies the [fixed stops](#fixed-stops) without asking.

Auto does not ask a human or consult a reviewer model for ordinary actions,
including actions taken by write sub-agents. Auto questions return a result
telling the agent to decide without human input. A denied operation returns a tool
error so the agent can choose another permitted approach. Denial does not grant
permission to bypass the boundary. Every denial reads
`permission_denied: <who>: <reason>; <what to do instead>`, followed by
"Do not retry it in another form." `<who>` is
`fixed_stop`, `user` (you said no, or nobody answered in time), `mode` or
`config`. Children remain subject to their parent's
permissions, including later revocation.

Ask requires fresh human approval for each non-read call. Stored grants and
`approval_delegate` cannot substitute for that approval. Headless Ask denies
immediately because no human is present. Approval in Ask applies once and does
not create a reusable grant.

### Native Auto execution

Shell and exec use the durable command worker. The worker retains command
identity, output and terminal status independently of the server connection.
Default Auto allows installed tools to read host files the user can already
read, and confines writes to the task workspace. Broad reads are not credential
privacy: readable host credential files remain readable.

Linux uses its native namespace and filesystem restrictions; macOS uses
Seatbelt. Windows uses a restricted caller token with workspace ACL grants.
The Windows baseline also permits writes to objects already writable by
Everyone or the logon SID, so it is weaker than Linux/macOS workspace-only
writes. Windows currently uses host networking; Linux/macOS request isolated
shell networking. `fetch` has a separate public-network policy.

Normal tool environment and HOME configuration are preserved. Credential-like
environment names are excluded unless explicitly requested through
`policy.shell_env`. Temporary files go into the writable task directory.
Default Auto adds no fixed per-command memory quota or cgroup setup requirement.
Embedders can explicitly request resource limits when their host supports them.

An embedding that does not supply a native task scope runs shell commands in
Auto without that boundary, subject only to the fixed stops below.

Shell needs operating-system support on the host, in every mode that runs
commands, and QQ has no unsandboxed fallback. On Linux that means a
non-root user, unprivileged user namespaces, and a root-owned `unshare`;
Auto also needs Landlock ABI 6 (Linux 6.12+). The full list is in
[Install › What the shell sandbox needs](install.md#system-requirements).

### Task workspaces

An Auto root run works in a private copy of the project, not the checkout
itself. When the run starts, QQ copies the project's regular files into a
task directory under its data directory, skipping files matched by
`.gitignore`, `.ignore`, or `.qqignore`, the `.git` directory, symbolic
links, special files, and project trust configuration. Built-in edits and
shell commands, including those of child sessions, all see that copy.

When the run completes, QQ publishes the changed files back to the
checkout. Before installing each one it checks that the checkout's version
is still the one the task started from, so edits you made meanwhile are kept
rather than overwritten. A failed, cancelled, or interrupted run publishes
nothing; its work stays in the task directory, and the error names the path.

A task copy holds at most 16,384 entries (files plus the directories created
for them) and 256 MiB in total; the limits are not configurable. Larger
projects fail before the run starts with `task preparation failed: task
workspace limit exceeded`. Build output such as `target/` or `node_modules/`
counts unless an ignore file excludes it. Ask and Full runs use the checkout
directly and have no task copy.

### Task commits, pushes, and pull requests

In an Auto root run inside a Git checkout, the agent also has three tools:

| Tool | Effect |
| --- | --- |
| `task_commit` | commits the task's changed files to a private branch, `qq/task/<task>`, in QQ's own task repository. It never touches your refs, index, or Git config and runs no hooks, filters, or signing. Children must finish first |
| `task_push` | pushes that branch to the GitHub repository a host `policy.task_hosting` grant names for this workspace |
| `task_pr` | opens or updates the pull request for the pushed branch against the grant's base branch |

`task_push` and `task_pr` refuse without a matching grant; nothing prompts
for one. Grants, and the stored GitHub token they name, live only in host
configuration; see [`policy.task_hosting`](configuration.md#policy). The
tools need an absolute `git` on `PATH` and a checkout whose `HEAD` is a
commit.

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
subshells and `sh -c '…'`. No grant lifts one; switch to Full if you mean
it.

This is an accident net, not a security boundary. It reads literal
commands. A script, an interpreter, or a command built at run time can do any
of these things without matching. Managed `deny_shell_prefixes` add literal
prefixes that are denied in every mode, Full included.

## Undoing a run

Before a run first changes files in a Git work tree, QQ snapshots the
workspace. `qq undo`, or `/undo` in the TUI once every run is idle,
restores the newest run's snapshot, and the undo can itself be undone. See [`qq undo`](cli.md#qq-undo---list-snapshot).

## The approval prompt

Only Ask, and explicitly supervised delegation outside Auto, put a call on
hold for a human. Auto never does. An Ask prompt looks like this:

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

Ask approvals apply once and never create a reusable grant. A held call
outside Ask also offers `a` (allow this shape for the rest of the session)
and `w` (also write the grant into `.qq/config.ron`), when the grant fits.

Edits show a diff; `fetch` shows the URL and whether a grant covers the
host; MCP calls show the server, tool, and arguments.

When several sessions run at once, `Alt-A` / `Alt-D` approve or deny the
oldest waiting call in another session without leaving yours, and
`/attention` lists everything waiting.

A prompt waits for you. There is no server-side timer that denies it while
you are away: the hold ends when you answer, when the run's own deadline
(`--max-duration` or `RunLimits`) cancels the run, or when you cancel. If
you want a bound anyway — a shared server, an unattended supervisor — set
`approval_timeout_seconds` in configuration and the call is denied
`denied_timeout` after that many seconds, counted from when you were
actually asked. Auto never enters this human wait. Headless `qq run --approval ask` denies immediately
because no human is available.

## Grants in configuration

Grants are standing authority in the `policy` section of any configuration
layer (the `w` key at a supervised prompt appends to the project's
`.qq/config.ron`). Native Auto does not need them for ordinary work; they
matter for explicit embeddings, delegated children, and hosts:

```ron
policy: (
    allow_tools: ["edit_file"],
    allow_shell_prefixes: ["cargo test", "npm install"],
    allow_hosts: ["docs.rs"],
)
```

You can write the same by hand in any layer. Grants merge across layers;
`Remove("cargo test")` in a higher layer drops one a lower layer added; a
managed layer's `deny_shell_prefixes` removes it no matter who declared it.

Headless runs take grants on the command line:

```sh
qq run --approval auto --allow-shell "cargo test" --allow-tool write_file --allow-host crates.io "…"
```

## Tool catalog vs. approval

Approval decides whether a call *runs*. `policy.exposed_tools` and a pack
profile's `tools: (allow: …, deny: …)` decide whether the model *sees* a
tool at all. A tool removed from the catalog cannot be granted back with
`--allow-tool`; the model does not know it exists. Use the catalog for
"never", approval for "ask me".

## Network

`fetch` is refused for private, link-local, and cloud-metadata addresses
under every mode. `policy.allow_hosts` (exact names or `*.suffix`) lets
an unscoped caller record a host grant; native Auto can fetch public hosts
without a per-host grant. A managed `deny_hosts` wins.

## Where the rules live

The effect classes and execution details are in
[`../design/tools.md`](../design/tools.md). Historical ADRs record earlier
classifier and reviewer designs; the current behavior is described above.

Parent permissions cap child authority. An Ask parent requires fresh human
approval for each child action, while a read-only parent prevents mutation.
Changing the task or parent policy invalidates pending approval authority;
a late response cannot execute an action that is now denied.
