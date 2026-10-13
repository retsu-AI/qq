# Credential preparation CP1

The attached TUI is a client: local credential-index presence may produce a
definite missing-credential remedy, but it never populates `model`, `models`,
`tui_model`, or `TuiOptions.model`. Selection and embedded initial-session
creation become available only after the server-authoritative `Models` response.
Trust refresh preserves this boundary; Alt-N cannot create from presence alone.
Unavailable credential metadata and AWS sources stay unknown without local
secret or AWS-file reads. Server plan route admission retains configured AWS
sources. Poison cleanup publishes typed failures and always wakes joined waiters;
ordinary admissions still fail closed after coordinator poison.

Keyring reads use a single cache/flight coordinator keyed by credential name,
endpoint, and epoch. It admits at most 64 distinct owners, publishes successful
results before removing flights, fans out typed failures, removes and notifies
on owner unwind, and waits on a predicate after releasing the state/index lock
before reloading current index metadata. File/WindowsProtected permission
validation and uncached OAuth refresh paths remain unchanged.

Acceptance requires deterministic tests for same-key coalescing, distinct-key
overlap, failure/retry, owner panic, poisoned coordinator fail-closed behavior,
capacity wait/revalidation, epoch/endpoint invalidation, and client bootstrap
selection/remedy boundaries. Live Keychain/provider/TUI behavior is a separate
root-owned acceptance.
