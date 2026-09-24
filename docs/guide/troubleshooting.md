# Troubleshooting

Messages you may see, what they mean, and the fix. Quoted text is what QQ
prints; `…` stands for a path or name specific to your machine.

First stop for anything: `qq doctor`. It runs every local readiness check and
puts the fix next to whatever failed:

```
qq 0.1.3 (ad01547 2026-09-22) · protocol 27 · capabilities 1 · descriptor 9 · store schema 34
ok    configuration    2 sources; qq config sources lists them
ok    project trust    nothing pending
ok    model            anthropic/claude-sonnet-5
fail  credential       anthropic: none found
                       run `qq auth login anthropic` or set ANTHROPIC_API_KEY
ok    credential store 0 stored (keyring)
ok    server           none running; qq starts one on demand
ok    workspace        /home/you/repo (AGENTS.md)
ok    data             /home/you/.local/share/qq (no sessions yet)

1 check failed
```

Exit status 0 means nothing failed. Then `qq config check`, `qq config
sources`, `qq auth list` for the detail behind any one line
([CLI › `qq doctor`](cli.md#qq-doctor---json)).

## Starting

### `no model is configured. Choose one with any of: …`

QQ found no `model:` in any configuration layer and no `--model` /
`QQ_MODEL`. The message lists every way to set one and names your global
config path. `qq init --model PROVIDER/MODEL` writes that file; pick a
route from [Providers](providers.md#built-in-models);
[Quickstart § 2](quickstart.md#2-tell-qq-which-model-to-use) shows the
one-time setup. Only `qq ask` and `qq run` stop here; bare `qq` opens the
TUI and asks with `/models` instead.

### `project configuration needs your trust before it is used: …`

A file under this repository declares something sensitive (a model,
providers, MCP servers, grants). Read the listed file(s), then run
`qq trust` in that directory. You will see this again after any edit to a
trusted file, including one that arrived with `git pull`.
[Permissions › Project trust](permissions.md#project-trust).

### `model route must use provider/model syntax: "…"`

The value must look like `openai/gpt-5.6`: one slash, both halves
non-empty.

### `model route selects an unknown or disabled provider: …`

The half before the slash is not a built-in id and not declared under
`providers`, or `policy.allowed_providers` / `denied_providers` excludes it.
Built-ins: `openai`, `anthropic`, `google`, `xai`, `openai-codex`,
`bedrock`, `bedrock-mantle`.

### `model "…" is not in provider "…"'s authenticated model list; available routes: …`

The provider is fine but the model is not in QQ's catalog for it and not
declared under that provider's `models`. Use one of the listed routes or
declare the model ([Configuration › models](configuration.md#models-entries)).

### `failed to parse configuration source …: …`

RON syntax. Common causes: a missing comma after the last field before `)`,
`"` vs `'`, a key QQ does not know (the message names it; unknown keys are
errors), a bare word where a string was expected (`model: openai/gpt-5.6`
needs quotes). Compare with [Configuration](configuration.md).

### `configuration source … has unsupported version …; expected 1`

Every document needs `version: 1,`.

### `configuration file was discovered more than once: …`

Two layers resolve to the same file, usually a symlink into `config.d/`.
Remove one.

### `symbolic links are not accepted as configuration sources: …`

Project files must be regular files. The *global* `config.ron` may be a
symlink to a regular file (for dotfile managers); nothing under `.qq/` may.

### `configuration containing literal secrets is not private: …`

A file with `Value("…")` must be readable only by you (`chmod 600`). Better:
use `Env(...)` or `Stored(...)` and set `policy.allow_literal_secrets` only
when you must.

### `interactive mode requires a terminal; use `qq ask "<prompt>"` or `qq run "<prompt>"` in a pipe`

Bare `qq` needs a TTY on stdin and stdout. In scripts use
[`qq ask` or `qq run`](headless.md).

### `the platform configuration directories are unavailable`

`HOME` (or `XDG_CONFIG_HOME` / `APPDATA`) is unset or unusable. Set it, or
supply everything through `QQ_CONFIG_CONTENT` and environment credentials.

## Credentials

### `no credential for provider `openai`: run `qq auth login openai` or set the environment variable `OPENAI_API_KEY``

The model's provider has no stored credential and no environment variable.
Do either. Check what is stored with `qq auth list`. The same shape appears
for `anthropic` / `ANTHROPIC_API_KEY` and for `google`, whose message ends
`` `GEMINI_API_KEY` (or `GOOGLE_API_KEY`) ``: either variable works, and
`GEMINI_API_KEY` wins when both are set.

### `environment variable `NAME` is not set`

Configuration references `Env("NAME")` explicitly and the variable is unset
in this shell.

### `provider response failed: no credential for provider `xai`: run `qq auth login xai --oauth` or `qq auth login xai` or set the environment variable `XAI_API_KEY``

Same cause for providers that resolve credentials at request time (`xai`,
`openai-codex`), so it surfaces when the first request is sent rather than at
startup: nothing stored under `PROVIDER/default` and, for xAI, no
`XAI_API_KEY`. Run one of the commands named. `openai-codex` reads no
environment variable, so its message offers only `qq auth login openai-codex`.

With a configured profile the message names it instead: `` credential
`xai/work` is not registered: run `qq auth login xai --oauth --profile work`
or `qq auth login xai --profile work` … ``. If the entry exists but the
keyring lost its secret: `` credential `xai/work` is registered, but its
secret is missing: run `qq auth logout xai/work`, then … ``.

### `credential `…` is not registered`

Configuration references `Stored("name")` and no such entry exists in
*this machine's* credential store. `qq auth set name` (or `qq auth login
PROVIDER` when the name is `PROVIDER/default`). If the reference came from a
repository's committed config, that config should use `Env(...)` or live in a
local, uncommitted fragment ([MCP › Where to declare it](mcp.md#where-to-declare-it)).

For a provider this fails the run. For an MCP server's `bearer` it only
degrades that server: the run proceeds and the catalog reports
`unavailable MCP servers: NAME (credential `…` is not registered; run `qq
auth set …`)` (see below); `qq doctor` warns about it under `mcp`.

### `credential `…` is registered in keyring, but its secret is missing`

The index knows the name but the keyring entry is gone (a keyring reset, a
different login session). `qq auth logout name`, then store it again.

### `credential `…` is bound to a different endpoint`

The stored secret was created with `--endpoint` for one host and the
provider's `base_url` is another. Store a separate credential for the new
endpoint or re-store without the binding.

### `the OS keyring is unavailable while attempting to … credential …`

Linux: no Secret Service on the session bus (a container, SSH without a
desktop session, or the keyring daemon not started). Start one (`gnome-keyring
-d`, KeePassXC with Secret Service enabled), or use `--allow-file` to store a
user-only file, or use environment variables.

### `provider "…" is not authenticated; connect it before spawning on it`

A sub-agent's route (from `delegation.roster` or `/models`) points at a
provider with no credential. Store one or remove the route.

### `"…" is not a built-in provider; `qq auth login` accepts openai, anthropic, google, xai, openai-codex`

`auth login` binds the credential to a built-in endpoint, so it only takes
those ids (check the spelling). For a gateway or MCP bearer use `qq auth set
NAME` and reference `Stored("NAME")`.

### Nothing in `/models`, or every row says `needs credential`

No built-in provider has a resolvable credential. Each `needs credential`
row names the fix: `qq auth login PROVIDER` or the environment variable.
`qq auth list` shows what is stored. Custom providers appear once their
`auth` reference resolves. Credentials are checked when `qq` starts, so
start it again after adding one.

## Running

### `qq run` denied everything

The default `--approval read-only` denies every edit, shell, MCP, and
network call. Use `--approval auto` (workspace edits and safe commands) and
grant specific extras with `--allow-shell` / `--allow-tool` / `--allow-host`.
[Headless › Approval without a human](headless.md#approval-without-a-human).

### The agent says a command was `forbidden`

The shell classifier refuses some shapes under every mode — `rm -rf` outside
the workspace, `sudo`, `git push --force`, `curl … | sh`, writes to `~/.ssh`
or `/etc`. A prefix grant does not lift that. A grant that quotes the exact
command string does, and only when that string fits a session grant (at most
256 bytes); a longer command cannot be blessed this way. Otherwise run it
yourself. Rules:
[Permissions](permissions.md#what-the-shell-classifier-decides).

### `unavailable MCP servers: …`

The named server did not start, connect, or authenticate; the reason follows
its name in parentheses. The run continues without it, and calls to its
tools return an unavailable error to the model. For a stdio server check the
`command` is on `PATH`; for HTTP check the `url`. When the reason names a
credential (`credential `linear/default` is not registered; run `qq auth set
linear/default``, or the environment variable for `Env(...)`), run the
command it names — the next run picks the credential up without a restart.
`qq doctor` reports the same finding under `mcp`; `eager: true` surfaces a
connection failure at startup instead of first use.

### `configuration working directory is invalid: …`

The workspace path does not exist or is not a directory. `qq run --workspace`
needs an existing directory.

### Exit code 2 from `qq run`

Invalid configuration: the message above the exit explains which. Exit code
meanings: [Headless › Exit codes](headless.md#exit-codes).

### Exit code 5 from `qq run`

The agent asked a question and no one was there. The question is in the
JSONL stream (`tool_approval_requested`); answer it interactively with
`qq --session ID`.

## TUI

### `openai needs a credential: run qq auth login openai or set OPENAI_API_KEY`

The configured model's provider has no credential, so QQ did not create a
session (a session needs a usable model). Do what the line says in another
terminal, then start `qq` again; `Alt-N` before that repeats the same line.
The provider and variable name follow your configuration (`anthropic` /
`ANTHROPIC_API_KEY`, `google` / `GEMINI_API_KEY`, `xai` / `XAI_API_KEY`;
`openai-codex` has only `qq auth login openai-codex`).

### Top row says `no model`; the rule says `choose a model with /models`

No `model` is configured anywhere and none was given with `--model` or
`QQ_MODEL`. Open `/models`, pick one, `Enter` creates the session. To make it
permanent, put `model: "PROVIDER/MODEL"` in your global or project
`config.ron` ([Quickstart § 2](quickstart.md#2-tell-qq-which-model-to-use)).

### `choose a model with /models before creating a session`

You pressed `Alt-N` (or `/new`) with no model chosen and no session focused
to inherit one from. `/models`, pick one, `Ctrl-N` to create a session with
it.

### Keys do nothing / wrong characters appear

Your terminal may not send `Alt` or `Ctrl` chords QQ expects. `/help` shows
the bindings; rebind in [`tui.ron`](configuration.md#tuiron). Shift-Enter
needs a terminal with the kitty keyboard protocol; `Alt-Enter` inserts a
newline everywhere.

### Colors look wrong

Set `COLORTERM=truecolor` if your terminal supports it (most do) so the
`ink` theme is chosen; otherwise the ANSI `terminal` theme follows your
palette. `/theme` previews every theme.

### `TUI client stopped: …`

The TUI lost its server and could not reconnect; the reason follows. If a
`qq serve` was running, check it is still up; otherwise rerun `qq`.

## Server and sessions

### `qq server already running at …`

Only one user-scoped server runs per machine; the TUI and `qq run` connect
to it. Stop it to bind another address.

### The sessions database is locked / `StoreBusy`

Another QQ process owns the store (an advisory lock protects it). Find it
with `ps`, or wait for it to exit. Two machines must not share one data
directory.

## Getting more detail

- `qq doctor --json` — the same checks as a JSON object, for scripts and
  bug reports.
- `qq config sources` — every path consulted and whether it applied.
- `qq config explain FIELD` — which layer set a value.
- `qq run --format jsonl` — every event of a run.
- `qq version` — protocol and schema versions, for bug reports.

If none of this helps, open an issue with `qq doctor` and `qq version` output
and the exact message: [bug report](https://github.com/retsu-AI/qq/issues/new?template=bug.yml).
