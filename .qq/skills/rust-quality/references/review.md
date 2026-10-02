# Evidence-based Rust review

## 1. Review the acceptance before the implementation

Read the issue/plan and scoped instructions, inspect the diff and relevant callers,
and state the behavior the change must preserve. Review two axes: spec conformance
first, then engineering correctness. Avoid substituting your preferred design for
the agreed scope or treating a stylistic preference as a bug.

Load only the relevant technical chapters from SKILL.md, then follow this order:

1. Trace one successful operation across validation, admission, execution,
   persistence, publication, and cleanup where applicable.
2. Trace each caller-visible failure and cancellation window through the same path.
3. Check invariants/types/ownership, resource bounds, compatibility, and hot costs.
4. Read regression tests; verify they fail for the intended pre-fix behavior.
5. Run narrow verification before workspace gates; inspect exact results and counts.
6. Inspect the final patch for accidental churn, missing docs, and unsupported claims.

Compilation plus Clippy establishes only part of the evidence. The borrow checker
cannot prove correct retries, no deadlocks, durable ordering, bounded queues, fair
admission, or the functional policy itself.

## 2. Technical review matrix

| Area | Questions | Evidence |
| --- | --- | --- |
| Functional boundary | Are inputs/effects explicit? Can rejection partially mutate? | transition/error tests, effect order |
| Ownership | Who retains each payload? Why clone/share? Are borrows narrow? | callers, lifetimes, allocation profile |
| Types | Can invalid states enter via fields/serde/defaults? | constructors and ingress tests |
| Errors | Is expected failure recovered or propagated intentionally? Are sources kept? | variant/source tests, callers |
| Async | Who owns tasks? Are joins observed? Can outer drop lose work? | shutdown/cancellation tests |
| Bounds | Items, bytes, tasks, completed outputs, blocking submissions all capped? | peak counters/saturation tests |
| Durability | Persist before publish? Unknown commit reconciled? | fault injection/replay |
| API/wire | Added bounds/variants/methods/fields break consumers? | callers, supported profiles/goldens |
| Performance | Relevant metric measured before/after? Workload comparable? | runbook receipt, benchmarks |
| Security | Workspace containment/approval intact? Secrets in errors/tracing? | existing policy tests, log fields |
| Simplicity | Abstraction earns its interface? Dependencies ship real behavior? | current call sites, scope |

### Concrete red flags

- `filter_map(Result::ok)` in an input pipeline: invalid items disappear.
- `unwrap_or_default()` on a store/network result: corruption becomes success.
- semaphore acquisition inside an unbounded spawn loop: task memory still grows.
- no reaping while permits recycle: completed outputs can accumulate.
- await between `mem::take` and restoration: cancellation loses pending state.
- snapshot/await/overwrite with no version validation: newer state can be lost.
- broadcasting before store append: consumer observes nondurable output.
- retry after timeout with a new operation ID: duplicate external effects.
- full-state clone/re-serialize for every token: likely scaling risk, measure it.
- replacing a sync mutex with async mutex to silence lint: deadlock/serialization
  risk remains unless the protected invariant and await dependencies are reviewed.
- `todo!`, ignored tests, broad lint allows, or unused public framework APIs: missing
  implementation is hidden rather than completed.

These are investigation prompts, not automatic findings: establish actual reachability,
caller contract, and consequence before reporting them.

## 3. Example review finding

**Blocking — `crate/src/worker.rs:87`: unbounded retained task results.**
The loop admits new workers whenever a permit becomes available but never calls
join_next until input closes. Finished tasks release permits while JoinSet retains
their results, so an indefinitely open stream grows memory despite the concurrency
limit. Reap results during admission and cap retained sink output. Add a test with
many completed jobs and a still-open input sender that asserts retained results
stay within the configured bound.

This names the mechanism, violated bound, consequence, correction, and regression.
"Use JoinSet" or "this code isn't functional" would not be actionable.

## 4. Severity and reporting

- **Blocking:** observable incorrectness, invalid accepted behavior, lost durability,
  safety/security breach, unbounded resource path, incompatible wire/API change,
  missing required acceptance/verification, or measured budget violation.
- **Should-fix:** concrete maintainability/design/test weakness without an established
  immediate violation. Explain the tradeoff and smallest improvement.
- **Nit:** optional readability/style preference. Do not bury correctness below nits.
- **Escalate:** the agreed spec conflicts with repository constraints or is wrong;
  follow the workflow instead of silently broadening the implementation.

Use path:line, affected condition, consequence, and specific fix/test. Distinguish
proven behavior from plausible risk and measured regression from speculation.
Don't claim "faster"/"leaks"/"deadlocks" without evidence or a demonstrated path.

## 5. Implementation finish receipt

Report:

1. **Behavior/design:** what changed; ownership and functional/effect boundary;
   justified traits, clones, boxing, shared mutation, or dependencies.
2. **Failure/resource contract:** relevant rejection/cancellation, bounds, durable
   ordering, recovery, and compatibility impact.
3. **Verification:** exact tests/fmt/Clippy/build/profile commands and results;
   baseline/post-change metrics where applicable; skipped/blocked gates and why.
4. **Risks/follow-ups:** concrete remaining gaps. No unsupported "perfect Rust" claim.

For review-only work end with Approve, Request changes, or Escalate, with evidence
and verification scope. No findings does not mean all behavior was proven; say
what was and was not inspected/tested.
