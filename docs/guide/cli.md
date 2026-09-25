# CLI reference

`qq --help` and `qq <command> --help` are authoritative; this page is the map.

## Global flags

Accepted by every command:

| Flag | Effect |
| --- | --- |
| `--model PROVIDER/MODEL` | override the configured route for this invocation |
| `--max-output-tokens N` | override the generation cap |
| `--organization NAME` | select an enrolled organization manifest |
| `-V`, `--version` | `qq 0.1.2 (abc1234 2026-09-22)` |

## `qq` — interactive

| Invocation | Effect |
| --- | --- |
| `qq` | open the TUI in the current directory; start a new session |
| `qq --session ID` | open the TUI on an existing session of this workspace |
| `qq --tui-qa-root DIR` | isolated, credential-free diagnostic fixture ([`../runbooks/tui-qa.md`](../runbooks/tui-qa.md)) |

Requires a terminal on stdin and stdout; in a pipe use `qq ask` or `qq run`.

## `qq ask PROMPT`

One streamed answer, no tools, no session. [Headless](headless.md#qq-ask--one-answer).

## `qq run [FLAGS] PROMPT`

One unattended agent task. [Headless](headless.md#qq-run--the-agent-unattended).

| Flag | Default |
| --- | --- |
| `--workspace PATH` | current directory |
| `--session ID` | new session |
| `--approval read-only\|auto\|full` | `read-only` |
| `--profile NAME` | `default` |
| `--allow-tool NAME`, `--allow-shell PREFIX`, `--allow-host HOST` | none; repeatable |
| `--steer-stdin` | stdin unread |
| `--timeout-seconds N`, `--max-turns N`, `--max-cost-usd V` | unlimited |
| `--correlation KEY=VALUE` | none; ≤ 8 |
| `--output-schema PATH`, `--output-repair-turns N` | none; `2` |
| `--format text\|jsonl` | `text` |
| `--trace PATH` | none |

Exit codes: 0 completed · 1 task failed · 2 invalid configuration · 3 timeout
or budget · 4 harness failure · 5 needs input · 130 interrupted.

## `qq serve [--bind ADDR] [--allow-origin ORIGIN]…`

The user-scoped server in the foreground. Default bind `127.0.0.1:0`.
[Headless](headless.md#qq-serve--a-persistent-server).

## `qq config …`

| Subcommand | Prints |
| --- | --- |
| `paths` | global config dir, global `config.ron`, global `tui.ron`, data dir, managed dir, organizations file and cache; each path ends in `(exists)` or `(missing)` |
| `sources` | every file consulted in precedence order, and `pending trust:` lines |
| `check` | `configuration is valid (model: …)` or the first error; exit 1 on error |
| `show` | the merged configuration with secrets redacted, then TUI settings |
| `explain FIELD` | which source set `FIELD`: `model`, `organization`, `worker_model`, `delegation`, `audit`, `jev_review`, `jev_routing`, `jev_approval`, `approval_delegate`, `approval_timeout`, `reasoning_effort`, `max_output_tokens`, `provider.NAME`, `profile.NAME`, `pack.ID`, `grant.tool.NAME`, `grant.shell.PREFIX`, `tui.theme`, `tui.bindings.ACTION` |

## `qq auth …`

| Subcommand | Effect |
| --- | --- |
| `login PROVIDER [--profile NAME] [--oauth] [--allow-file]` | store a credential for a built-in provider (`openai`, `anthropic`, `google`, `xai`, `openai-codex`); prompts without echo, or reads stdin when piped; `--oauth` for `xai`; `openai-codex` always opens the browser |
| `set NAME [--kind KIND] [--endpoint URL] [--allow-file]` | store an arbitrary named secret for `Stored("NAME")` |
| `list` | every stored credential: name, backend, kind, endpoint |
| `status NAME` | metadata for one credential |
| `logout NAME` | remove one |

`--allow-file` permits a user-only plaintext file when no OS keyring exists.

## `qq trust`

Accept the sensitive sections of the project configuration found from the
current directory, printing what was accepted. [Permissions](permissions.md#project-trust).

## `qq doctor [--json]`

Is QQ ready to run here? One line per check, each `ok`, `warn`, `fail`, or
`skip`, with the remedy indented under anything that is not `ok`. Exit 0 when
nothing fails, 1 otherwise; warnings do not fail the run. Everything is
local: files, environment, the credential store, and a loopback probe of the
discovery file. No provider is contacted and nothing is written.

| Check | `ok` means | Otherwise |
| --- | --- | --- |
| `configuration` | every layer parses and validates | `fail` with the parse or policy error; `warn` when project files await trust |
| `project trust` | no project file is pending | `fail` listing each file and the sections it declares; `qq trust` |
| `model` | a route is selected (`--model`, `QQ_MODEL`, or a file) | `fail` naming your global `config.ron`; `skip` when configuration did not load |
| `credential` | the model's provider resolves a credential: `stored PROVIDER/default (OS keyring)`, `environment VAR`, an AWS chain input, or `none required` | `fail` with `qq auth login PROVIDER` / the environment variable; `skip` when there is no model |
| `credential store` | the store index reads; `N stored (keyring)` | `warn` when the index cannot be read |
| `mcp` | `none declared`, or every declared HTTP server's `bearer` resolves | `warn` naming the server, the credential problem, and its remedy (`qq auth set NAME`, export the variable); runs proceed without that server; `skip` when configuration did not load |
| `server` | `running at ADDR (pid, version)` or `none running; qq starts one on demand` | `warn` when the discovery state is unreadable |
| `workspace` | the current directory resolves; lists `.qq/config.ron` and `AGENTS.md` when present | `warn` without an `AGENTS.md`; `fail` when the directory does not exist |
| `data` | the data directory is private and writable; shows `sessions.sqlite3` and its size | `fail` when it is not a directory, world-readable, or read-only |

`--json` prints one object:
`{ "version": { "qq", "protocol", "capabilities", "descriptor", "store_schema" },
"checks": [ { "name", "status": "ok|warn|fail|skipped", "summary",
"details": [...], "remedy": null|"..." } ], "failed": N }`
(`details` is omitted when empty).

## `qq init [--project] [--model PROVIDER/MODEL] [--force]`

Write a commented starter `config.ron` with the model sessions start with,
then print the path and the next command:

```
wrote /home/you/.config/qq/config.ron (model: openai/gpt-5.6)

next: qq auth login openai          # or export OPENAI_API_KEY
then: qq
```

| Flag | Effect |
| --- | --- |
| none | write `<global>/config.ron` (the directory `qq config paths` lists as `global`), created user-private |
| `--project` | write `.qq/config.ron` in the current directory instead; the output adds `note: run qq trust so this project's model is loaded` |
| `--model PROVIDER/MODEL` | use this route; without it, and with a terminal on stdin, `qq init` lists the built-in providers and reads a number or a full route. Without a terminal it fails with `pass --model PROVIDER/MODEL` |
| `--force` | replace an existing file; otherwise `… already exists; pass --force to overwrite` and the file is untouched |

The next-step line names `qq auth login PROVIDER` and the API-key variable
for the built-in HTTP providers, the browser sign-in for `openai-codex`, the
AWS credential chain for `bedrock`, and a `providers:` declaration for any
other name. The written file is validated through the same loader as every
other command; a route that names an unknown provider is reported with the
path so you can edit it or rerun with `--force`.

## `qq org …`

Organization manifests: a RON document fetched over HTTPS and layered
between global packs and your global config.

| Subcommand | Effect |
| --- | --- |
| `enroll NAME URL` | fetch and cache |
| `list` | enrolled names without network |
| `use NAME` | make one the default (`--organization` / `QQ_ORGANIZATION` override) |
| `refresh NAME` | refetch, keeping the last good copy on failure |
| `remove NAME` | forget it |

## `qq jev …`

Optional TypeSafe Jev review. `setup [--allow-file]` stores the API key;
`observe` assesses completed runs on the running local server without gating
them. [`../runbooks/jev.md`](../runbooks/jev.md) explains what the observer
reads and how its receipts resume.

| Subcommand | Effect |
| --- | --- |
| `setup [--allow-file]` | prompt for and store the TypeSafe API key; `--allow-file` permits a user-only plaintext file when no OS keyring exists |
| `observe --workspace-id UUID --receipts PATH --max-cost-usd V [--session-id ID] [--max-requests N] [--max-total-tokens N] [--duration-seconds N]` | follow the workspace (or one session) and write one JSONL receipt per assessed run to `--receipts`; stops at the spend cap, `--max-requests` (default and limit 32), `--max-total-tokens` (default 4194304), or `--duration-seconds` (default 300, at most 86400) |

## `qq version`

Version plus the compatibility contracts this build speaks: protocol,
capabilities, descriptor, store schema.

## Environment

See [Configuration › Environment variables](configuration.md#environment-variables).
