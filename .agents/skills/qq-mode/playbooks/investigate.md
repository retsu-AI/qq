# Investigate

You own the answer. This is read-only: no edits, no commits, no PR.

1. Restate the question in one sentence. If it hides a decision ("should
   we..."), name the options.
2. Find the evidence. Read the code paths end to end, the matching
   `docs/design/` page, and `git log -S <symbol>` or `git blame` for why.
   Run read-only commands or existing tests when behavior matters more
   than the source text. Delegate wide searches with a precise question.
3. Check your answer against the code once more: open each `file:line`
   you plan to cite.
4. If the user wants a change after the answer, route again through
   `qq-mode`.

**Reply:** the answer first, then how it works with `file:line`
citations, then gotchas. For a decision, give a short table of options
and your recommendation with its reason. Say so when the premise of the
question is wrong.
