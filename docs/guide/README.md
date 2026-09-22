# QQ user guide

QQ is one binary that runs AI coding agents in your terminal, from scripts,
and as a local server. This guide is for people using QQ. If you are changing
QQ, start at [`../README.md`](../README.md) instead.

## Start here

| I want to… | Read |
| --- | --- |
| install QQ | [Install](install.md) |
| go from nothing to a first answer in a minute | [Quickstart](quickstart.md) |
| connect a model provider (OpenAI, Anthropic, Google, xAI, Codex, Bedrock, a gateway) | [Providers and credentials](providers.md) |
| understand what the agent may do without asking | [Permissions and trust](permissions.md) |
| learn the terminal UI | [The TUI](tui.md) |
| run agents from scripts and CI | [Headless: `qq ask`, `qq run`, `qq serve`](headless.md) |
| give the agent more tools | [MCP servers](mcp.md) |
| look up every setting | [Configuration reference](configuration.md) |
| look up every command and flag | [CLI reference](cli.md) |
| fix an error message | [Troubleshooting](troubleshooting.md) |
| something else | [FAQ](faq.md) |

## Two things QQ needs

1. **A model route** such as `openai/gpt-5.6` or `anthropic/claude-sonnet-5`,
   set once in a config file or per run with `--model` / `QQ_MODEL`.
2. **A credential for that model's provider**, stored once with
   `qq auth login <provider>` or supplied through the provider's environment
   variable.

Everything else has a working default.

## Where things live

| What | Linux | macOS | Windows |
| --- | --- | --- | --- |
| Your config | `~/.config/qq/config.ron` | `~/Library/Application Support/dev.qq.qq/config.ron` | `%APPDATA%\qq\qq\config\config.ron` |
| Sessions database | `~/.local/share/qq/sessions.sqlite3` | `~/Library/Application Support/dev.qq.qq/sessions.sqlite3` | `%APPDATA%\qq\qq\data\sessions.sqlite3` |
| Project config | `<repo>/.qq/config.ron` | same | same |
| Credentials | OS keyring (Secret Service) | Keychain | Credential Manager, DPAPI file for large entries |

`qq config paths` prints the exact paths for your machine.

## Conventions in this guide

- `qq` alone means the interactive TUI in the current directory.
- `PROVIDER/MODEL` is a model route; the provider half is one of the built-in
  ids (`openai`, `anthropic`, `google`, `xai`, `openai-codex`, `bedrock`,
  `bedrock-mantle`) or a name you declared under `providers`.
- Config examples are [RON](https://github.com/ron-rs/ron); a file starts
  with `(` and `version: 1,`.
