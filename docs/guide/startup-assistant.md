# Assistant gateway and nodes

The assistant is a candidate Rust client and orchestration layer around QQ.
QQ remains the execution engine for providers, sessions, transcripts, runs,
tools, and approvals.

## Implemented local journal

The Rust candidate documents an executable conversation path in
`docs/units/U2B-CLI.md`. It registers projects, stores scoped memory, prepares
an exact prompt, records pending and uncertain operations, and inspects the
receipt-derived run. The U1A and U2A documents describe schema migration,
scope isolation, byte-preserving intent, and recovery rules.

The assistant store owns project bindings, conversations, memory, drafts, and
pending intent. QQ owns provider credentials, session and run state, tool
execution, and authoritative events. A lost response stays uncertain. The
assistant does not invent a replacement request or silently replay it.

This is a private candidate source at observed head
`ebd0dbb39cdb63cdc5eb9968366a568c442289b8` on `codex/rust-assistant-slice`.
Repository access is required. Local source checks do not prove useful model
output, a hosted gateway, or a shipped assistant.

## Planned gateway and node surfaces

The broader startup design names a gateway, authorized Mac and iOS nodes, a
dashboard, and memory, context, tool, plugin, and model boundaries. Those are
planned composition surfaces. They are not all implemented in the reviewed
candidate.

The Mac and iOS candidates own client selection, local memory, drafts, and
pending command identities. QQ remains the host authority. The clients do not
own a second agent engine or provider key store.

## Acceptance boundary

Provider execution, authenticated host enrollment, device pairing, remote
transport, revocation, dashboard behavior, and hosted deployment each need
their own evidence. A local journal check or a typed gateway request cannot
close those claims. See [Direct and assistant modes](startup-native-modes.md),
[Infrastructure](startup-infrastructure.md), and [Recovery](startup-recovery.md).
