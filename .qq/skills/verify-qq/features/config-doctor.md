# init, auth, config, doctor

## Sub-features

- `qq init [--project] [--model R] [--force]` writes
  `$XDG_CONFIG_HOME/qq/config.ron`.
- `qq auth login|set|list|status|logout`.
- `qq config paths|sources|check|show|explain FIELD`.
- `qq doctor [--json]`. It exits 1 when any check fails.

## How to get to it (user POV)

Run `qq init --model …`, then `qq auth login <provider>`, then `qq doctor`.

## Driving it headlessly

```sh
H=.qq/skills/verify-qq/qq-home.sh
$H qq <name> doctor --json | jq '{failed, checks: [.checks[]? | {name, status}]}'
$H qq <name> config sources        # Applied	QQ_CONFIG_CONTENT
$H qq <name> config explain model  # model: QQ_CONFIG_CONTENT
$H qq <name> config check
```

The smoke scenario `doctor-and-config` covers `config check` and
`doctor --json`.

## Gotchas

- `doctor --json` prints indented JSON over many lines. Parse the whole of
  stdout, not one line at a time.
- A `workspace` warning about a missing `AGENTS.md` is expected in a scratch
  workspace.
- `qq auth` can touch the real OS keyring. Do not drive it outside an
  isolated home, and pass `--allow-file` to keep secrets in the home.
- `qq init` without a TTY needs `--model`.
