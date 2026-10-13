# Recovery

Recover from the smallest authoritative state. Preserve an uncertain action
until you know whether the provider or host received it.

## Recover a QQ session

Use the session id printed by QQ to reopen the same session. `qq --session ID`
opens it in the TUI, and `qq run --session ID "prompt"` submits a durable
headless continuation. Check [Headless](headless.md) before choosing a retry.

An interrupted run is recovered before a new headless operation. A timeout or
lost response is not proof of zero effect. Inspect the authoritative session
and provider result before sending a replacement request.

## Recover stores

Stop every process before copying a QQ SQLite database and its WAL and shared
memory files. If QQ must remain active, use SQLite's consistent backup path.
Follow the upgrade and backup procedure in [Install](install.md). Never copy a
live database directory as if it were a consistent snapshot.

The assistant candidate keeps its own private store and intent records. Its
recovery operation uses the same stored identity, revision, and request body.
It does not generate a replacement intent after an ambiguous response.

## Recover native clients

Native clients refresh the authoritative QQ snapshot after stream loss,
restart, or a foreign cursor. They preserve an uncertain command id for an
explicit retry and do not silently resubmit it. Device enrollment, revocation,
and remote reconnect remain separate candidate acceptance units.

## Report the limit

For every recovery report, state the source or binary revision, operation,
stored identity, observed outcome, and unverified dependency. A restored file,
passing fixture, or local reconnect does not prove provider output, deployment,
payment, or customer acceptance.
