# Permissions and trust

QQ separates two questions:

- **Trust** — may this project's configuration influence QQ at all?
  Answered once per file content, in the TUI when it opens or with
  `qq trust`.
- **Approval** — may the agent take this action right now? Answered by the
  session's approval mode, the grants in effect, and, when neither decides,
  you.

## Project trust

A repository can ship `.qq/config.ron`, `qq.ron`, `.qq/config.d/*.ron`, and
`.qq/packs/`. Those files can declare providers, MCP servers that run
commands, sub-agent rosters, and grants that let tools run without asking.
QQ therefore refuses to load a project file that declares anything sensitive
until you have accepted that exact content. Every project `pack.ron` (under
`.qq/packs/` or named by a project `packs` entry) is sensitive on its own,
even in a repository with no other configuration, and changing one asks
again. A project `packs` entry must stay inside the repository: a path
that leads out of it (absolute or through `..`) or through a symbolic link
is a configuration error
and is never opened, because QQ lists those manifests before you have
trusted anything. Declare packs that live elsewhere in your own global
configuration instead. A `packs` entry in your own
global, `QQ_CONFIG`, or managed configuration is yours and loads without a
prompt, even when it points into a repository.

Bare `qq` opens the TUI and asks, listing each pending file and what it
declares:

```
◇ this project's configuration needs your trust
  /home/you/repo/.qq/config.ron
    model anthropic/claude-sonnet-5
    MCP linear → https://mcp.linear.app/mcp
    MCP executor → executor
    grants: 2 tools, 3 shell prefixes
  t trust   s this session   q quit
```

- `t` records the files exactly as `qq trust` does; the next launch does
  not ask.
- `s` loads them for this process only. Nothing is written; the next `qq`
  asks again. Runs you start in this TUI (and the embedded server that
  serves them) see the trusted configuration.
- `q` (or `Esc`) leaves without loading them.

Only names, commands, URLs, and counts are shown; open the file for the
rest. The prompt appears only when this `qq` owns the server: a client
attached to a server on another host cannot read or record trust for that
host's files and sees the error below instead.

`qq ask`, `qq run`, and `qq serve` do not prompt; they exit:

```
error: project configuration needs your trust before it is used:
  /home/you/repo/.qq/config.ron
Review the file, then run `qq trust` in this directory to accept it. Sensitive
sections (providers, MCP servers, grants, model) load only after that.
```

Sensitive means any of: `model`, `worker_model`, `reviewer_model`,
`organization`, `providers`, `mcp`, `packs`, `profiles`, `delegation`,
`audit`, `approval_delegate`,
`reasoning_effort`, or a `policy` grant (`allow_tools`,
`allow_shell_prefixes`, `allow_hosts`, `shell_env`).
A project file that only sets `policy.exposed_tools` or `max_output_tokens`
loads without trust.

```sh
qq trust
trusted /home/you/repo/.qq/config.ron
  declares: model, policy.allow_tools, policy.allow_shell_prefixes
```

Trust is recorded as a digest of the file's sensitive sections in your data
directory, not in the repository. Any edit to a sensitive section of a
trusted file — yours or from `git pull` — makes QQ ask again (in the TUI, or
as the error above); a session-only grant expires the same way. `qq config
sources` shows `pending trust:` lines for files awaiting your decision.

Nothing in your global configuration needs trust: you wrote it.

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

| Mode | Reads | Edits and writes in the workspace | Shell | MCP tools | `fetch` |
| --- | --- | --- | --- | --- | --- |
| `read_only` | run | denied | denied | denied | denied |
| `ask` | run | explicit approval each call | explicit approval each call | explicit approval each call | explicit approval each call |
| `auto` **(default)** | run | run | task-contained, classifier `allow`, or a grant → run; otherwise denied | run | granted hosts run; others denied |
| `full` | run | run | run except `forbidden` | run | run |

Write sub-agents require internal review for each non-read action. `full`
is authority over the workspace, not the machine:
`forbidden` shell shapes are refused under every mode.

`auto` is the default for the TUI and `qq run`. In `ask`, each non-read
action needs explicit human approval even when a stored grant matches; no
reviewer model can approve it. Headless `qq run --approval ask` denies
those calls immediately because no human is present. Approvals in Ask
mode apply once and never create session or workspace grants.

## Who decides a held call

Auto never requests permission from a human. Actions outside its configured
workspace, task, and grants are denied before review. Allowed write-child
actions retain internal supervision through `reviewer_model`.

| Setting | Auto child supervision | Ask |
| --- | --- | --- |
| absent or `approval_delegate: on` | reviewer approves or denies | human approves each call |
| `approval_delegate: off` | supervised actions denied | human approves each call |

A reviewer denial, escalation, timeout, or outage denies the Auto call and
returns a tool error so the agent can pursue an allowed alternative. A
missing reviewer also denies actions requiring supervision. Questions and
policy-forbidden actions never go to the reviewer.

Set it at the top level, in a profile
(`Profile(approval_mode: auto, approval_delegate: on)`), or for one process
with `QQ_APPROVAL_DELEGATE=on|off`. In a project file it needs trust like
any other sensitive key.

### Stop delegating for this session

If a delegate is approving things you would rather see, `/delegate` in the
TUI (or `set_approval_delegate` on the wire) changes who settles the focused
session's held calls for the rest of that session: `off` denies Auto calls
requiring supervision; `on` or `by_mode` restore delegation, and `configured`
clears the override. It takes effect at the session's next held call — a
running session included — and rewrites nothing: your `.qq/config.ron` and
the next session are unchanged. Children the session spawns afterwards start
with the same choice. The mode is still the ceiling: no value here lets the
model do more than the mode allows.

### Seeing who decided

A settled call says who settled it. In the TUI the expanded call detail
reads `approved by reviewer` beside its timing, and a
delegate's denial shows as `denied by reviewer` on the
row; your own decisions read `approved for session`, `denied by you`, or
nothing for a plain once-approval. In `qq run --format text` the log line is
`[tool] NAME approved by reviewer`; in JSONL the `tool_approval_resolved`
event carries `"delegate": "reviewer"`. When an Auto delegate abstains, the
denial includes its reason.

## What the shell classifier decides

Before policy sees a command, QQ parses it with a real bash grammar and
grades it, strictest verdict across every simple command in the pipeline:

| Verdict | Examples | Under `auto` | Under `full` |
| --- | --- | --- | --- |
| `allow` | `cargo test`, `git status`, `ls src`, `rg TODO`, `cat README.md` — literal words, listed read/build programs, operands inside the workspace | runs | runs |
| `prompt` | anything with `$VAR`, globs, `$(…)`, redirects that write, `rm`, `git commit`, `git push`, `sed -i`, `npm install`, `curl`, `docker`, operands outside the workspace, unknown programs | runs inside the native task boundary or with a grant; otherwise denied | runs |
| `forbidden` | `rm -rf /` or `~`, `sudo`, `mkfs`, `dd of=/dev/…`, `git push --force`, `curl … \| sh`, `eval "$x"`, writes to `~/.ssh` or `/etc`, fork bombs, `LD_PRELOAD=` | refused | refused |

Wrappers (`env`, `nice`, `timeout`, `xargs`, `sh -c "…"`) are peeled so the
inner command is graded too. In Ask, the approval prompt shows the verdict
and the rule ids behind it. Auto never turns a classifier verdict into a
human prompt.

## The approval prompt

```
◇ approval needed
$ npm install left-pad  (in ~/repo)
asks because: package_install
y once   a session   w workspace   n deny
```

| Key | Effect | Lifetime |
| --- | --- | --- |
| `y` | run this call | once |
| `a` | run this and every later call of the same shape in this session, when the grant fits | until the session ends |
| `w` | write the grant into `.qq/config.ron` and run, when the grant fits | every session in this workspace |
| `n` | deny; the model receives the denial as a tool error and continues | — |
| `Shift-Y` / `Shift-N` | decide and then steer the run with a note | — |
| `Esc` | deny, like `n`; after `Shift-Y` / `Shift-N` it first cancels the note and leaves the prompt open | — |

"Same shape" means: an exact tool name for edits, writes, MCP tools, and
`fetch` hosts; a **word-boundary prefix** for shell. Approving `cargo test
-p qq-core` for the session records the prefix `cargo test -p qq-core`;
approving `cargo test` covers `cargo test -p anything` but never
`cargo test | sh` — a command containing `|`, `;`, `&`, redirection, or
substitution matches only a grant that quotes it exactly.

A session grant is at most 256 bytes, and a session holds at most 256 of
them. When the command is longer than that, or the session is already at the
cap, `a` and `w` are not offered: the call is approved once, nothing is
recorded, and the status says why. A grant that cannot be stored never fails
the approval.

A `forbidden` verdict is the exception to "same shape". A prefix grant never
lifts it. A grant that quotes the exact command string does, which is why the
byte cap matters: a command longer than 256 bytes cannot be blessed, under
any mode, including `full`.

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
actually asked. Auto never enters this human wait: internal supervision
times out to a denial. Headless `qq run --approval ask` denies immediately
because no human is available.

## Grants in configuration

The `w` key appends to the `policy` section of the project's
`.qq/config.ron`:

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
`auto` reach a host without asking; a managed `deny_hosts` wins.

## Where the rules live

The full model, the effect classes, and every shell rule are in
[`../design/tools.md`](../design/tools.md) § Approval Policy and § Shell
Classification. The decisions behind them: ADR-0007 (effect-classified
approval), ADR-0020 (`forbidden` above every mode), ADR-0021 (`fetch` and
`ask_user` classes).

A write child's reviewer cannot grant an action that its parent policy denies
or holds without a grant. Such a call returns `parent_permission_required`;
the operator must grant the necessary parent scope explicitly. An Ask
parent instead requires human approval for each child action, without a
reviewer or reusable grants. Existing supervised write delegation still
requires its configured reviewer; this change does not enable unsandboxed
children to acquire broader authority.

Pending approvals do not preserve revoked authority. Tightening task or parent
permissions while a reviewer is deciding denies that stale verdict without
recording a grant. Switching a task or its parent to read-only also prevents
a late human approval from executing the held mutation. Request the action
again under the current policy.
