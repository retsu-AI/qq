---
name: verify-qq
description: Drive the real qq binary (ask, run, serve, TUI, sessions, tools, delegation, compaction, MCP, config) in an isolated home against a fake provider, and capture evidence that a feature works.
---

# verify-qq

Prove a user-facing behavior on the real binary, the way a user reaches it.
`cargo xtask smoke` covers the core paths automatically; use this skill for
anything smoke does not cover, and add a smoke scenario when the drive is
worth repeating on every PR.

## 1. Launch

```sh
cargo build --bin qq
eval "$(.qq/skills/verify-qq/qq-home.sh new my-check)"   # sets QQ_HOME, QQ_ENV, QQ_FAKE_PORT
```

`qq-home.sh new <name>` creates `target/qq-verify/<name>/` with `HOME`,
`XDG_{CONFIG,DATA,STATE,RUNTIME}_HOME`, a git-initialized `workspace/`, and a
fake OpenAI Responses endpoint on a free loopback port that serves the
replies in `$QQ_HOME/script.jsonl` (one JSON reply per line; see *Scripting
the fake*). It prints shell assignments; nothing outside the directory is
read or written. Ready when `curl -s http://127.0.0.1:$QQ_FAKE_PORT/health`
prints `ok`.

Run qq inside the home:

```sh
.qq/skills/verify-qq/qq-home.sh qq my-check run --format jsonl "where is Widget?"
```

## 2. Doctor

Run first, and whenever anything looks off:

```sh
.qq/skills/verify-qq/qq-home.sh qq my-check doctor --json | jq '{failed, version}'
```

`failed` must be `0` (a `workspace` warning about a missing `AGENTS.md` is
expected). `version.qq` must match `target/debug/qq --version`.

## 3. Drive

Find the feature in `features/README.md`, open its file, and follow
*Driving it headlessly*. Exercise the path a user takes: the CLI command,
the server route, or TUI keys. Never call internal APIs or test-only hooks
to make a proof pass.

### Scripting the fake

`$QQ_HOME/script.jsonl`, one reply per provider request, in order; the last
reply repeats when the script runs out:

```json
{"text": "streamed answer"}
{"tool": "read_file", "args": {"path": "inventory.rs"}}
{"status": 401, "body": "{\"error\":{\"message\":\"bad key\"}}"}
```

Every request is appended to `$QQ_HOME/provider-requests.txt`, so you can
prove what qq sent (tool results, prior turns, tool schemas).

The reply index counts requests for the life of the fake, across runs. Start
a fresh home (`qq-home.sh new <name>`) for each scripted scenario, or write
the script to cover every request the sequence of runs will make.

## 4. Evidence

A proof has three parts; capture all of them under `$QQ_HOME/evidence/`:

1. **The action**: the exact command or keys.
2. **The observed state**: stdout/JSONL, exit code, SSE events, TUI capture.
3. **The side effect**: files in `workspace/`, rows in
   `$QQ_HOME/data/qq/sessions.sqlite3` (queries:
   `docs/runbooks/run-output-queries.md`), requests in
   `provider-requests.txt`.

Paste the relevant lines (not whole logs) into your reply or PR with the
evidence path.

## 5. Cleanup

```sh
.qq/skills/verify-qq/qq-home.sh stop my-check
```

Stops only the fake and any `qq serve` this home started (tracked PIDs in
`$QQ_HOME/pids`). It keeps `evidence/`; `qq-home.sh rm my-check` deletes the
whole home. Never `pkill qq`: other agents run qq on this machine.

## Isolation and concurrency

Each home has its own data directory, so homes never share a session store
or server. Two agents must use different names. `qq run` exits `4` if a
`qq serve` from the same home is running; stop it first.

## Keeping the map honest

When a feature's commands, keys, or events change, update its file in
`features/` in the same PR. A feature with no file gets one (copy the four
headings from any existing file). Run one drive end to end after editing a
file.
