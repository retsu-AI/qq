---
name: qq-mode
description: Complete a QQ task with a narrow reproduction, the smallest fix, relevant verification, and the review the user requested.
---

# QQ work

The user's request sets the scope and stopping point. `AGENTS.md` governs
repository work; this skill adds no approval gates, ledgers, or mandatory
documents. Planned work starts at `docs/plans/README.md`.

Work within the authorized task, workspace, and tool permissions. Ask only
when the next action crosses that boundary; do not request approval for each
shell command or repeat permission already granted. Enforce the boundary
through the available tools and policy, without bypassing a denial.

## Do the work

1. Read `git status` and the relevant code. Use your own worktree when other
   agents are active; give delegated work explicit owned paths. Preserve
   other people's changes.
2. Identify the requested observable result. For a runtime defect, reproduce
   it with the narrowest behavioral test and show that test failing before
   fixing the cause. For a refactor, preserve the observable contract.
   Read-only and documentation tasks do not need invented runtime tests.
3. Make the smallest justified change. Delete obsolete code and conflicting
   instructions instead of adding layers. Update guide or design docs only
   when the change makes them wrong.
4. Iterate with the relevant narrow test:
   `cargo nextest run -p <crate> -E 'test(/<pattern>/)'`.
   Exercise the changed CLI, TUI, or server behavior directly or through an
   existing smoke scenario or `verify-qq` drive. A simulated provider
   proves the exercised runtime behavior, not the model's ability to work.
5. Review the final diff against the branch base, including delegated edits.
   Check for unrelated reversions, missing requested behavior, and weakened
   assertions. For a PR, commit before the final gate so its evidence
   identifies the commit. If the pre-push hook is enabled, let it run that
   gate during push; otherwise run `cargo xtask check` once before pushing.
   On failure, read `target/qq-check/last.json` and use the named rerun
   command while fixing the cause. Do not loop the full gate.

Do not run perf, release builds, mutation testing, or soak tests unless the
task calls for them. Report observed limitations and budget overruns; do not
invent new blockers. Encode a recurring defect in a focused behavioral test,
type, or lint when it prevents the actual mistake.

## Deliver the requested result

Push, open a PR, or merge only when the user has authorized it. Existing
authorization persists; do not ask again. A requested final PR is non-draft
unless the user says otherwise. Keep its description to the user-visible
change, relevant verification, and known impact; link an existing issue.

When review is requested, check findings against the code and fix valid ones
in new commits. Explain incorrect findings with evidence. Do not game the
reviewer, dismiss findings to obtain approval, or substitute a clean receipt
for a required Codex 👍. When the user requires that reaction, obtain it for
the final non-draft PR head after the last change.

Keep working while an actionable step remains. When review is requested, wait
only after the concrete changes are pushed and actionable work is complete.
Stop at the user's requested boundary. Finish with the result, what was
verified, and any remaining blocker.
