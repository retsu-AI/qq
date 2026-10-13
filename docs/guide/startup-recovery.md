# Recovery

Recover from the smallest authoritative state. Preserve an uncertain action
until you know whether the provider or host received it.

## Recover a QQ session

Use the session identifier printed by QQ to reopen the same state:

```sh
qq --session SESSION_ID
qq run --session SESSION_ID "Continue from the last saved state"
```

The first command opens the TUI; the second submits a durable headless
continuation. Read [Headless](headless.md) before choosing a retry. The CLI
session boundary is in
[src/cli.rs](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/src/cli.rs)
and durable run/session handling is in
[src/headless.rs](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/src/headless.rs).

A timeout or lost response is not proof of zero effect. Inspect the
authoritative session and provider result before sending a replacement request.
Cancellation is a separate outcome; preserve its request and response IDs.

## Recover stores safely

Stop processes before copying a QQ SQLite database and its WAL and shared-memory
files. If QQ must remain active, use SQLite's consistent backup path described
in [Install](install.md). Do not copy a live database directory as if it were
a snapshot. Record the database revision and backup destination without
publishing private records.

The assistant candidates keep separate private stores and pending-intent
records. Their recovery operation reuses stored identity, source revision, and
request body. It does not manufacture a replacement intent after an ambiguous
response. See [assistant](startup-assistant.md).

## Recover native clients

After stream loss or restart, native clients refresh the authoritative QQ
snapshot and preserve an uncertain command id for an explicit retry. Device
enrollment, revocation, remote reconnect, and protection policy remain separate
candidate acceptance units. Embedded Tailscale source exists in the reviewed
Mac candidate, but a source implementation is not proof of paired device
operation.

## Report the limit

Every recovery report names source or binary revision, operation, stored
identity, expected result, observed result, and unverified dependency. A
restored file, passing fixture, local reconnect, or closed stream does not
prove provider output, deployment, payment, or customer acceptance.
