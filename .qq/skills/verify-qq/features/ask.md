# ask

## Sub-features

- `qq ask PROMPT`, with the global `--model`, `--max-output-tokens`, and
  `--organization` (`src/cli.rs`). It runs in-process and streams the answer
  to stdout (`src/main.rs` `fn ask`).
- The read-only tools (`read_file`, `tree`, `search`) run. Mutating tools are
  refused, not held: "Tool approval is unavailable for this run".

## How to get to it (user POV)

`qq ask "Reply with pong"`. It exits 0 on success. Any error prints
`error: …` and exits 1.

## Driving it headlessly

```sh
echo '{"text": "pong"}' > $QQ_HOME/script.jsonl
.qq/skills/verify-qq/qq-home.sh qq <name> ask "ping"   # stdout: pong
```

The proof is stdout containing the text, exit 0, and one request in
`provider-requests.txt` that contains the prompt. The smoke scenario
`ask-streams-text` checks the same things.

## Gotchas

- It never uses a server and never writes the session store: the data
  directory holds only `auth.lock`.
- `docs/guide/headless.md` says "No tools". The request does still declare
  tools.
