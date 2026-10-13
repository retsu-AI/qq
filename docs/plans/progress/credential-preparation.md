# Credential preparation progress

## CP1 integration candidate

- Existing auth owner: `/root/jev_contract_trace`; runtime/TUI integration: `/root`.
- Independent verifier: `/root/jev_final_verifier`; live acceptance/signing/install
  and canonical continuity writer remain `Ultra Manager`.
- Base: `eddff56e054e4ad6f16a4156660f3918782ac49d`.
- Candidate: current `fix/qq-credential-preparation-20261013` branch head; the
  retained final check receipt and independent verdict bind its full revision.
- Current source disposition: integration checks and independent final acceptance
  are pending. No live Keychain/provider/TUI acceptance is established.

The candidate uses a bounded Keyring-only cache/flight coordinator. Success is
published before flight removal; joined followers share typed results; owner
unwind and poisoned cleanup notify waiters and reclaim pending flights. Capacity
waiting drops the state/index lock and reloads current epoch metadata afterward.
Clients keep models and selection empty until the server's `Models` reply. Trust
refresh sends the configured route only as request metadata. Unavailable index
metadata and unresolved AWS sources are unknown, so they produce no definite
missing remedy. Server plan admission retains configured AWS-source checks.

## Retained evidence and corrections

The test-only baseline `a775e3e` recorded eight backend reads for eight concurrent
cold same-key callers where the regression expected one shared read. Its cast,
log, and rendered video remain retained. Earlier focused casts cover their own
historical revisions; they do not verify this integration candidate.

The parent full-head capture at `8b49271e95efad6fb7fa9cc34a6a3fdc6d9ed121`
executed six auth cases: five passed and shared failure/retry failed (three reads
versus two, including retry). The separate trust case passed. The failed cast,
logs, receipt, and video remain retained; that candidate is not accepted.

The auth-only `c8e999a` follow-up reports eight executed cases passing, including
observably joined success/failure/panic followers, two-key overlap, capacity
entry/target rotation, and active coordinator/result poison cleanup. Parent
integration and independent exact-head checks must verify the final subject.
Live original-session one greeting/one final review/one completion remains the
root-owned acceptance condition. The first macOS Keychain read's stall is not
proven resolved by source tests.
