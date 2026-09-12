# QQ Product Design

## Product Intent

QQ is an agent and LLM tool for building software. It should let a developer
talk to an agent, give it controlled access to a codebase, observe its work,
and automate the same workflow without an interactive interface.

QQ is not only a chat client. Its useful output is inspected code, applied
changes, executed commands, test results, and a durable record of how those
results were produced.

## Product Priorities

All design choices are evaluated in this order:

1. **Speed and optimization.** QQ should start quickly, acknowledge input
   immediately, stream output as it arrives, execute tools efficiently, and
   reach a useful result with minimal overhead.
2. **Developer friendliness.** QQ should be exceptionally easy and enjoyable
   to install, understand, operate, recover, and automate.

Features, ecosystem breadth, compatibility layers, and architectural novelty
do not outrank these priorities. Correctness, safe workspace manipulation, and
durable history remain necessary because failures in those areas directly make
the tool slower and less friendly.

## Core Experience

Running QQ in a repository should require no ceremony:

```sh
cd my-project
qq
```

`qq` opens the TUI using the current directory as the workspace and starts a
new session. The developer can have an ongoing conversation, watch model and
tool activity stream in real time, approve sensitive actions, cancel work,
inspect changes, and resume the conversation later.

Every entry point follows the same shape — bare starts a new session,
`--session ID` continues an existing one — and every exit prints the id and
the command that continues it:

| Command | Surface | Session |
| --- | --- | --- |
| `qq` | TUI | New |
| `qq --session ID` | TUI | Existing (root session of this workspace, idle or not) |
| `qq ask "<prompt>"` | One streamed answer | New, not resumable |
| `qq run "<prompt>"` | Non-interactive agent, JSONL or text | New |
| `qq run --session ID "<prompt>"` | Non-interactive agent | Existing (must be idle) |

Earlier sessions stay reachable from the TUI's session list as well as by id.

The first complete vertical slice should allow a developer to:

1. Run `qq` in a codebase.
2. Send a request from the TUI.
3. Receive a streamed model response.
4. Let the agent read and search files.
5. Review and apply a file change.
6. Run a build or test command with visible output.
7. Exit and resume the persisted session.

This slice is more valuable than many disconnected commands or provider
integrations.

## Interaction Modes

### Interactive TUI

The TUI is the initial human interface. It should optimize keyboard-driven
conversation and make agent state obvious without filling the screen with
incidental detail. Streaming must never block input, cancellation, or
navigation.

The TUI is a client of the QQ server even when both run inside one process.
This keeps local and remote behavior aligned and allows an interactive session
to outlive a particular client when using `qq serve`.

### Headless Server

`qq serve [ARGS]` runs persistent sessions for clients on the local machine or
over a private Tailscale network. Several clients may observe a session. Rules
for simultaneous control must be explicit; the initial design may permit one
active controller with additional read-only observers.

The server owns model requests, tools, history, scheduling, and event replay.
Clients render state and submit commands but do not become the source of truth.

### Comprehensive CLI

QQ will grow a comprehensive CLI for direct conversations, one-shot agent
runs, session management, scripting, and machine-readable automation. Command
names beyond `qq` and `qq serve` are intentionally unspecified until their
workflows are designed.

CLI commands must reuse the same server/runtime behavior as the TUI. They must
support predictable exit codes and structured output where automation needs
it, without making the default human experience verbose.

## Design Principles

- **One binary.** Installation and upgrades should place one `qq` executable.
- **Current directory by default.** The common local workflow requires no
  workspace registration or configuration.
- **Local first, remote capable.** Local operation is excellent on its own;
  HTTP/SSE and `qq serve` permit remote clients without a separate product.
- **Durable by default.** Sessions, messages, tool activity, and outcomes are
  stored automatically in SQLite.
- **Detach and resume.** Closing a client must not imply cancelling a run owned
  by a persistent server.
- **Visible work.** Users can tell what the agent is doing, what changed, and
  why it is waiting.
- **Fast cancellation.** A user must be able to stop model and tool work
  promptly.
- **Small interfaces.** Hide orchestration behind a compact command/event
  interface instead of leaking provider, database, or scheduler details into
  clients.
- **Minimal dependencies.** Add libraries when they remove more complexity
  than they introduce. Compile time, binary size, runtime cost, and maintenance
  are all part of the decision.
- **Evidence-led optimization.** Profile realistic workflows before adding
  caches, binary protocols, unsafe code, or specialized data structures.

## Agent Behavior

The agent loop combines model reasoning with controlled tools. It should be
possible to understand every run as an ordered history of user input, model
output, tool requests, tool results, approvals, and final status.

Start with one well-supported model integration and the smallest tool set that
can complete the vertical slice. Provider breadth and complex orchestration
come after the loop is reliable and measured.

Automation should become more autonomous through explicit policy, not hidden
behavior. Interactive and non-interactive modes may choose different approval
defaults, but they use the same tool implementation and record the same events.

## Parallel Agent Direction

QQ is expected to run many agents over time, but raw request concurrency is not
the product. Useful parallelism requires scheduling, budgets, cancellation,
workspace isolation, conflict handling, and result integration.

The progression should be:

1. Concurrent independent sessions.
2. Parallel read-only research within a run.
3. Editing agents in isolated Git worktrees or sandboxes.
4. A coordinator that reviews and integrates returned results.

Agents must not race to edit one checkout. No multi-agent swarm is required for
the first useful release.

## Scope Now

The initial implementation and its supporting specifications should cover:

- Cargo workspace and the `qq` binary.
- `qq` TUI startup from the current directory.
- `qq serve` process lifecycle and configuration.
- Versioned HTTP commands and resumable SSE events.
- SQLite session and event persistence.
- One model integration.
- Minimal file, search, patch, and shell tools.
- Cancellation, approval, and error behavior.
- Performance measurement for the core vertical slice.

## Explicit Non-Goals

Do not create these products or scaffolds during the initial Rust work:

- Web or React frontend.
- Mobile application.
- JavaScript or TypeScript workspace.
- Hosted SaaS or multi-user system.
- Distributed execution workers.
- Plugin ecosystem.
- Broad provider matrix.
- Autonomous multi-agent editing.

Future web and mobile clients are expected, but the HTTP/SSE protocol is the
only preparation they need now.

## Open Decisions

These choices should be resolved by focused designs or small benchmarks rather
than assumptions:

- Authentication for a server exposed on a Tailscale address (loopback
  bearer-token auth is settled; the remote story is not).
- Session control when several clients are attached to one session.
- Concrete startup, latency, memory, and binary-size targets.

Decisions previously listed here — crate choices, server discovery,
provider and credential flow, and default approval policy — are made and
recorded in `architecture.md`, `providers.md`, and `tools.md`.
