# qq

AI coding agents in one binary: a terminal UI, a headless runner for scripts
and CI, and a local server that several clients can share. Agents read,
search, and edit your files, run commands, call MCP tools, and spawn
sub-agents. Every mutating action passes an explicit approval policy — a
real bash parser grades each command — and every event is stored in SQLite,
so sessions survive restarts and many agents can run at once.

Documentation: <https://retsu-ai.github.io/qq/> (the same pages as
[`docs/guide/`](docs/guide/README.md)).

## Install

Linux and macOS:

```sh
curl -fsSL https://retsu-ai.github.io/qq/install.sh | sh
```

The script picks the archive for your machine, verifies it against the
release's `SHA256SUMS`, and installs to `~/.local/bin`. Other routes:

| | |
| --- | --- |
| Homebrew | `brew install retsu-ai/qq/qq` (once the tap is published) |
| Nix | `nix run github:retsu-AI/qq` or `nix profile install github:retsu-AI/qq` |
| cargo-binstall | `cargo binstall --git https://github.com/retsu-AI/qq qq` |
| From source | `cargo install --git https://github.com/retsu-AI/qq --locked qq` |
| Windows | the `x86_64-pc-windows-msvc.zip` from the [latest release](https://github.com/retsu-AI/qq/releases/latest) |

Pinning, checksums, and upgrades: [Install](docs/guide/install.md).

## First run

```sh
qq auth login anthropic                 # or openai, google, xai, openai-codex; or export ANTHROPIC_API_KEY
QQ_MODEL=anthropic/claude-sonnet-5 qq ask "Reply with pong"
```

Make the model permanent (`~/.config/qq/config.ron` on Linux; `qq config
paths` shows the path on your OS):

```sh
qq init --model anthropic/claude-sonnet-5   # or plain `qq init` to pick from a list
```

Then, in any repository:

```sh
qq                                       # interactive
qq run --approval auto "Add a --dry-run flag and a test for it"   # unattended
```

The [Quickstart](docs/guide/quickstart.md) walks through approvals,
resuming sessions, and cloned projects that ship their own `.qq/config.ron`.

## Documentation

**Using QQ** — [`docs/guide/`](docs/guide/README.md)

| | |
| --- | --- |
| [Install](docs/guide/install.md) · [Quickstart](docs/guide/quickstart.md) | get running |
| [Providers and credentials](docs/guide/providers.md) | OpenAI, Anthropic, Google, xAI, Codex, Bedrock, gateways, local models |
| [Permissions and trust](docs/guide/permissions.md) | approval modes, the shell classifier, grants, project trust |
| [The TUI](docs/guide/tui.md) · [Headless](docs/guide/headless.md) · [MCP servers](docs/guide/mcp.md) | the three surfaces and extra tools |
| [Configuration reference](docs/guide/configuration.md) · [CLI reference](docs/guide/cli.md) | every key, flag, and environment variable |
| [Troubleshooting](docs/guide/troubleshooting.md) · [FAQ](docs/guide/faq.md) | every message and its fix |

**Building QQ** — [`docs/README.md`](docs/README.md): design of the system as
built, architecture decisions, plans, runbooks. [`AGENTS.md`](AGENTS.md) is
the contributor contract; [`CONTRIBUTING.md`](CONTRIBUTING.md) the short
version.

## What it does

- **Three surfaces, one runtime.** `qq` (TUI), `qq run` (JSONL or text,
  exit codes for CI), `qq serve` (HTTP/SSE with resumable cursors). Same
  tools, policy, and store everywhere.
- **Approval you can read.** `read_only`, `ask`, `auto`, `full`; a bash
  parser grades every command `allow` / `prompt` / `forbidden`; `forbidden`
  never runs, under any mode. Approve once, for the session, or write the
  grant into `.qq/config.ron` from the prompt.
- **Durable sessions.** Everything is in SQLite before it is shown. Quit,
  reboot, `qq --session ID`. Long sessions compact themselves.
- **Many agents.** Sub-agents with a delegation roster and depth bound; a
  sidebar grouped by what needs you; `Alt-A`/`Alt-D` to answer another
  session's approval without leaving yours.
- **Layered configuration.** Global, per-project (trusted before it is
  loaded), fragments, environment, managed and MDM layers; `qq config
  explain FIELD` tells you which one won.
- **Providers.** OpenAI, Anthropic, Google, xAI, ChatGPT Codex, Amazon
  Bedrock and Bedrock Mantle built in; any OpenAI-, Anthropic-, or
  Google-compatible endpoint by declaration. Credentials live in the OS
  keyring.
- **MCP.** Stdio and streamable-HTTP servers join the same approval flow as
  built-ins.

## Contributing and support

Issues and PRs are welcome — see [`CONTRIBUTING.md`](CONTRIBUTING.md).
Questions go to [Discussions](https://github.com/retsu-AI/qq/discussions);
security reports follow [`SECURITY.md`](SECURITY.md). Development is tracked
in [`docs/plans/`](docs/plans/README.md).

## License

[MIT](LICENSE).
