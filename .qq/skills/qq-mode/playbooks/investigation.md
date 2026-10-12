# Playbook: investigation

Read-only. The deliverable is a cited answer, not a change.

1. **Restate the question** as one sentence that has a checkable answer.
2. **Find the code path** from the user surface inward: CLI (`src/cli.rs`,
   `src/main.rs`), server routes (`crates/qq-server/src/lib.rs`), runtime
   (`crates/qq-core/src/sessions/`), provider (`crates/qq-provider/`).
   Delegate wide searches to a subagent; keep only conclusions here.
3. **Check history and intent**: `docs/adr/`, `docs/design/`, and
   `git log -S '<symbol>' --oneline` for when and why it changed.
4. **Settle facts by running, not reading,** when a run is cheap: a smoke
   scenario, `qq run --format jsonl` against the fake, or a focused test.
5. **Answer** with `file:line` citations. Label every claim *verified*
   (you ran it), *read* (you read the code), or *inferred*.
