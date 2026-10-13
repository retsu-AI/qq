# Infrastructure

Keep local source, staging, production, and customer acceptance as separate
states.

## Local and staging

QQ runs locally as a TUI, headless command, or server. The server binds to a
loopback address by default. Remote clients require an explicit configured
connection and transport boundary. A local server or fixture does not prove a
hosted deployment.

The Retsu source design describes a gateway in front of a private Grip model
backend. Its deployment notes describe GPU hosts, health checks, metering, and
blue-green replacement. Those are infrastructure design and runbook inputs.
They do not establish that a staging or production service is running.

## Production boundary

A production claim needs the exact repository revision, configuration, account
and deployment result, health observation, and rollback path. A green build,
an applied plan, or a source receipt does not prove traffic reached the new
revision.

Do not place account numbers, credentials, private hostnames, or deployment
logs in this guide. Use the private infrastructure runbooks with the access
they require.

## Deployment status

The hosted orchestration boundary, staging and production automation, DNS,
and customer-facing deployment remain outside this documentation change. See
[Observability](startup-observability.md) and [Recovery](startup-recovery.md)
for the checks that must accompany an operational claim.
