# Core stabilization

Freeze new product features until QQ can complete and verify ordinary editing
work. Permissions, shell execution and tool policy are core repairs. This is
the only active plan; it replaces the overlapping plans, progress ledgers,
reservations and calendar gates. Existing issues and PRs remain open until
someone resolves them. Historical evidence remains in Git and the linked
research/ADRs; deleting a ledger does not turn a failed result into a pass.

## What failed

The 2026-10-19 read-only session-store investigation found interrupted goals
losing continuity, commands timing out during builds, advertised delegation
choices being rejected, stale edits and agents stopping with work remaining.
In the inspected v22 cohort: 211 root runs, 7,626 tool calls, 25 shell/exec timeouts
(21 involving Cargo), 13 advertised-model rejections, and 84 failed edits out
of 806 (46 stale). Four inspected approval-timeout goals stayed paused; 22
subsequent prompts had no goal association. These are diagnostic counts from
mixed builds, not a task-success rate or proof of every proposed cause.

The previous approach accumulated instructions and completion counters without
proving the resulting task artifact. More prompting cannot repair lost run
ownership, unavailable tools or an unenforced authorization boundary. Recent
merges already removed Jev (#374), repaired empty-session reuse (#370), and
simplified provider/auth plumbing (#373/#376); do not rebuild those changes.

## Small fixes, in dependency order

Each row can take several small PRs, each fixing one observable failure. Start
independent rows in parallel with disjoint owned paths. Recheck current source
and open PRs before editing; retain compatible work already underway.

| Outcome | Implementation boundary | Proof before calling it fixed |
| --- | --- | --- |
| One enforceable permission contract | `qq-config`, core tool admission/delegation and runtime composition. Derive task authority once; replace duplicated approval decisions and reviewer-dependent write delegation with child authority bounded by its parent. | Allowed work proceeds without repeated prompts; out-of-scope work is denied; a child cannot widen its parent's grants. Read-only, Ask, Auto and Full have one tested dispatch contract. |
| Shell and built-ins enforce that contract | Core tool dispatch/process ownership and native execution adapters. Deliver an enforced Linux path first, then macOS/Windows against the same contract below. Delete obsolete paths as each replacement is proven. | Real native tests exercise allowed work, denial and escape attempts; unavailable enforcement is explicit and fails closed. Managed polling lets commands outlive the current 600-second tool ceiling within the task deadline, retaining output, exit/cancel state and ownership across recovery. Termination leaves no descendants; test the deadline with a shortened limit or controlled clock. |
| The task survives interruption | `sessions/goals`, approvals, scheduler and client continuation. Explicit resume/continue reconnects the same unfinished task; distinguish waiting, stopping a run and cancelling its goal. | Approval timeout → resume, steer, disconnect/reopen and deadline expiry preserve task identity with one successor. Denied effects never replay automatically; expired budgets never silently reset; ordinary unrelated prompts do not revive goals. |
| Tools tell the truth and edits preserve work | `src/runtime.rs` model discovery/admission; core edit tools. Advertise and validate against the same discovery snapshot; fresh availability failures remain recoverable. Preserve atomic edit semantics. | Every advertised model admits under the same snapshot. Unique matches after unrelated edits succeed; ambiguous/stale matches and partially invalid batches change nothing. Final diff retains unrelated upstream changes. |
| Compaction preserves obligations | Finish/reconcile [#371](https://github.com/retsu-AI/qq/pull/371) before overlapping its runtime work; then test retained constraints and recovery. | Early user constraints beyond the record cap, later corrections, repeated compaction and a failed summary retain the effective task; retries do not duplicate tool effects. |
| Less code and prompt machinery | After the affected behavior is covered, remove redundant dispatch wrappers, policy branches, obsolete adapters and duplicated instructions. Reuse existing tests, smoke providers and benchmark adapters. | The affected task still passes its independent verifier; no new supervisor, model judge, mandatory planning layer or replacement evaluation framework. |

### Permission and execution contract

[#375](https://github.com/retsu-AI/qq/pull/375), reviewed at
[`e74209e`](https://github.com/retsu-AI/qq/commit/e74209e75b2fd7f46a7b131720e347369643090b),
is input to these repairs, not an approved implementation specification. Carry
its valid review findings into the implementing PRs; reconcile its proposed
plan with this one instead of maintaining a second work queue.

- Authorization covers the task's workspace, readable/writable paths, network
  operations, credentials, expiry/revocation and resource limits. Preserve it
  through resume/restart; protect `.qq` policy and `.git` trust files. Enforce
  it at every built-in
  and shell boundary. Children receive equal or narrower authority. Command
  classification and prefix rules assist policy; neither provides containment
  or grants blanket authority to scripts, hooks or descendants.
- An unattended run never waits indefinitely for approval: allowed actions
  execute; requests outside its grants return a typed denial and the missing
  scope. An attended user can explicitly extend scope. Model/tool output is
  never authorization. Expose Read-only, Ask, Auto and Full without compatibility aliases. Ask
  requires fresh human approval; reusable grants authorize Auto work. Write
  supervision remains internal to delegation, bounded by parent authority.
- Shell egress has its own explicit grant; existing HTTP-fetch allowlists do
  not authorize arbitrary shell traffic. Isolate each execution's proxy and
  credential authority. Git/hosting operations must not expose reusable
  credentials to repository hooks; use narrowly authorized operations with
  credentials outside the untrusted process. Scope commit/push/PR operations
  to the authorized repository and branch so the full task can finish.
  Protect Git trust configuration
  and hooks even when repository mutations are authorized.
- Enforce file and network boundaries against symlinks, existing hardlinks,
  inherited descriptors, host Unix sockets and cross-session access. A Linux
  PID namespace needs its own procfs. Do not label a fallback sandboxed when
  it lacks any required boundary. Control memory/process/CPU limits and reap
  descendants even after daemonization or owner failure. Prove these properties
  on each supported native backend; a watchdog or process group alone is not
  proof. An unavailable backend must not silently run an unattended sandbox
  request on the host.
- Keep external MCP servers explicitly trusted and outside any shell-isolation
  claim; their advertised tools still pass QQ's grant checks. Preserve event
  persistence before publication and bounded output/queues. Wire/result changes
  require protocol bumps and previous/current fixture coverage. Keep safe-Rust
  rules; choose an existing safe OS interface before introducing dependencies
  or a new crate, and ship a real caller with any abstraction.

Implement policy through real dispatch first, then an end-to-end native slice;
do not merge placeholder types claiming enforcement. Preserve existing
approval behavior until the selected backend qualifies: fewer prompts and
new child write authority must never ship ahead of their containment.
Each PR's acceptance
covers only capabilities it actually delivers. Backend enablement requires
its native tests for permitted operations, denied paths/egress, credential and
cross-session isolation, aliases/links/sockets, resource exhaustion and complete
teardown. Mocks test admission; they do not qualify OS containment. Use finite
fixtures and event-driven deadlines, not 24-hour or multi-day soaks. Other
platforms can follow without blocking unrelated deterministic core fixes.

## A fast environment agents can trust

Use isolated worktrees with a named owner for shared files. The issue or PR
states the reproduction, owned paths and independent acceptance command; no
additional task ledger or ADR reservation. Delegate disjoint work, review the
combined diff against its actual base, and preserve unrelated upstream edits.
Keep the existing `qq_provider::test_support`, session integration tests and
`xtask` smoke path as the routine feedback loop.

Create six small fixtures from the observed failures: bug fix, refactor with
an unrelated upstream edit, build/debug with a long command, approval
interruption/resume, long-context correction, and a review/fix cycle. Each
starts from a clean repository and isolated QQ home, has fixed permissions and
budgets, and a verifier outside the agent that checks files/tests, persisted
state and surviving processes. Exercise review states with a local fake;
actual PR delivery still requires the review the user requested. No live
credentials or network are needed for deterministic regression tests.

For the finite model check, pin baseline/candidate commits, tool/config versions,
model/effort, initial files, grants and budgets. Run each fixture once on the
baseline and once on the candidate (12 initial runs). Use only an authorized account within an explicit spend cap;
otherwise mark this check unrun. Retain failures and transcripts; do not rerun
until green. Use affected-case reruns while fixing a failure and keep every result.
Qualify a changed candidate with all six cases on that same final build; never
combine passes across revisions. All six tasks must satisfy their verifier
without hidden human corrections; report baseline/candidate outcomes, tool failures and
interventions. These fixtures establish a bounded readiness claim, not general
correctness. Deterministic fixes can merge individually; the feature freeze
ends only after the integrated candidate passes this finite check and its
enabled native backends pass containment tests. No elapsed-day requirement.

For each implementation PR: demonstrate the narrow regression failing before
the fix, exercise the affected real CLI/TUI/server path, inspect the final
diff, commit, then run `cargo xtask check` once (through the pre-push hook
when enabled, otherwise manually). Read `target/qq-check/last.json`
and use targeted reruns on failure. Do not weaken tests, approvals or sandbox
guarantees to get green. Report budget overruns rather than inventing new perf
gates. Resolve valid review findings; a completed run, green unit tests or a
review reaction alone is not evidence that the user's task succeeded.

This keeps the useful parts of Poteto's pstack: [small verified fixes](https://github.com/cursor/plugins/blob/main/pstack/skills/poteto-mode/playbooks/bug-fix.md),
[real application checks](https://github.com/cursor/plugins/blob/main/pstack/skills/create-verification-skill/SKILL.md)
and [artifact-based evaluation](https://github.com/cursor/plugins/blob/main/pstack/skills/poteto-mode/playbooks/eval.md).
Cursor's [harness findings](https://cursor.com/blog/continually-improving-agent-harness)
also favor task outcomes, reliable tools and model-specific experiments over
activity counts. The local `.source/` Codex, OpenCode, Pi and fx comparisons
informed process ownership and context recovery; they do not prove QQ quality.
Import neither harness wholesale. Change the model prompt or tool surface only
when an affected fixture shows improvement; retain retrievable prior context
and bounded outputs rather than forcing extra planning.
