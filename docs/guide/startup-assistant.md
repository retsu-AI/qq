# Assistant gateway and nodes

The assistant is a separate client and orchestration layer around QQ. QQ
remains the execution engine for models, sessions, transcripts, runs, tools,
and provider credentials.

## Gateway boundary

The Rust assistant candidate stores projects, scoped memory, conversations, and
pending intent metadata in its own private SQLite database. It resolves a
canonical QQ workspace, creates or inspects a QQ session, prepares a bounded
prompt, and records the exact intent and receipt state.

The assistant does not mirror the QQ transcript as its source of truth. It does
not store provider credentials. A lost reply remains uncertain and needs an
explicit same-intent inspection or retry. Ordinary startup does not resend a
pending operation.

The candidate executable uses typed JSON input and bounded output. It refuses
unknown fields, malformed identity, invalid revisions, and unsafe database
ownership before changing domain state. The source and verification documents
are in the private assistant repository. Repository access is required.

## Node boundary

The Mac and iOS clients use QQ's HTTP and SSE contract. They display bounded
QQ snapshots and send explicit commands. The clients own local selection,
memory, drafts, and pending command identities. QQ owns session authority,
event ordering, approvals, provider calls, and execution.

The current native sources describe local loopback transport first. Remote
enrollment, TLS, revocation, device pairing, and durable reconnect remain
separate acceptance units. A native source check does not prove a real device
or provider journey.

## Current status

The gateway, Mac client, and iOS client are candidate source units. The full
assistant composition is not a deployed service. Missing node behavior,
provider output, hosted routing, and customer acceptance remain open. See
[Direct and assistant modes](startup-native-modes.md) and [Recovery](startup-recovery.md)
for the user-facing boundaries.
