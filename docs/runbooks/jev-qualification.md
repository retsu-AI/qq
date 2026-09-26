# Qualifying optional Jev decisions

This is an evaluation procedure for the [proposed usefulness plan](../plans/jev-usefulness.md),
not an assertion that its new receipts or controls have shipped. Steps that depend
on JU1–JU7 wait for their implementation. The [current operator runbook](jev.md)
describes commands available today. No command here enables a capability silently.

## 1. Establish scope and consent

Record in the plan ledger: exact QQ commit/build/platform, task fixtures and
revisions, approval mode/grants, provider/model/effort and Jev model/policy IDs,
configuration provenance, evaluator version, seeds, initial workspace hashes and
limits. Check proposed PRs at their actual merged heads, not their former titles.
Keep TypeSafe setup separate from capability activation. Disclose task/evidence
sent to TypeSafe; masking is defense in depth, not proof that secrets cannot leave.

ENG-809 owns paid-run approval, ENG-811 the Jev mode/quiet-host work, ENG-815
routing. Obtain task subset, authorized credentials, numeric currency ceiling,
per-run duration/token/tool limits, whole-experiment limit and a named stop owner
**before** any live inference. An issue or plan is not a spend authorization.
Do not copy credentials, private task payloads, session databases or raw receipts
into Git. Raw logs live under `target/qq-perf/jev-usefulness-<date>/` with access
appropriate to the task data; the ledger carries only bounded redacted results.

## 2. Run credential-free contract checks first

Use memory credentials and loopback fake endpoints. Subprocess environments must
exclude real TypeSafe/provider credentials, global config and live server discovery.
Prove zero remote connections when off, malformed, untrusted, cancelled before
admission or over budget. A fixture that accidentally sends a real request is an
incident/unknown-spend receipt, not a harmless successful test.

Execute C1–C5 cases: config/profile/reload/off, authoritative task/steering,
Jev-only headless waiting, human-required phase, repeated/late resolutions,
precision validation, per-attempt spending, restart and missing-evidence behavior.
Then exercise C6 negatives and one changed-evidence recovery limit. Keep one
red/green regression per reported bug. Check old wire/store fixtures with new code;
never run an older binary against a forward-migrated session store.

For soak tests, drive at least 1,000 synthetic decisions with deterministic
cancellation, steering, two clients and restarts. Assert bounded queues/caches,
no repeated billed dispatch, exactly-once action/settlement and stable replay.
Test a remote reviewer outage separately from semantic rejection.

## 3. Define disjoint arms before sampling

Use unique Jev-specific arm names, not the A0–A3 names used by other evaluations.
Keep unrelated settings equal; intentional model routing differences must be
recorded as the experimental variable, not passed off as a fixed-model comparison.
Reuse existing evaluation tooling and accounting coverage rather than a new suite.

| Arm | Purpose |
| --- | --- |
| `JU-off` | No Jev capabilities; ordinary policy plus the same configured optional LLM fallback |
| `JU-current-approval` | Optional pinned pre-repair approval baseline, sandboxed to fixtures; no known unsafe production exposure |
| `JU-repaired-approval` | JU1–JU5 repairs, same question/threshold policy unless a documented parser identity changed |
| `JU-pilot-shadow` | New C6 questions scored without settling approvals; separately opted in and billed |
| `JU-pilot-approval` | Accepted/qualified C6 decision policy; no other Jev capabilities |
| `JU-routing` | JU7 only, versus a fixed authorized fallback with matching other limits |
| `JU-advisory` | Existing explicit observer or accepted selective final checks; separate from approval |
| `JU-strict` | Only if #187/Strict lands and is explicitly selected; separate finite-budget product-value experiment |

A comparison across different QQ revisions also includes a matched off control on
both revisions so general harness improvements are not credited to Jev. For a
full combined mode, qualify components first and then the combination. Do not
infer a combined benefit by adding isolated percentages.

Stratify coding/research, root/child, local/public-network/external-write held
calls, long/short histories and evidence completeness. Preserve all failures,
ineligible calls and timeouts. Static refusals are not model abstentions; reads
that bypass approval are not missing approvals. A human-created/adjudicated
label set with exact task/effect context is required; another model's agreement
alone is not a safety label.

## 4. Collect the right outcomes

Per decision: eligibility, consent/provenance, action and task revision hashes,
evidence completeness, attempt identity, raw label/distribution/confidence,
parser result, QQ policy identity/result, per-stage timestamps, settled/unknown
spend, fallback and actual human outcome. Store only permitted bounded content.
Existing events without these fields are marked missing; do not invent receipts
from free-text logs or treat missing cost as zero.

Classify each hold into a documented path: static policy; no delegate/opt-in;
Jev approval/denial/abstention; low confidence; missing evidence; precision/schema
rejection; unavailable key/transport/timeout; LLM fallback; human-required;
explicit human override before delegate completion. Use the same classifier for
all arms and retain both remote and local results.

Report at least:

- Human-required transitions and **actual human answers**, separately. Count
  per unique hold; `ToolApprovalRequested` is not an escalation metric.
- Interrupted work per task and per active wall-clock agent-hour, plus verified
  task completions and unsuccessful/censored runs. Do not improve the denominator
  by dropping failed tasks or running extra idle hours.
- Unsafe approval by severity, unnecessary denial, coverage/risk curves and
  calibration by action class; sample sizes and confidence intervals.
- p50/p95 end-to-end completion, approval wait, time spent on the critical path,
  fallback rate and remote latency distinct from harness overhead.
- Total cost/tokens per externally verified success including children, routing,
  checkpoints, approval attempts, repair and advisory spend; unknown accounting
  coverage from TE1. Auxiliary requests are not main-model turns.

Use executed tests/workspace results for software and reviewed citations/source
support for research. A Strict Verified record means supplied evidence met that
review policy, not proof of task truth, safety or user acceptance.

## 5. Apply preregistered gates and stop rules

The plan's C8 proposes the utility gate; the owner approves it before sampling.
Choose a sample size with adequate power for the two-percentage-point success
noninferiority margin; a small pilot is allowed to be inconclusive. Use paired
outcomes where tasks/seeds match, report interval methods and retain discordant
pairs. Any severe false approval or authority breach stops the execution pilot.
Shadow classification alone never enables it. Do not tune on the held-out set,
relabel inconvenient failures, repeatedly sample until significance or silently
relax a budget/threshold to pass.

Run performance baselines and candidate measurements on a quiet host using
[perf recording](perf-recording.md): alternating A/B plus same-binary A/A,
fixed build mode, no overlapping heavy tests. Enforce the existing +5% off-path
gate and applicable absolute budgets. Loaded-host tail failures remain failures
or unqualified, never waived. Run the finite eight-hour live soak only after
contract and authority checks pass and within its explicit spend ceiling.

## 6. Publish a bounded receipt and rollout decision

The ledger names source hashes, actual commands, sample counts, pass/fail/unknown,
initial failures, cost coverage, intervals, approved tradeoffs and raw evidence
path. Keep per-mode outcomes; “tests pass” is not “Jev improves task success.”
Default-off is permanent policy for this plan, even if an opt-in arm wins.

Document server-side Off versus configuration inheritance and active-versus-next
mode. Use tested explicit revocation/cancellation to stop an execution pilot;
stop independently running advisory observers too. Keep remote in-flight spend
unknown until a supported receipt resolves it. Never claim key deletion rolls
back side effects or grants. Preserve previous exact grants visibly unless the
operator explicitly revokes them. Reverting a policy changes its pinned identity;
downgrading a schema requires a pre-upgrade backup, not a blind binary swap.

Do not close ENG-791, ENG-811 or ENG-815 solely because the docs PR, local
regressions, or a narrow feedback fix merged. Each owner's qualification gate
and the user's merge decision remain separate.
