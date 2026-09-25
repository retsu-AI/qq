# Ledger — Terminal-Bench readiness

Plan: [`../terminal-bench-readiness.md`](../terminal-bench-readiness.md).
Only the agent working this plan edits this file. Current state on top;
dated entries appended below, newest last.

Phases 1–5 (durable headless run, authoritative context, task-completion
contract, linear/fair streaming R4, resolved model and spend R5) are complete
and qualified; receipts are in Git history (the plan collapsed to its open items in #66). Remaining work is evaluation-gated.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| R6-search | Ignore-aware search candidate | Superseded | tool-layer T2 (#32) | Shipped as `search` v2 / `tree` ahead of the paired evaluation; the evaluation is tool-layer T13 |
| R6-patch | Patch-style edit candidate | Superseded | tool-layer T5 (#37) | Shipped as `edit_file` v2 (batch, cascade, anchors); evaluation is T13 |
| R6-terminal | Persistent terminal/process contract candidate | Planned (gated) | tool-layer T10 | Ships only on T13 / trajectory evidence; gates speed-first H10 |
| R7 | Sub-agent economics and provider-aware scheduling | Planned | | H23/H24 shipped the ownership and budget repairs; remaining items are worker-model defaults and queue-wait recording |
| R8 | Remaining warm-path candidates: credential-lease caching, MCP bounds, prompt-cache determinism | Planned | | Retry (H14), shared messages and encode bench (H18), and cold-path load work (H22.2) shipped in speed-first; what remains here is the prompt-cache determinism measurement |
| TB-pilot | Credentialed Terminal-Bench pilot beyond the first smoke | Planned | | First smoke recorded `9e275be`; Harbor adapter `990df3e`, `af6ed7e`, `9d38c08`, `b80c919` |

## Entries

### 2026-09-08 — ledger opened

No R6–R8 slice in progress. The last landed work on this plan was the
Terminal-Bench pilot tooling (`feat/terminal-bench-pilot`, merged `723b7eb`).

Shipped: none since the pilot. In progress: none. Blocked: R6 paired
evaluations and TB-pilot need paid model runs.

### 2026-09-16 — R6 rows reconciled with the tool layer

R6-search and R6-patch are marked Superseded: their candidates shipped as
tool-layer T2 and T5 in v0.1.0 without the paired evaluation this plan
required first; that evaluation is now tool-layer T13 and still owns the
keep/reject decision for T10 (`terminal`). R8's warm-path items other than
prompt-cache determinism shipped through speed-first H14/H18/H22.2. No R
slice in progress.


### 2026-09-25 — ENG-815 Harbor version identity regression

TB-pilot prerequisite, branch `fix/eng-815-harbor-version-parser`, in progress.
Actual accepted291440a Linux binary in Ubuntu24.04 reports
`qq 0.1.4 (291440a 2026-09-25)`, but the pinned Harbor adapter records
`2026-09-25)` because it selects the final token. Test-first coverage captures
the exact output, legacy plain output, revision/semver suffixes and malformed
responses. No agent/model run, credential or accepted source identity changed.
Runtime evidence is retained in the manager's September25
`qq-jev-evidence/colima-prerequisite-20260925/` directory.

### 2026-09-25 — ENG-815 Harbor version parser locally verified

Test-first commit `2b08bb9` reproduces the actual annotated version failure.
The parser now extracts the package version from recognized QQ output and
returns empty for unexpected output instead of guessing a date or token.
Native Harbor 0.20.0: focused version tests 4/4 and full adapter/ATIF suite
24/24 pass; retained real Linux stdout reads back as `0.1.4`. Python AST and
diff whitespace checks pass. Evidence: manager `qq-jev-evidence/harbor-version-repair-20260925/`.
Rust source is unchanged from accepted `291440a`; its prior workspace gates
are baseline evidence, not rerun checks on this Python-only candidate.
Independent final-head review and publication remain manager-owned. No live
agent/model run, provider call, hosted CI, merge or release is claimed.

### 2026-09-25 — ENG-815 independent-review counterexamples repaired

Review of5eed43f requested changes: numeric prerelease leading zero and
one-field/blank annotation were accepted. Added all three negatives; focused
red run fails exactly those cases, then corrected grammar. Native Harbor0.20.0
focused4/4 and full24/24 pass; AST/diff checks pass. No Rust source changes.
New candidate requires independent review; prior5eed43f is not accepted.
Evidence: manager harbor-version-repair-20260925/followup/.
