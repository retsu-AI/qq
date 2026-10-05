# Concepts and glossary

The words the rest of this guide uses, one short definition each, with a
link to the page that goes deeper. Read [How they fit together](#how-they-fit-together)
first; look up the rest when a page uses a word you are unsure of.

## How they fit together

```text
workspace                    the directory QQ works in
└── session                  one conversation with an agent
    ├── run                  one prompt, carried to a final answer
    │   └── turn             one model reply
    │       └── tool call    one action the model asked for
    └── child session        a sub-agent, with runs of its own
```

A session runs one run at a time; the agent loops through turns, executing
the tool calls each turn asks for, until the model answers without asking
for more. Its [approval mode](#approval-mode) and [grants](#grant) decide
which tool calls run straight away and which are [held](#held-call) for a
[delegate](#delegate) or for you.

## What is saved, and where

| What | Kept in | Survives quitting QQ |
| --- | --- | --- |
| workspaces, sessions, runs, turns, messages, tool calls and their output, session grants, compaction summaries | the sessions database, `sessions.sqlite3` in your data directory | yes |
| profiles, packs, the roster, `policy` grants, `w` approvals | your config files and the project's `.qq/config.ron` | yes |
| which project files you trusted | `trust.ron` in your data directory | yes |
| credentials | the OS keyring (details in [Providers](providers.md#where-credentials-go)) | yes |
| drafts queued with `Ctrl-Enter` | the TUI's memory | no |
| a run in progress, and any tool call it is waiting on | the sessions database, but the work itself is live | no: when the `qq` process running it exits, the run and its held calls end `interrupted`, and nothing held runs |

`qq config paths` prints the directories for your machine; the defaults are
in [Where things live](README.md#where-things-live).

## Glossary

### Workspace

The directory QQ works in: the current directory for `qq`, or
`--workspace PATH` for `qq run`. Every session belongs to one workspace, and
the file tools refuse paths outside it. See [`qq run`](headless.md#qq-run--the-agent-unattended).

### Session

One conversation with an agent: its history, [approval mode](#approval-mode),
[profile](#profile), model, and grants. Sessions are saved, so you can quit
and resume one with `qq --session ID` or `/sessions`. Its id is 32
hexadecimal characters. See [Sessions](tui.md#sessions).

### Child session

A session started under another one: a sub-agent the model spawns with the
`spawn_agent` tool, or one you open with `Alt-C`. It has its own runs and
history and stays in its parent's workspace. The [roster](#roster) bounds
how many and how deep. See [`delegation`](configuration.md#delegation).

### Run

One prompt carried to a final answer, a failure, or a cancellation. Each
prompt you submit starts a run (a [steer](#steer) joins the current one);
a session runs one at a time. `qq run` is one run, unattended. See
[While a run is executing](tui.md#while-a-run-is-executing).

### Turn

One model reply inside a run. When the reply asks for tools, QQ runs them
and the next turn sees the results; the run ends on a turn that asks for
none. `--max-turns` caps them. See [Limits](headless.md#limits).

### Tool call

One action the model asked for in a turn: read a file, edit, run a shell
command, fetch a URL, call an MCP tool. The session's
[approval mode](#approval-mode) and [grants](#grant) decide whether it runs,
is held for an answer, or is refused. See [Permissions and trust](permissions.md#approval-modes).

### Steer

Send more input to a run while it is working. `Enter` during a run steers:
the text reaches the model at the next boundary between model and tool,
without starting a new run. `Alt-S` interrupts the current turn or tool
first. Steering the run has not used when it ends is dropped. See
[While a run is executing](tui.md#while-a-run-is-executing).

### Queue

Hold a draft until the run ends instead of steering it in. `Ctrl-Enter`
queues; the draft becomes the next run when the session goes idle, and
`Alt-Up` pulls it back. Queued drafts live in the TUI, up to 8 per session,
and are lost if you quit. See [While a run is executing](tui.md#while-a-run-is-executing).

### Compaction

Replacing the older part of a session's model context with a summary, so a
long conversation keeps fitting the model's context window. QQ compacts on
its own near the end of the window; `/compact` does it on demand and
`/rollback` undoes the newest one. The transcript you see is unchanged. See
[Prompts](tui.md#prompts).

### Profile

A named set of session defaults (model, [approval mode](#approval-mode),
reasoning effort, and similar), chosen with `/profile` or
`qq run --profile NAME`. Anything a profile leaves out comes from the
top-level configuration. Not to be confused with a
[credential profile](providers.md#profiles). See [`profiles`](configuration.md#profiles).

### Pack

A directory with a `pack.ron` that bundles profiles, a persona prompt,
skills, commands, and MCP servers, so a setup can be shared. A project's
packs load only once you [trust](#trust) it. See [`packs`](configuration.md#packs).

### Roster

The models an agent may spawn [child sessions](#child-session) on:
`delegation.roster`, up to 8 routes, each tagged with a [role](#role). The
same section bounds how deep children may nest (default 1, at most 3) and
whether they may edit files. See [`delegation`](configuration.md#delegation).

### Role

The label on a [roster](#roster) entry: `fast`, `balanced`, or `strong`. The
model picks a sub-agent's model by role; the child keeps its parent's
[profile](#profile). See [`delegation`](configuration.md#delegation).

### Grant

A standing approval: a tool name, a shell command prefix (`cargo test`), or
a host that may run without asking. `a` at an approval prompt grants for
the session and `w` for the workspace; `policy.allow_*` in configuration
grants for every new session. See
[Grants in configuration](permissions.md#grants-in-configuration).

### Approval mode

How much a session may do without asking: `read_only`, `supervised`, `ask`,
`auto` (the TUI default), or `full`. `qq run` defaults to `read_only`
(`--approval read-only`); `supervised` is what sub-agents that may edit run
under. Change it with `/approval` or `qq run --approval`. See [Approval modes](permissions.md#approval-modes).

### Held call

A [tool call](#tool-call) waiting for an answer because its session's mode
does not let it run on its own. A [delegate](#delegate) or you settle it; it
waits until then, until the run's deadline or `approval_timeout_seconds`,
or until you cancel. See
[Who decides a held call](permissions.md#who-decides-a-held-call).

### Delegate

Who answers a [held call](#held-call) before you are asked: [Jev](#jev) when
`jev_approval` is on and its key is stored, then the [reviewer](#reviewer);
if neither decides, you. `approval_delegate`, `QQ_APPROVAL_DELEGATE`, and
`/delegate` choose whether a delegate is asked. See [Who decides a held call](permissions.md#who-decides-a-held-call).

### Reviewer

The model named by `reviewer_model`. It answers [held calls](#held-call) as a
[delegate](#delegate): approve, deny, or send to you. Sub-agents that edit
files need one. See
[Who decides a held call](permissions.md#who-decides-a-held-call).

### Trust

Your permission for a project's configuration to take effect. A project
file that declares anything sensitive (such as the model, providers, MCP
servers, packs, or grants) does not load until you accept its exact content:
the TUI asks, and `qq ask`, `qq run`, and `qq serve` exit until you run
`qq trust`. Editing a sensitive part asks again. See
[Project trust](permissions.md#project-trust).

### Jev

TypeSafe's optional assessment model. QQ can ask it to review final answers
(`jev_review`), pick the model and effort for a prompt (`jev_routing`), and
answer held calls as a [delegate](#delegate) (`jev_approval`). All are off
by default; `qq jev setup` stores the key and turns nothing on. See
[`qq jev`](cli.md#qq-jev) and the [Jev runbook](../runbooks/jev.md).
