---
name: qq-mode
description: Start here for any non-trivial QQ task (bug, feature, refactor, investigation, PR follow-up). Routes to a playbook and the skills its steps need.
---

# QQ mode

The user's request sets the scope and the stopping point. `AGENTS.md`
governs repository work. This skill picks the procedure; it adds no
approval gates, ledgers, or mandatory documents. Planned work starts at
`docs/plans/README.md`.

## 1. Pick one route

| The request is... | Route |
| --- | --- |
| A question: how does X work, why is Y, should we do A or B | playbook `investigate` |
| A defect: wrong output, crash, hang, flaky test | playbook `bug-fix` |
| New or changed behavior, or a docs change | playbook `feature` |
| Same behavior, different structure (rename, move, collapse) | playbook `refactor` |
| Continue work another agent or session started | playbook `resume` |
| Open a PR for finished work | skill `open-pr` |
| Get a PR green: CI, review threads, Codex findings | skill `babysit-pr` |
| Merge a PR | skill `land-pr` |
| Check a diff before it ships | skill `review-diff` |

Read a playbook with `read_file .qq/skills/qq-mode/playbooks/<name>.md`.
Load a skill with `load_skill <name>`. If two routes fit, take the one
whose deliverable the user asked for. Bug-fix beats feature when the user
calls existing behavior wrong.

Follow the playbook's steps in order. A step you skip gets a one-line
reason in the final reply. When a step names a skill, load it then, not
up front. If a skill or playbook is wrong for the task, finish the task
and tell the user what to fix in the skill.

## 2. Authority

What the user said decides how far you go. Authority given once lasts for
the session; do not ask again.

| The user said | You may |
| --- | --- |
| Asked a question | read and run read-only commands |
| Asked for a change | edit, test, and commit in your own worktree |
| "open a PR", "push it" | also push the branch and open a ready PR |
| "merge", "land", "ship", "take it all the way" | also merge once `land-pr` says ready |

Keep working while an actionable step remains, then stop at the
boundary the user set. Always stop and ask before you force-push, rewrite a pushed branch,
delete a branch you did not create, change branch protection or CI
settings, or publish a release. Never bypass a tool denial or an approval
prompt; report it as the blocker.

## 3. Delegate with a full brief

A spawned sub-agent sees no skills catalog and cannot call `load_skill`.
It has only your brief. Every brief says:

1. Read `AGENTS.md`, `.qq/skills/qq-mode/SKILL.md`, and
   `.qq/skills/principles/SKILL.md` with `read_file` before any work.
2. The question or change, the files it owns, and the files it must not
   touch.
3. What done looks like: the test or command that must pass.
4. The answer shape: findings with `file:line`, or the diff and the
   command output that proves it.

Use `authority: read` unless the child must edit. Only one write child
runs at a time, so give write children disjoint paths and run them in
sequence. You own the result: read the child's diff yourself before you
build on it or report it.

## 4. Reply

Lead with the result for the person who asked: what changed for users,
or the answer. Then how you verified it, quoting the command and the
observed result. Label anything you did not observe as inferred. Then any
blocker in one sentence with what you need. Give a PR as its full URL.
