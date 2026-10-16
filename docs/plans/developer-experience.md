# Developer experience

## Goal and scope

Make the shortest path from install or edit to verified useful work obvious.
This plan owns the remaining guide and contributor-feedback work, formerly
`guide-expansion` and `agent-feedback`. Existing GE/AF IDs and Linear issues
remain valid; consolidation does not close them.

Shipped: GE0 corrections and GE10 glossary (#222–#224, #230); affected/full
checks, lint, nextest, smoke and skills (#282); cancellation audit (#311);
gate-integrity fixes (#317). Their contracts belong to
[testing](../runbooks/testing.md), [website](../runbooks/website.md), the
[user guide](../guide/README.md), and `AGENTS.md`, not another implementation
checklist. Evidence: [ledger](progress/developer-experience.md).

## Remaining work

| ID / tracker | Outcome | Acceptance |
| --- | --- | --- |
| GE5 / ENG-933 | Keybindings reference | Context-specific compose/run/approval/question/trust/picker/transcript keys; Esc precedence, rebinding and terminal caveats. Reuse command registry checks; cover binding names and hard-coded keys in docs-truth |
| GE4 / ENG-932 | Environment reference | Config, credential, terminal, installer and path variables; child environment versus explicit forwarding; confirm proxy behavior before documenting it. Assert exported variable/base-env sets; configuration page links rather than duplicates |
| GE2 / ENG-930 | Sessions guide | Persistence, owner-process lifecycle, serve/attach versus direct run/StoreBusy, find/resume/delete/prune, retention and spill limits, paths and consistent backup. Verify actual behavior; proposed archive is not shipped |
| GE3 / ENG-931 | Skills, commands and mentions | Discovery roots and compatibility behavior, names/bounds/ambiguity, mention forms/caps, root versus nested instructions, ignore rules. Assert mention kinds and load-skill surface |
| GE1 / ENG-929 | Agents, profiles and packs | Model/effort selection, roster, depth/count/budget limits, watching children, serialized supervised write authority, pack authoring and audit. Link config shapes; add field-name and spawn-argument truth checks |
| GE9 / ENG-981 | Common workflows | Tested recipes for fixing a test, reviewing, refactoring with approvals, delegation, resuming, CI and switching models. Link references instead of restating them |
| GE6 / ENG-934 | Server reference | Ownership, binding/exposure, origins/auth, routes, SSE replay/resnapshot, limits and compatibility. Assert route inventory; describe enrollment/TLS only as implemented |
| GE7 / ENG-935 | Managed configuration | Precedence, ownership/security, MDM/GPO, organization enrollment/cache, forbidden manifest authority and debugging. Assert managed identifiers and filenames |
| GE8 / ENG-936 | Site changelog | Generate from existing root `CHANGELOG.md`, add sidebar/edit URL, handle absent file explicitly and update website runbook. The old first-release dependency is satisfied |
| AF5 / ENG-1028 | Opt-in structured `QQ_LOG` tracing | Run/turn/provider/tool/store spans; JSON state-dir log, off by default, redaction test and unchanged perf budget. Reserve an ADR before design |
| AF6 / ENG-1028 | Remaining module test audits | One module per PR: stress flakes, replace sleeps without weakening assertions, lower sleep ratchet, mutation catch counts, merge redundant fixtures. Cancellation is already done; prioritize shell-heavy compaction/delegation and known child-summary timeouts |
| AF9 / ENG-1028 | Encode repeated review lessons | Mine actual repeated corrections; enforce with types/tests/lints rather than more prose |
| AF10 / ENG-1028 | Evaluate agent guidance | Replay 3–5 past tasks with fresh agents, current qq-mode versus old instructions; report time to green, gate iterations, review findings and produced evidence |
| AF7 operator follow-up | Activate merge queue | Verify branch protection and record observed settings. CI implementation alone is not activation; keep full pre-merge checks until confirmed |

Guide parent: ENG-928. Work on GE5/GE4 first, then GE2/GE3/GE1,
then GE9; GE6/GE7 and contributor slices are independent. There is no
mandatory cross-page PR stack: separate pages may proceed in parallel.
AF10 can follow the shipped AF6.1 audit.

## Shared acceptance

- Document shipped behavior, checked on a real build; label proposed behavior
  in the sentence naming it. Re-read current source instead of copying stale
  numeric bounds from the old plan.
- Answer a user task, define terms once in `concepts.md`, and keep command/key
  and configuration tables in one authoritative place.
- Each new guide page includes sidebar placement, docs-truth coverage for its
  new reference surface, and a passing site build/link check per the website
  runbook. Edit the guide, not generated site pages.
- Run `cargo xtask check`; guide work also runs docs-truth. Behavior/tooling
  changes need real-binary smoke or verify-qq evidence. Record limitations,
  not inferred performance wins.
