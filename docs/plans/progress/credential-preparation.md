# Credential preparation progress

## CP1 candidate

- Owner: `/root/jev_contract_trace`; independent verifier: `jev_final_verifier`.
- Base: `eddff56e054e4ad6f16a4156660f3918782ac49d`.
- Candidate: `5b25a3f`.
- Baseline: `baseline-cold-same-key.cast` and matching logs recorded 8 Keyring reads
  for one cold same-key request where the regression expected 1.
- Focused checks on the candidate: two `qq-auth` coalescing/failure-retry tests and
  two `qq` server-catalog/remedy tests passed under Rust 1.97.1/Xcode 26.

The candidate uses one Keyring-only cache/flight coordinator. Success is published
to the bounded TTL cache before the flight is removed; followers receive the same
typed result, owner unwind removes the flight and notifies waiters, and a capacity
retry reloads the index/epoch after releasing the state lock. Attached clients keep
model options empty until the server-authoritative `Models` response; credential
index presence remains presentation-only.

Remaining verification belongs to the parent lane: full workspace gates and
independent exact-head review. Saturation and attached live-TUI behavior are not
claimed by the focused checks above.
