# compaction

## Sub-features

- Manual: `/compact` and `/rollback` in the TUI, or the server routes
  `POST /v1/sessions/compact` and `/v1/sessions/compact/rollback`.
- Automatic before a prompt: runs when the estimated context (4 bytes per
  token) exceeds the model's `context_window` minus 10 %
  (`crates/qq-core/src/sessions/context.rs`).
- In-run, at a tool boundary: `runtime/compaction.rs` (ADR-0039).

## How to get to it (user POV)

Long sessions compact automatically before prompts and during runs. `/compact`
is available on an idle session when the user requests it.

## Driving it headlessly

1. Start the server and create a session with one run (see
   [serve.md](serve.md)).
2. Script the summarizer reply. It must contain all five headings, or
   validation rejects it:

   ```json
   {"text": "## Intent\nx\n## Decisions and constraints\nx\n## Work state\nx\n## Open problems\nx\n## Next step\nx"}
   ```
3. `POST /v1/sessions/compact` with
   `{"command_id":"…","command":{"type":"compact_session","session_id":"…"}}`.
   The response outcome is `compaction_queued`. SSE then shows `run_started`,
   `run_finished`, and `session_compacted`, which carries `before_bytes` and
   `after_bytes`.
4. For automatic compaction, script enough tool turns to fill a declared window:
   `QQ_CONTEXT_WINDOW=16384 .qq/skills/verify-qq/qq-home.sh qq <name> --max-output-tokens 1024 run …`
   Return the structured summary when the
   request ends with the summarizer instruction. The original task continues
   after `session_compacted`, without a manual compaction command.
5. To exercise reactive recovery, return a provider context-length error before
   work, then answer the automatic summarizer. An oversized summarizer request
   can be rejected too; QQ folds smaller portions and commits only the final
   summary. Verify the task artifact and persisted tool-call count after restart.

## Gotchas

- `qq run` has no `/compact`. Use the server route.
- The headless golden `completed_after_in_run_compaction.jsonl` shows the
  in-run event sequence.
