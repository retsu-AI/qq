# Credential attribution diagnostics

## Scope and process activation

DI1 adds a secret-free diagnostic session around existing credential-read hooks. It explains whether a blocked OS read belongs to stored Keyring coordination, OAuth/request-time work, or an explicitly known direct route. It does not change credential resolution, prompt behavior, cancellation, coordinator admission, protocol, sessions, or storage.

The exact operator activation is a new explicit global CLI option on the existing `qq`, `qq serve`, and `qq run` entry points:

```text
qq [--credential-diagnostics PATH] <existing command and arguments>
```

`PATH` is a requested final output path. Before constructing any runtime or reading credentials, the CLI exclusively creates that exact path as a new private regular file (`0600` on Unix; activation is refused on platforms without this private-output implementation), using no-follow/exclusive-create semantics. An existing file, symlink, directory, device, permission failure, or non-regular target rejects activation; QQ never truncates, appends to, renames over, or replaces an existing path. Activation failures use a fixed typed CLI error with no path, raw I/O text, or underlying error source; JSONL also excludes the path. The option is absent by default and cannot come from environment, persisted configuration, protocol data, credentials, or a live-session toggle. Startup creates one process-local `DiagnosticSession` before normal runtime/store composition; the same `Arc` session is passed into the normal `RuntimeFactory` store and separately constructed `advisory_reviewer` store. No additional `CredentialStore` or credential read is created.

The session owns a bounded JSONL writer. Closing the producer sends a completion signal after the writer drains and writes its reserved summary; the CLI waits exactly 250 ms for that signal and joins only after completion. On open/write/consumer failure or timeout, the output is marked incomplete: no false writer-joined or summary-written claim is made, and the CLI reports a fixed typed diagnostic failure without delaying credential/runtime work beyond the bound. Intentional graceful shutdown and consumer failure have distinct summary states. No diagnostic output is sent over QQ protocol or persisted in session storage.

## Typed event model

The encoder accepts only these variants and fields:

```text
DiagnosticEvent {
    session_id: OpaqueId,
    pid: u32,
    event: ResolutionAdmitted { attempt_id, route, endpoint, store_id, coordinator_id? },
    event: CacheHit { attempt_id, route, endpoint, store_id, coordinator_id },
    event: OwnerAdmitted { attempt_id, route, endpoint, store_id, coordinator_id, flight_id },
    event: FlightJoined { attempt_id, route, endpoint, store_id, coordinator_id, flight_id },
    event: BackendReadStarted { attempt_id, route, endpoint, store_id, coordinator_id?, flight_id?, read_id },
    event: BackendReadReturned { attempt_id, route, endpoint, store_id, coordinator_id?, flight_id?, read_id, outcome },
    event: ResolutionFinished { attempt_id, route, endpoint, store_id, coordinator_id?, flight_id?, read_id?, outcome },
    event: OwnerUnwind { attempt_id, route, endpoint, store_id, coordinator_id, flight_id, outcome },
    event: Summary { emitted, dropped, bytes, sink: Open | Closed | Failed | BudgetExhausted },
    monotonic_ns,
}
```

`route` is an enum: `StoredKeyring`, `CodexOAuth`, `XaiOAuth`, `DirectConfiguredKeyring`, `TypesafeReviewer`, `OtherKnown`. `endpoint` is an enum: `Typesafe`, `ChatGPT`, `Xai`, `CustomLoopback`, `Other`; the TypeSafe row always uses route `TypesafeReviewer` and endpoint `Typesafe`. URLs are never serialized. `outcome` is an enum: `Success`, `CacheHit`, `Missing`, `Unavailable`, `Failure`, `CoordinatorFailure`, `OwnerPanic`, `LockPoisoned`, `BackendReturned`. Arbitrary error text, `Debug`/`Display`, OS status, names, paths, account/item/service identifiers, hashes, prompts, content, and secrets are rejected by the encoder.

`attempt_id` identifies each caller attempt. `flight_id` identifies one stored owner flight; `read_id` identifies one actual backend call. Cache hits have no read or flight. Followers emit admission/join/finish but never backend-read events. Owner unwind does not imply backend return. `BackendReadReturned` means only that the existing synchronous call returned. Run, thread, and cancellation context are unavailable in DI1 unless an existing hook supplies them, so they are omitted rather than fabricated. A held fake backend must emit no return event before release and no event may claim OS cancellation.

IDs are opaque process-local counters from one shared session domain. Clones preserve `store_id`/`coordinator_id`; separately constructed stores receive distinct IDs. Thread IDs are omitted unless an existing typed caller context supplies one; thread debug text is never serialized. Counters saturate and disable diagnostics at exhaustion without reuse. IDs are allocated only when the option is enabled.

## Exhaustive hook map

| Route | Existing source hook | Coordination | Events and context |
|---|---|---|---|
| Stored Keyring | `qq-auth/src/lib.rs:1014-1152`, backend `:1175-1215` | Per shared `CredentialStore` `(name, endpoint, epoch)` flight | Admission/cache/owner/join/read/return/finish/unwind; store/coordinator/flight/read real; run/thread/cancel context unavailable and omitted |
| Direct configured Keyring | `qq-auth/src/lib.rs:1286-1302` migration/read path | No stored-flight claim | Only exact admission/read/return/finish if a typed caller hook exists; otherwise silent |
| Generic provider stored resolution | `qq-auth/src/lib.rs:1538-1574` | Enters stored path when registered | Same stored events; endpoint class from allowlist, never URL |
| Codex OAuth | `qq-auth/src/codex.rs:1340-1365` and request path `qq-auth/src/lib.rs:882-906` | Four existing request permits; uncached OAuth | Admission/read/return/finish at exact existing hooks; no stored flight/coordinator |
| XAI OAuth | `qq-auth/src/xai.rs:447-496` and `qq-auth/src/lib.rs:882-906` | Four existing request permits; uncached OAuth | Same OAuth events; no stored flight/coordinator |
| Normal server models/workspace tools | `src/runtime.rs:3183-3259` | Uses the RuntimeFactory-owned store | Route/endpoint only where the auth hook is exact; no guessed run ID |
| TypeSafe reviewer | `src/runtime.rs:typesafe_http_client` | The normal checkpoint shares the existing factory store; the existing advisory path retains its separate store. Both share the DiagnosticSession. | Each actual caller supplies its purpose: checkpoint/advisory uses `TypesafeReviewer`, while task routing and approval use `OtherKnown`; all classify this host as endpoint `Typesafe`. The call-scoped view preserves cache, backend, coordinator and store IDs without relabeling concurrent ordinary callers; distinct advisory-store IDs remain distinct. |
| File/WindowsProtected, unknown migration, uninstrumented routes | Existing auth paths | No DI1 attribution | Silent; no inferred event |


## Sink, budgets, and shutdown

The session owns a bounded nonblocking producer and a dedicated output consumer. Auth/coordinator code captures tiny typed metadata while its existing state/result locks are held, releases those locks, then attempts emission. It never performs file/network/SQLite/Keychain I/O or waits on a diagnostic lock. Each encoded JSONL line, including its newline, is capped at 512 bytes. The inclusive activation budget is 256 total records and 131072 total bytes: at most 255 ordinary records and 130560 ordinary bytes are admitted, reserving one 512-byte record for the writer-generated summary. Ordinary event bytes exclude the summary; `Summary.bytes` reports ordinary bytes only and is not self-referential. Queue overflow and every post-budget attempt increment the saturating dropped count. Ordinary output is drained and flushed before the summary status is chosen, so a flush failure yields a `Failed` footer. The single reserved summary is then attempted as a final direct write to the unbuffered output file, outside producer queue contention, even when all ordinary capacity is full. A footer-write failure is conveyed by the fixed incomplete close notice; completion is only reported after the writer has dropped the file, and an incomplete notice overrides any footer. Exact-bound (255 records/130560 ordinary bytes), one-over, line-cap rejection, trailing-drop, and failed-writer cases are required; line-cap rejection is typed and contains no rejected input.

Default-off construction allocates no session, sink, thread, ID, formatting, parsing channel, or additional backend call. Enabled diagnostics never retry, coalesce, or alter auth results. No dependency, wire schema, migration, global Keychain interaction toggle, ACL, securityd, auth-lock, or credential storage change is permitted.

## Acceptance tests and output paths

The implementation will be limited to `src/cli.rs`, `src/main.rs`, `src/runtime.rs`, `crates/qq-auth/src/lib.rs` plus a focused diagnostic module/tests, `crates/qq-auth/src/codex.rs`, and `crates/qq-auth/src/xai.rs`. No `qq-core` execution hook, protocol, or store path is in scope.

A credential-free composition fixture launches the actual CLI activation and JSONL encoder with private fake stores, loopback NoAuth configuration, and a held fake backend; it uses no real Keychain/provider/token/live session. Its fixed output is retained outside the source checkout:

```text
diagnostic-fixture-{revision}.jsonl
diagnostic-fixture-{revision}.log
```

It must assert literal outcomes and counts for paired default-off, enabled, and deactivated runs; absence of the option creates no file, sink, thread, or IDs; an existing file remains byte-identical; symlink, directory, device, non-regular, and permission-denied paths reject activation before runtime/credential construction. Enabled output must show one real PID, shared session identity across normal/advisory stores with distinct store IDs, cache hit, owner/follower, separate-store, poison/unwind/retry, distinct-key concurrency, and OAuth hooks. A held fake backend must produce no `BackendReadReturned` event before release and no event may claim OS cancellation. Exact ordinary limits are tested at 255 records/130560 bytes, one-over, 131072 total bytes including the reserved summary, line-cap rejection, trailing drops, writer failure, missing summary, and the 250 ms completion timeout; the reserved summary reports `emitted`, `dropped`, `bytes` (ordinary bytes only), and incomplete sink state. Adversarial names, paths, URLs, prompts, and error strings passed through the real encoder must be absent. Backend read counts must equal the disabled baseline.

Focused commands after approval are a pinned `cargo test -p qq-auth` selector for diagnostic fixtures and a credential-free CLI composition test, with an owned `CARGO_TARGET_DIR`; parent runs fmt, Clippy, workspace tests/build, and independent exact-head review. No live Keychain/provider or original-session trial is part of DI1.

## Deliberate omissions

DI1 does not infer Keychain prompt display, ACL state, securityd activity, same logical credential from a stack, OS cancellation, run identity, or drain completion. It does not add generic core cancellation plumbing. It does not instrument routes lacking exact context, add cross-process coordination, or create a durable diagnostic store. Those require separate changed runtime evidence and root authorization.

## Verification limits

`src/advisory.rs` only forwards the existing optional sink, and `crates/qq-auth/src/tests.rs` contains the necessary existing fake hook regressions. No core/protocol/session/dependency/provider/security changes or live credential reads are included.

The opt-in Unix implementation validates the private exclusively created descriptor and final path before launching the writer. Other platforms fail activation instead of silently claiming private permissions. Boundary tests use the actual bounded encoder/admission routines; synthetic padded JSONL exercises a byte bound that today's fixed-size typed events cannot naturally fill. Process fixtures reuse production activation/finalization with two fake stores and the actual advisory attachment helper; they do not invoke a provider or the installed QQ session. A captured terminal test run cannot establish native Computer UI acceptance.
