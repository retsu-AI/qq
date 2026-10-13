# Direct and assistant modes

The native candidates are clients of QQ. The reviewed sources do not establish
that both direct and assistant UI modes are currently shipped.

## Current client source

The Mac candidate at observed head `a4a1d247a26c7019e4d4cec1ea84f3cd4ed309cd`
and the iOS candidate at `a3ef908573b73fbc3fe35dd00336fa4ef2eab21a` share a
QQ HTTP and SSE contract. Their `SPEC.md` files describe discovery, protocol
validation, workspace and session selection, bounded snapshots, explicit
approval display, reconnect, local memory, drafts, and pending command ids.
Repository access is required.

These source units implement local client journals and client-side state
boundaries. They do not prove a shipped native app, useful provider output, a
paired phone, or a remote host.

## Direct mode

The intended direct mode reads a private QQ discovery record, validates the
server identity and protocol, selects a workspace, and submits a prompt to
QQ. QQ owns the session, provider, approval, tool, and event authority. A
client timeout remains uncertain and cannot trigger an automatic duplicate.

Direct mode is a planned user-facing composition of the candidate client and
the stable QQ runtime. The reviewed evidence does not establish its final UI.

## Assistant mode

The intended assistant mode adds project and memory selection through the
assistant gateway before the assistant uses QQ for the run. The assistant
store owns selected reference data and intent metadata. It does not grant tool
authority or replace QQ's approval policy.

The gateway, dashboard, and memory, context, tool, plugin, and model surfaces
remain planned composition work. Do not document assistant mode as available
until a current native client, gateway, and QQ run are accepted together.

## Remote transport and embedded Tailscale

The Mac source includes an embedded Tailscale launch transport for macOS 15.6
and later. The source contains node identity, enrollment, scoped grants, and
shutdown handling. The iOS source describes the corresponding client contract.
Remote pairing, key custody, revocation, durable reconnect, physical-device
acceptance, and provider output remain open. A source test or local loopback
run does not prove a Tailscale node or paired phone.
