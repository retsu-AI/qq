# Permissions and trust

QQ separates two questions:

- **Trust** — may this project's configuration influence QQ at all?
  Answered once per file content with `qq trust`.
- **Approval** — may the agent take this action right now? Answered by the
  session's approval mode, the grants in effect, and, when neither decides,
  you.

## Project trust

A repository can ship `.qq/config.ron`, `qq.ron`, `.qq/config.d/*.ron`, and
`.qq/packs/`. Those files can declare providers, MCP servers that run
commands, sub-agent rosters, and grants that let tools run without asking.
QQ therefore refuses to load a project file that declares anything sensitive
until you have accepted that exact content:

```
error: project configuration needs your trust before it is used:
  /home/you/repo/.qq/config.ron
Review the file, then run `qq trust` in this directory to accept it. Sensitive
sections (providers, MCP servers, grants, model) load only after that.
```

Sensitive means any of: `model`, `worker_model`, `reviewer_model`,
`organization`, `providers`, `mcp`, `packs`, `profiles`, `delegation`,
`audit`, `jev_review`, `jev_routing`, `jev_approval`, `approval_delegate`,
`reasoning_effort`, or a `policy` grant (`allow_tools`,
`allow_shell_prefixes`, `allow_hosts`, `shell_env`).
A project file that only sets `policy.exposed_tools` or `max_output_tokens`
loads without trust.

```sh
qq trust
trusted /home/you/repo/.qq/config.ron
  declares: model, policy.allow_tools, policy.allow_shell_prefixes
```

Trust is recorded as a digest of the file in your data directory, not in
the repository. Any edit to a trusted file — yours or from `git pull` —
makes QQ ask again. `qq config sources` shows `pending trust:` lines for
files awaiting your decision.

Nothing in your global configuration needs trust: you wrote it.

## Approval modes

Every session runs under one mode. Set the default in a profile
(`approval_mode`), change it in the TUI with `/approval`, or pass
`qq run --approval`.

| Mode | Reads | Edits and writes in the workspace | Shell | MCP tools | `fetch` |
| --- | --- | --- | --- | --- | --- |
| `read_only` | run | denied | denied | read-only allowlisted only | denied |
| `ask` | run | ask | ask | ask | ask |
| `auto` **(TUI default)** | run | run | classifier `allow` or a grant → run; `prompt` → ask; `forbidden` → refused | run | granted hosts run; others ask |
| `full` | run | run | run except `forbidden` | run | run |
| `supervised` | run | reviewer model decides | reviewer decides | reviewer decides | reviewer decides |

`supervised` is used for sub-agents that may write; you cannot pick it for
your own session. `full` is authority over the workspace, not the machine:
`forbidden` shell shapes are refused under every mode.

`qq run` defaults to `read_only` because nobody is there to answer. Pass
`--approval auto` for a run that may edit.

## Who decides a held call

The mode says what is held. `approval_delegate` says who settles it when a
delegate is configured; without one, every held call is yours. The delegate
is Jev when `jev_approval: true` and a TypeSafe key is stored, otherwise
`reviewer_model`; when Jev abstains or is unavailable the reviewer model is
asked next, then you.

| Profile | Set | What happens |
| --- | --- | --- |
| default | nothing | `auto` and `supervised` holds go to the reviewer first; `ask` holds come to you |
| hands-off | `approval_delegate: on` | `ask` holds go to the reviewer too. Its approve runs the call; its deny still comes to you, with the reason, and your wait starts then |
| strict | `approval_delegate: off` | every held call comes to you, even under `auto`, even with a reviewer configured |

Under `auto` and `supervised` a reviewer deny is final: the model gets the
reason as a tool error and you are not asked. Under `ask` you asked to decide
everything, so a reviewer deny is advice and the prompt still appears. A
reviewer `escalate`, timeout, or outage always comes to you. `forbidden`
shell shapes, private or denied hosts, and `ask_user` questions never go to
the reviewer.

Set it at the top level, in a profile
(`Profile(approval_mode: ask, approval_delegate: on)`), or for one process
with `QQ_APPROVAL_DELEGATE=on|off`. In a project file it needs trust like
any other sensitive key. `jev_approval` works the same way
(`QQ_JEV_APPROVAL=on|off`); a stored key with it off is never read.

## What the shell classifier decides

Before policy sees a command, QQ parses it with a real bash grammar and
grades it, strictest verdict across every simple command in the pipeline:

| Verdict | Examples | Under `auto` | Under `full` |
| --- | --- | --- | --- |
| `allow` | `cargo test`, `git status`, `ls src`, `rg TODO`, `cat README.md` — literal words, listed read/build programs, operands inside the workspace | runs | runs |
| `prompt` | anything with `$VAR`, globs, `$(…)`, redirects that write, `rm`, `git commit`, `git push`, `sed -i`, `npm install`, `curl`, `docker`, operands outside the workspace, unknown programs | asks | runs |
| `forbidden` | `rm -rf /` or `~`, `sudo`, `mkfs`, `dd of=/dev/…`, `git push --force`, `curl … \| sh`, `eval "$x"`, writes to `~/.ssh` or `/etc`, fork bombs, `LD_PRELOAD=` | refused | refused |

Wrappers (`env`, `nice`, `timeout`, `xargs`, `sh -c "…"`) are peeled so the
inner command is graded too. The approval prompt shows the verdict and the
rule ids behind it.

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
| `Esc` | leave the prompt open; `Ctrl-G` jumps back to it | — |

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
actually asked (after the delegate answered or was cut off, not from when
the delegate was consulted). `qq run` has nobody to ask and never waits on
you: an `auto` hold is denied immediately, or after the delegate has had its
20 s when one is configured.

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
