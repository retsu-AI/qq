# FAQ

### What is QQ, in one paragraph?

A single Rust binary that runs AI coding agents: interactively in a terminal
UI, unattended from scripts (`qq run`), and as a local HTTP/SSE server that
several clients can share. Agents read, search, and edit your files, run
commands, call MCP tools, and spawn sub-agents, with every action gated by
an approval policy you control, shell commands confined by the operating
system's sandbox, and every event stored in SQLite so sessions survive
restarts.

### How is it different from Codex, OpenCode, Pi, or Claude Code?

Mostly in emphasis. QQ is built for running *many* agents at once with
bounded resources and a durable record — sessions are first-class, the
default mode works unattended in a sandboxed private copy of the workspace
and publishes checked edits, the server is part of the design rather than an
add-on, and it is
one static binary with no runtime. It is younger and has fewer integrations.
A detailed comparison lives in
[the archived harness comparison](https://github.com/retsu-AI/qq/blob/2cafa6537e61495f0a7ef022eef9b985ade9dc22/docs/research/harness-comparison.md) (2026-09-16 snapshot).

### Which models work?

Any model from OpenAI, Anthropic, Google, xAI, Amazon Bedrock, a ChatGPT
Codex subscription, or any endpoint speaking the OpenAI Responses, OpenAI
Chat Completions, Anthropic Messages, or Google GenerateContent protocol
(gateways, LiteLLM, local servers). [Providers](providers.md).

### Is there a default model?

No. QQ asks you to choose one once because the choice decides cost and
capability, and the error that asks lists every way to set it. A repository
can commit its team default in `.qq/config.ron`.

### Where are my credentials?

In your OS keyring (Secret Service, Keychain, Credential Manager). Never in
configuration files, never in the session database. `qq auth list` shows
names and backends only. In environments without a keyring, `--allow-file`
opts into a user-only file, or use environment variables.

### Does QQ send telemetry?

No. QQ makes network requests only to the model providers and MCP servers you
configure, and to an organization manifest URL if you enroll one.

### What does the agent see of my project?

Files it reads with tools, output of commands it runs, and at the start of a
run the root `AGENTS.md` (or `CLAUDE.md` when `AGENTS.md` is absent), capped at
64 KiB. It is told to look for nested `AGENTS.md` files before changing files
under them. `@path` in a prompt attaches a file explicitly.

<a id="agents-md"></a>
### How do I give the agent project instructions?

Write `AGENTS.md` at the repository root: conventions, how to build and
test, what not to touch. QQ reads it every run. Nested `AGENTS.md` files
scope instructions to a subtree.

### Why does it ask me before running `cargo test`?

Under the default `auto` mode it should not: Auto runs commands inside its
task sandbox without asking. A prompt means the session is in `ask` mode,
which asks before every non-read call. The top row shows the mode; switch
with `/approval`. If commands fail instead, check the
[sandbox requirements](install.md#system-requirements).
[Permissions](permissions.md).

### Can it run unattended in CI?

Yes: `qq run --approval auto --format jsonl …` with credentials from the
environment, on a runner that meets the
[sandbox requirements](install.md#system-requirements).
[Headless › In CI](headless.md#in-ci).

### Can I use it on a remote machine?

Run `qq serve` there and attach clients over a private network; the wire
protocol is documented. Authentication for non-loopback clients is being
designed, so today keep it on loopback or a Tailscale-style private network.

### How do I resume a conversation?

Every exit prints the session id and the commands to continue it:
`qq --session ID` for the TUI, `qq run --session ID "…"` unattended. Inside
the TUI, `/sessions` lists them by name.

### Where is the data, and how do I back it up or delete it?

`qq config paths` prints the data directory; `sessions.sqlite3` there holds
everything. Copy it to back up; delete it to start over. Trust decisions live
alongside it.

### How do I uninstall?

Delete the binary, then optionally the directories `qq config paths` lists
and each credential via `qq auth logout`. [Install › Uninstall](install.md#uninstall).

### How do I report a bug or ask for a feature?

[Issues](https://github.com/retsu-AI/qq/issues) with the templates there;
include `qq version` output. Security problems: see
[`../../SECURITY.md`](../../SECURITY.md).

### Where is the developer documentation?

[`../README.md`](../README.md): design (the system as built), ADRs (why),
plans (what is next), runbooks (how to). `AGENTS.md` at the repository root
is the contributor contract, and yes, agents read it too.
