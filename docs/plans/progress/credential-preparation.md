# Credential preparation feature map

Stored Keyring reads share a bounded coordinator keyed by credential, endpoint and epoch. Success is published before flight removal. Followers share typed results, and owner unwind or poisoned cleanup wakes waiters. Capacity waits release the state/index lock and reload current epoch metadata before admission.

Attached clients keep models and selection empty until the server's `Models` response. Trust refresh supplies route metadata without treating local credential presence as server authority. Unavailable index metadata and unresolved AWS sources remain unknown.

| Journey | Source | Verification |
|---|---|---|
| Same-key cold reads and cache reuse | `crates/qq-auth/src/lib.rs` | Stored Keyring coordinator cases in `crates/qq-auth/src/tests.rs` |
| Failure, retry, unwind and poison | `crates/qq-auth/src/lib.rs` | Joined follower and cleanup regressions in auth tests |
| Distinct-key capacity and epoch changes | `crates/qq-auth/src/lib.rs` | Capacity-entry and target-rotation cases in auth tests |
| Server-authoritative model selection | `crates/qq-tui/src/app.rs` | Client bootstrap and trust cases in `app/tests.rs` |

Each check and independent verdict applies to its recorded revision. Source checks do not establish live Keychain consent, provider execution or native TUI acceptance.
