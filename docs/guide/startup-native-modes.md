# Direct and assistant modes

The same native client family can connect directly to QQ or act as a client of
the assistant gateway. The modes share the QQ execution authority.

## Direct QQ mode

In direct mode, the client discovers a local QQ server, validates its identity
and protocol, selects a workspace, and reads or creates a session. It submits a
prompt through the QQ HTTP and SSE contract. The client does not implement a
second agent loop or grant tool authority.

The Mac source describes local loopback transport, bounded requests, explicit
approval display, reconnect snapshots, and private local SQLite for drafts and
memory. The iOS source consumes the shared client contract and keeps remote
pairing as a separate host dependency.

## Assistant mode

In assistant mode, the client talks to the assistant boundary for project and
memory selection, then the assistant uses QQ for the authorized session and
run. The assistant store can retain intent and recovery metadata. QQ remains
the authority for provider credentials, approvals, tools, and durable run
events.

The client must show which authority owns a pending action. Recalling memory
does not grant permission. A timeout is uncertain. A reconnect refreshes the
authoritative state and does not silently repeat a command.

## What is available

The native clients and assistant gateway are candidate private source units.
Their source checks do not prove a shipped Mac app, an iOS device pairing, a
remote host, useful provider output, or a deployed assistant. Treat those as
separate acceptance claims. See [Assistant gateway and nodes](startup-assistant.md)
and [Infrastructure](startup-infrastructure.md).
