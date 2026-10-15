# Performance gate repairs

| Slice | Goal | Status | Branch/PR | Evidence |
| --- | --- | --- | --- | --- |
| ENG-834 | Repair inherited size/retry failures and qualify unchanged speed limits | In review | [#315](https://github.com/retsu-AI/qq/pull/315) | User authorizes follow-up after #306 merged; ceilings remain 48/41 MB |

## 2026-10-14 — baseline and ownership

- Merged input: `9868038c994cc0c0b64795731d1402ca21ff6348` (#306).
  Root is the sole writer in `.worktrees/perf-eng-834-gates`; shared-file
  ownership is requested in the root ledger.
- Hosted prior-head H0 run `38646957393`, artifact `11934628926`: main
  `62c65930` default/minimal sizes 52,961,016/46,294,328 bytes exceed the
  unchanged 48,000,000/41,000,000 ceilings. AB6 adds 32,864/24,672 bytes.
  Candidate passes relative limits; same-binary control fails HTTP-ack and
  one-subscriber fanout tails. Retry baseline/candidate/control are 6,000
  milli, the ADR-0040 expectation; external budget remains 1,050 milli.
  No failed gate is waived. Current AB6 H0 run `38649895250` remains active.
- Pre-change optimized local size diagnostic at exact AB6 source `e0d0fb94`
  (same source tree as merged input, except merge identity): symbol-preserving
  build and separate stripped copy retained under the AB6 worktree's
  `target/qq-perf/ab6-2026-10-14/size-diagnostic/`. Linked parser symbols total
  10,545,999 bytes, of which 10,024,360 are static data. These diagnostic
  sizes are not hosted qualification. Fat-LTO experiment remains live;
  no production profile change yet.

## 2026-10-14 — focused repairs and compiler experiments

- Regression `retry_performance_budget_accepts_turn_recovery_but_rejects_an_extra_send`
  fails on the old budget (0 pass/1 fail), then passes after the ADR-0040
  correction to 6,300 milli. Relative allowance remains zero; a seventh send
  exceeds the absolute ceiling. The real faulting-provider fixture
  `retry_amplification_fixture_observes_the_turn_retry_bound` also passes.
  Red/green logs: `/tmp/qq-eng834-retry-budget-{red,green}.log`; real fixture
  log: `/tmp/qq-eng834-retry-fixture.log`.
- Full LTO at unchanged optimization level reduces the optimized local default
  binary from 53,112,568 to 48,288,920 bytes (-9.08%). Full LTO with level 2
  only for the final `qq` package produces 47,919,896 bytes, below 48 MB;
  runtime libraries keep level 3. All features remain present. Proposed root
  profile adopts this combination; minimal measurement and speed qualification
  remain pending. Symbol-preserving and stripped binaries, hashes, build logs
  and receipts remain in the pre-change diagnostic directory; local sizes are
  not hosted qualification.
- CI pins merged #306 as the baseline/common fixture, retains every speed/size
  limit except the approved retry-contract correction, and repeats the
  candidate as its same-binary control. Exact inline budget guard admits the
  retry correction and rejects a one-byte relaxation of the size ceiling.
  All four inline Python blocks compile; actionlint passes with the existing
  unrelated SC2129 release-step style warning excluded.

## 2026-10-14 — resumed after explicit pause

- User explicitly resumes the follow-up. Main remains `9868038c`. Root-only
  size optimization with full LTO gives minimal 41,198,312 bytes, still
  198,312 bytes above the unchanged ceiling. Stronger root-only size
  optimization is being measured before selecting the final profile.
- AB6 hosted H0 run `38649895250` completed with failure; artifact
  `11935749016` is retained on GitHub. No performance acceptance is claimed.

## 2026-10-14 — selected profile for verification

- Full LTO with root-only optimization `z` gives local minimal 40,330,088
  bytes, below the unchanged 41 MB limit. Features and dependencies are
  unchanged; dependency crates retain level 3. Both `qq` and the H0 driver
  adopt this final optimization. This supersedes the provisional level-2
  setting above. Runtime speed remains unqualified pending matched checks.
- Protected root Git metadata is read-only in the resumed environment. An
  independent local clone under `/tmp` holds the exact verification snapshot;
  original worktrees and their unrelated changes remain intact. GitHub
  publication uses the connector.

## 2026-10-14 — normal-build stripping regression

- Normal Cargo builds of local source `897aaad8` measure default 51,730,520
  and minimal 46,691,400 bytes, failing both ceilings. Captured rustc
  invocation has `opt-level=z`, `lto=fat`, but `strip=debuginfo`, retaining
  symbol tables. Explicitly stripping those exact files gives default
  44,239,768 and minimal 40,330,088 bytes. Make `strip="symbols"` explicit
  in both executable package overrides and verify normal builds again.
- Exact snapshot formatting, Clippy, workspace build and focused retry/color
  checks pass. Local workspace socket tests are blocked by EPERM; hosted
  correctness/performance remain pending. GitHub tree creation is blocked by
  the session approval policy (`requires approval`, policy `never`).

## 2026-10-15 — publication and Codex review

- Normal builds at `f05340b2`: default 44,239,776 bytes; minimal 40,330,096
  bytes, both within unchanged ceilings. Workspace fmt/Clippy/build and focused
  retry/context/export/render checks pass. Source and full receipts are in
  the AB6 worktree's `target/qq-perf/eng834-2026-10-14/`. Normal git push
  succeeds; nondraft PR #315 is open and conflict-free.
- Codex's ADR and retry-inventory findings apply: add ADR-0062, mark ADR-0010
  superseded in part, update the index and reconcile the inventory. The
  profile-validation finding does not reproduce on pinned Cargo 1.97.1:
  `cargo rustc --release --message-format=json -v -- -C opt-level=z` reports
  artifact profile 3 while the final rustc override is z. CI run 38681591675
  likewise passes both benchmark builds/profile checks and starts recording.
  Retain the common Cargo-profile check and per-arm final-opt receipts.
- Hosted fmt/Clippy/workspace tests, wasm and Windows pass at `f05340b2`. H0
  job 116247138155 and turn job 116247138237 are still running; runtime
  qualification and final-head checks remain open.

## 2026-10-15 — per-run retry bound

- Codex finds averaging hides a rare overrun: nineteen six-entry runs plus one
  seven-entry run report 6,050 milli and pass the old 6,300 correctness gate.
  Regression-first commit `7c92c057` reproduces this (0 pass/1 fail).
- Keep the mean metric and unchanged budgets; independently require every
  sampled run to stay within `MAX_TURN_RETRIES + 1`, retaining all counts in
  the correctness receipt. Regression and real retry fixture pass (2/2).
- H0 overlays only candidate-pinned `xtask/src/perf.rs` into both immutable
  production arms. Receipt records original/effective hashes and commits;
  baseline workspace modification remains explicit. Guard checks pass for
  the valid overlay and refusal of dirty input, wrong SHA and a relaxed budget.
- Workspace build, Clippy, formatting and workflow lint pass; independent
  review approves. Local socket creation still returns EPERM, so full hosted tests
  remain required. Earlier H0 records do not prove the repaired per-run bound.

## 2026-10-15 — H0 failures and serialization diagnosis

- H0 `38683573275` completes all recordings in 31m; artifacts `11950341434`
  retain unchanged-limit sizes 44,116,376/40,211,800 bytes and passing per-run
  retry receipts. Five A/B and five A/A tail checks fail; no gate is waived.
- Replay median repeats the earlier regression: baseline 2,301,568 ns, candidate
  3,343,105 ns (+45%), control 3,357,095 ns. Other noisy tails remain open.
- A same-output CPU probe built from the release protocol/JSON/SHA dependencies
  alternates 3/s/z across 12 samples per case. `z` is +99% for event encoding
  plus hashing and +208% for headless JSON encoding versus 3; `s` is faster in
  this diagnostic. Probe source, binaries and samples stay in ignored artifacts.
- Root/driver/auth `s` gives minimal 41,053,296 bytes (fails by 53,296); adding
  config `s` gives 40,896,752 bytes (passes). All other libraries remain level 3.
  These are local diagnostic sizes; exact committed-source validation follows.
- CI streaming preserves complete logs while exposing compiler output, arm
  durations, exit codes and workload stages outside sample timing. Fake-command
  success/failure checks retain stdout/stderr, reports and stop-on-error behavior.

## 2026-10-15 — balanced profile and observable recording

- Clean-source `9fbe5307` normal release builds pass unchanged limits: default
  44,969,760 / 48,000,000 bytes; minimal 40,896,752 / 41,000,000 bytes.
  Binary hashes, logs and source receipts remain in the ignored ENG-834 artifacts.
- Independent review approves profile and timing placement, but finds a console/log
  write error can abandon recorder descendants. Failed sinks are disabled while
  output continues draining; record exit status/reports before failing the step.
  Fake-command regressions pass success, command failure, console failure and log
  failure, including descendant completion and retained receipts (4/4).
- Common-fixture guards pass all four positive/negative cases; workflow actionlint
  passes with ShellCheck disabled (existing SC2129 style finding is unchanged).
  Current-head workspace checks and hosted performance qualification remain open.

## 2026-10-15 — current-main integration

- Remote main advances to `3b763ea1` with the agent feedback gate and test
  cancellation audit while `25dff5af` CI is active. Merge main without conflicts;
  retain the new check/lint/smoke workflow and existing performance jobs. Pin
  production baselines to current main; the immutable turn fixture remains
  `9868038c`, with original/effective hashes still required. No budget changes.
- Exact clean `25dff5af` release sizes confirm 44,969,760/40,896,752 bytes,
  Codex reports no major issues (comment6185400887), and all four threads are
  resolved. These pre-integration receipts do not qualify the merged head.
- Re-run the current-main gates and hosted qualification before merge. Local
  loopback bind remains EPERM; socket-dependent gates require hosted evidence.
- Independent review approves integration; correct the runbook's production
  baseline to `3b763ea1`, retaining turn fixture `9868038c` explicitly.
- First integrated `check --full` passes fmt/lint, then fails Clippy when the
  shared target directory disappears during the build (ENOENT fingerprint writes).
  Re-run in the private clone's target. Prior shared binary/log receipts were
  removed too; recover available hosted ZIPs/probe data from `/tmp`. Earlier size
  observations remain historical; merged-head measurements are still required.

## 2026-10-15 — require complete recovery in faulting H0 runs

- Codex thread4279627949 finds the per-run upper bound accepts lost attempts:
  nineteen six-send runs plus one five-send run passes at 5,950 milli. Red
  commit `e828f39c` reproduces this (1 failed regression).
- Require exactly `MAX_TURN_RETRIES + 1` entries per always-faulting run. Keep
  the mean metric, runtime policy and all budgets unchanged. Regression covers
  occasional five/seven entries and uniformly five entries; focused Nextest
  retry checks pass 3/3, including the real runtime fixture. Independent review
  approves; inventory and runbook describe both under-runs and over-runs.
- Current-head full checks and hosted qualification still required. Previous
  `2f7c5af2` hosted verification passes all seven gates plus smoke 6/6; its
  recordings do not qualify this stricter assertion.
