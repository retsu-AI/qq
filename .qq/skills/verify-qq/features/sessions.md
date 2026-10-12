# sessions and resume

## Sub-features

- The store is `$XDG_DATA_HOME/qq/sessions.sqlite3`; `qq config paths`
  prints its location.
- `qq --session ID` opens that session in the TUI.
- `qq run --session ID` submits a prompt to an idle root session.
- Exit hints: `To continue this session:` followed by
  `qq --session <id>` and `qq run --session <id> "<prompt>"`.

## How to get to it (user POV)

Copy the hint qq prints on exit, or use `/sessions` in the TUI.

## Driving it headlessly

```sh
printf '%s\n' '{"text":"first answer"}' '{"text":"second answer"}' > $QQ_HOME/script.jsonl
H=.qq/skills/verify-qq/qq-home.sh
S=$($H qq <name> run --format jsonl "remember: blue" | jq -r 'select(.type=="trial").session_id')
$H qq <name> run --format jsonl --session "$S" "what colour?" > $QQ_HOME/evidence/resume.jsonl
sqlite3 "file:$QQ_HOME/data/qq/sessions.sqlite3?mode=ro" 'select id, parent_id from sessions'
```

Proof:

- The second `trial.session_id` equals `$S`.
- The last request in `provider-requests.txt` contains both
  `remember: blue` and `first answer`.

The smoke scenario `run-resume-session` checks both. More store queries are
in `docs/runbooks/run-output-queries.md`; open the store read-only.

## Gotchas

- `run --session` rejects sessions that are unknown, busy, children, or
  from another workspace.
- The model, profile, and approval mode come from the new invocation.
