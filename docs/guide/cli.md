# CLI reference

`qq --help` and `qq <command> --help` are authoritative; this page is the map.

## Global flags

Accepted by every command:

| Flag | Effect |
| --- | --- |
| `--model PROVIDER/MODEL` | override the configured route for this invocation |
| `--max-output-tokens N` | override the generation cap |
| `--organization NAME` | select an enrolled organization manifest |
| `--approve-self-contained-read-image FAMILY:SHA256` | attest exact self-contained native read behavior for progress/audit accounting; repeat for up to ten families |
| `-V`, `--version` | `qq 0.1.8 (90d0fb8 2026-10-19)` |

Read-image approvals are process-start host decisions for `ask`, `run`, `serve`
and interactive execution. The flag attests read behavior for admitted
argv/stdin in a C/POSIX or unset locale, independent of HOME/TERM and without
executable helpers or configuration-driven code. Approvals remain immutable
through profile, child and cached plans. Existing-server attachment and the
isolated TUI QA fixture reject nonempty approvals explicitly.

Verified reads currently require literal Linux exec, an approved self-contained
static native image, a held workspace cwd and sealed image binding. Supported
families are cat, head, tail, wc, grep, nl, cut, pwd, echo and uniq. Other process
contexts count as Work. Ordinary approval policy still controls execution;
built-in reads keep their existing accounting. See [ADR-0060](../adr/0060-process-context-for-progress.md)
for the host contract and receipt boundary.

## `qq` — interactive

| Invocation | Effect |
| --- | --- |
| `qq` | open the TUI in the current directory; start a new session |
| `qq --session ID` | open the TUI on an existing session of this workspace |
| `qq --tui-qa-root PATH` | isolated, credential-free diagnostic fixture ([`../runbooks/tui-qa.md`](../runbooks/tui-qa.md)) |

Requires a terminal on stdin and stdout; in a pipe use `qq ask` or `qq run`.

## `qq ask PROMPT`

One streamed answer with workspace read tools and configuration-granted external
tools; no approval questions or persisted session. [Headless](headless.md#qq-ask--one-answer).

## `qq run [FLAGS] PROMPT`

One unattended agent task. [Headless](headless.md#qq-run--the-agent-unattended).

| Flag | Default |
| --- | --- |
| `--workspace PATH` | current directory |
| `--session ID` | new session |
| `--approval read-only\|ask\|auto\|full` | `auto` |
| `--profile NAME` | `default` |
| `--allow-tool NAME`, `--allow-shell PREFIX`, `--allow-host HOST` | none; repeatable |
| `--steer-stdin` | stdin unread |
| `--timeout-seconds N`, `--max-turns N`, `--max-cost-usd V` | unlimited |
| `--correlation KEY=VALUE` | none; ≤ 8 |
| `--output-schema PATH`, `--output-repair-turns N` | none; `2` |
| `--format text\|jsonl` | `text` |
| `--trace PATH` | none |
| `--goal [OBJECTIVE]`, `--replace` | ordinary prompt; explicit goal replacement only |
| `--check COMMAND`, `--goal-check-approval once\|for-goal` | no goal check or consent |
| `--goal-deadline DURATION`, `--goal-max-runs N` | workspace goal defaults |

Exit codes: 0 completed · 1 task failed · 2 invalid configuration · 3 timeout
or budget · 4 harness failure · 5 needs input · 130 interrupted.

`--goal OBJECTIVE` runs the goal driver instead of submitting an ordinary
prompt. Bare `--goal --session ID` resumes the stored goal. Goal exits are
0 achieved, 6 paused, 7 blocked, 8 goal budget exhausted, 130 interrupted;
setup/harness errors retain 2/4. Check consent requires both the exact
`--check COMMAND` and `--goal-check-approval` scope. `--approval` and
`--allow-shell` cannot grant check consent. `--replace` requires an objective.
Goal flags conflict with an ordinary prompt; goal mode also rejects
`--steer-stdin` and `--output-schema`. With an objective, timeout/turn flags
set per-run limits, while `--max-cost-usd` caps total goal spending. Budget
changes on bare resume require supplying an objective explicitly.

## `qq serve [--bind ADDR] [--allow-origin ORIGIN]…`

The user-scoped server in the foreground. Default bind `127.0.0.1:0`. The same
server as `qq daemon run`.
[Headless](headless.md#qq-serve--a-persistent-server).

## `qq daemon …`

The per-user daemon: one background `qq` that owns the session store and
serves clients until it is stopped.
[Headless](headless.md#qq-daemon--a-background-server).

| Subcommand | Behavior |
| --- | --- |
| `run [--bind ADDR] [--allow-origin ORIGIN]…` | serve in the foreground until stopped, SIGTERM or `Ctrl-C` |
| `run --detach` | start in the background and return once it serves; report an already running daemon instead |
| `status` | print the running daemon's address, pid, version and log path; exit 1 when none runs |
| `stop` | ask the daemon to stop and wait until it has closed the store |
| `restart [--bind ADDR] [--allow-origin ORIGIN]…` | `stop`, then start this binary with `--detach` |

## `qq goal …`

Manage a session goal through an already running `qq serve`. Every operation
requires `--session ID`; `--workspace PATH` selects its workspace (default:
current directory). Results are JSON, pretty printed by default; `--json`
prints compact JSON. Errors exit 1; invalid arguments exit 2.

| Subcommand | Behavior |
| --- | --- |
| `set OBJECTIVE` | Create or edit a goal; preserve unspecified check and budget fields when editing |
| `show` | Print the current goal snapshot, or `null` when absent |
| `pause`, `resume`, `clear` | Apply the named control to the observed goal revision |
| `approve-check once\|for-goal --command COMMAND` | Authorize the exact reviewed check separately from ordinary tool approvals |

`set` accepts `--replace` for an explicit replacement, `--check COMMAND` or
`--clear-check`, `--goal-deadline DURATION` (positive integer plus `s`, `m`,
`h`, or `d`, at most seven days), `--goal-max-runs N` (1–2000), and
`--max-cost-usd AMOUNT` (positive decimal, up to nine fractional digits).
Workspace defaults initialize new and replacement goals; current managed
limits remain authoritative. An achieved goal requires `--replace`.
Concurrent changes cause a revision conflict; inspect `show` before retrying.
A successful control exits 0 even when the resulting goal is paused.

## `qq config …`

| Subcommand | Prints |
| --- | --- |
| `paths` | global config dir, global `config.ron`, global `tui.ron`, data dir, managed dir, organizations file and cache; each path ends in `(exists)` or `(missing)` |
| `sources` | every file consulted in precedence order, and `pending trust:` lines |
| `check` | `configuration is valid (model: …)` or the first error; exit 1 on error |
| `show` | the merged configuration with secrets redacted, then TUI settings |
| `explain FIELD` | which source set `FIELD`: `model`, `organization`, `worker_model`, `delegation`, `audit`, `approval_delegate`, `approval_timeout`, `reasoning_effort`, `max_output_tokens`, `provider.NAME`, `profile.NAME`, `pack.ID`, `grant.tool.NAME`, `grant.shell.PREFIX`, `tui.theme`, `tui.bindings.ACTION` |

## `qq auth …`

| Subcommand | Effect |
| --- | --- |
| `login PROVIDER [--profile NAME] [--oauth] [--device-auth] [--allow-file]` | store a credential for a built-in provider (`openai`, `anthropic`, `google`, `xai`, `openai-codex`); prompts without echo, or reads stdin when piped; `--oauth` is for `xai`; `openai-codex` uses the loopback browser flow by default and `--device-auth` prints a code for a browser on any device |
| `set NAME [--kind KIND] [--endpoint URL] [--allow-file]` | store an arbitrary named secret for `Stored("NAME")` |
| `list` | every stored credential: name, backend, kind, endpoint |
| `status NAME` | metadata for one credential |
| `logout NAME` | remove one |

`--allow-file` permits a user-only plaintext file when no OS keyring exists.

## `qq mcp inspect NAME`

Connect to the one declared MCP server `NAME` (starting its process or
contacting its endpoint, with only that server's credential resolved), list
its tools, and print one JSON object: `server`, `digest`, `configured_pin`,
`matches_pin` (`null` without a pin), a `warning` that the descriptors are
untrusted, and `tools` with each `name`, `description`, `input_schema`, and
`hints`. No tool is called and nothing is written; the digest is what `pin`
takes ([MCP servers](mcp.md#pinning-a-servers-tools)). Exit 1 when the
server is not configured, cannot be reached, or does not answer within 45 s.

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

## `qq version`

Version plus the compatibility contracts this build speaks: protocol,
capabilities, descriptor, store schema.

## Environment

See [Configuration › Environment variables](configuration.md#environment-variables).
