# ADR-0063: Opt-in metadata-only structured tracing

- Status: Proposed
- Scope: developer-experience AF5 / ENG-1028

## Context

Durable events describe agent output, not time spent waiting at runtime
boundaries. Diagnostic logging must not become another transcript, block Tokio
workers on disk, or capture dependency HTTP headers and bodies.

## Decision

`QQ_LOG=info` installs a process-global subscriber in the binary. Absent, empty
or `off` installs nothing and creates no log files. Other values fail explicitly.
Only the exact `qq::diagnostic` target is admitted. Core emits run, turn,
provider, tool and store spans with numeric ordinals, not user content or
arbitrary error strings. JSON lines contain lifecycle timing and parent spans.
No wire, descriptor or store schema changes are involved.

The binary owns initialization and the writer thread. A bounded 256-record queue
accepts complete records up to 4096 bytes without waiting; overload drops whole
records. Each file stops accepting bytes at 8 MiB. Sixteen create-new slots in
`<state-dir>/logs/qq-N.jsonl` (local-data fallback where the platform has no state directory) bound retained disk to 128 MiB and refuse further
logging when full; users explicitly remove old files. QQ never deletes an active
process's diagnostic log. Normal shutdown drains the queue off the executor.
Unix directories/files are private; existing symlink components are rejected.
These are diagnostics, not authoritative durable events or complete audit logs.

## Consequences

The default path pays disabled tracing callsites but no writer or formatting.
Enabled diagnostics may omit records under pressure or after reaching the cap.
A stalled run can be diagnosed from unmatched start records; abrupt process
termination need not retain queued records. Enable logging on the server process
for remote clients. Performance acceptance compares the same existing budgets;
no budget is widened for logging. A full directory requires operator cleanup.

## Alternatives

Unfiltered `RUST_LOG`/dependency tracing risks leaking request content. Synchronous
file writes block runtime workers. Unbounded queues or per-process unlimited
files violate resource bounds. Automatic deletion/rotation introduces shared
process retention races; fixed exclusive slots are the smaller initial contract.
