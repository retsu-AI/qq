# Quickstart

From an installed `qq` to a working agent. Five minutes if you read; one if
you paste.

## 1. Pick a provider and store its credential

Choose one row. The credential goes into your OS keyring; nothing is written
to disk in plain text unless you pass `--allow-file`.

| Provider | Store the credential | Or set an environment variable | Example route |
| --- | --- | --- | --- |
| OpenAI | `qq auth login openai` | `OPENAI_API_KEY` | `openai/gpt-5.6` |
| Anthropic | `qq auth login anthropic` | `ANTHROPIC_API_KEY` | `anthropic/claude-sonnet-5` |
| Google Gemini | `qq auth login google` | `GEMINI_API_KEY` or `GOOGLE_API_KEY` | `google/gemini-2.5-flash` |
| xAI | `qq auth login xai` (API key) or `qq auth login xai --oauth` | `XAI_API_KEY` | `xai/grok-4.6` |
| ChatGPT / Codex subscription | `qq auth login openai-codex` (loopback browser) or `qq auth login openai-codex --device-auth` (code for another device) | — | `openai-codex/gpt-5.6-luna` |
| Amazon Bedrock | AWS credential chain | `AWS_PROFILE` or `AWS_ACCESS_KEY_ID`+`AWS_SECRET_ACCESS_KEY` | see [Providers](providers.md#amazon-bedrock) |

`qq auth login <provider>` prompts for the key without echoing it. In a
script, pipe it: `printenv OPENAI_API_KEY | qq auth login openai`.

The Codex device option changes only the interactive authorization step. QQ
stores and refreshes the resulting credential through the same protected
credential store as the browser flow. A later service or cloud job therefore
needs a durable OS-backed secret store that is available to that service
identity; completing the device prompt does not copy a credential to another
host. `--allow-file` remains an explicit user-only plaintext fallback and is
not a substitute for protected unattended storage.

## 2. Tell QQ which model to use

Once, for every project:

```sh
qq init --model openai/gpt-5.6
# wrote ~/.config/qq/config.ron (model: openai/gpt-5.6)
qq config check     # configuration is valid (model: openai/gpt-5.6)
```

Plain `qq init` lists the built-in providers and asks which one; `--force`
replaces a file that is already there. The file it writes is short and
commented, so editing it later needs no reference.

Or for one project, `qq init --project` writes `<repo>/.qq/config.ron`
(then `qq trust` in that directory, as for any project file that sets
`model`), or for one command with `--model openai/gpt-5.6` or
`QQ_MODEL=openai/gpt-5.6`.

`qq doctor` confirms everything before the first run: configuration, model,
credential, and server, one line each, with the fix next to anything that
fails ([CLI › `qq doctor`](cli.md#qq-doctor---json)).

Do not know which model to pick? `qq ask --model PROVIDER/MODEL "hi"` with
any route from [Providers](providers.md#built-in-models); the TUI's `/models`
lists every model your credentials unlock.

## 3. First answer

```sh
qq ask "Reply with pong"
```

One streamed response, no session, no tools. If this works, everything
below works.

## 4. First agent session

```sh
cd your-project
qq
```

The TUI opens on a new session in this directory. Type a request and press
Enter. The agent can read and search files immediately. When it wants to
edit a file, run a command, or reach the network, it asks:

```
◇ approval needed
$ cargo test  (in ~/your-project)
asks because: command is not on the allow list
y once   a session   w workspace   n deny
```

`y` runs it once, `a` allows that shape for the rest of the session, `w`
writes the grant into the project's `.qq/config.ron` so it never asks
again, `n` denies and tells the model why. See
[Permissions and trust](permissions.md) for the full model.

Skipped step 1 or 2? `qq` still opens. Without a model the top row reads
`no model` and the composer rule says `choose a model with /models`; pick
one there and `Enter` creates the session. With a model whose provider has
no credential, the transcript reads `openai needs a credential: run qq auth
login openai or set OPENAI_API_KEY` (for whichever provider you named); add
the credential and start `qq` again. Only `qq ask` and `qq run` insist on
both before they start.

Useful keys while it works: `Esc Esc` cancels, `Enter` steers the running
agent with a new instruction, `Ctrl-K` opens the command palette, `?` on an
empty prompt or `F1` lists every key. `Ctrl-C` or `/quit` exits and prints
how to resume:

```
To continue this session:
  qq --session 01J…
  qq run --session 01J… "<prompt>"
```

## 5. A project you cloned

If a repository ships its own `.qq/config.ron` (like this one does), the first
`qq` there stops with:

```
error: project configuration needs your trust before it is used:
  /path/to/repo/.qq/config.ron
Review the file, then run `qq trust` in this directory to accept it. Sensitive
sections (providers, MCP servers, grants, model) load only after that.
```

That file may declare providers, MCP servers that run commands, and
approval grants, so QQ never loads it silently. Read it, run `qq trust`, and
QQ prints what it accepted. Edit the file later and QQ asks again.

## 6. Automate it

```sh
qq run --approval auto "Add a unit test for parse_duration and make it pass"
```

`qq run` is the agent without a UI: readable progress on stderr, the final
answer on stdout, exit code `0` on success. `--approval read-only` (the
default) lets it look but not touch; `auto` allows workspace edits and safe
commands; `--format jsonl` gives you every event as a JSON line. Details in
[Headless](headless.md).

## Where next

- [The TUI](tui.md) — sessions, sub-agents, `@file` mentions, themes.
- [Configuration reference](configuration.md) — every key.
- [Troubleshooting](troubleshooting.md) — every error you might see now.
