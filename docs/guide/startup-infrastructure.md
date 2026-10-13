# Infrastructure

Separate current source configuration from the selected hosted architecture.
Neither a source file nor a deployment plan proves a live service.

## Current source path

The Retsu gateway is a Cloudflare Worker. Its routes and Effect services live
under `packages/api/src/`; its D1 migrations live under `packages/api/migrations/`.
The web application is a separate Worker under `packages/web/worker/`. The
deployment runbook describes GitHub Actions and Cloudflare-side builds for
those Workers. The reviewed private snapshot does not prove that the current
remote environment is deployed or that DNS points to it.

The gateway source points to a Modal Grip backend through `BACKEND_URL` and
uses `BACKEND_SECRET` and Modal proxy settings. The model process is the Rust
`grip serve` source under `crates/grip-server/`. This is the current
Cloudflare-to-Modal source path. It is separate from the selected AWS target.

## Selected hosted target

The startup architecture selects trusted AWS ECS services for the hosted
gateway and workers, with one qualified Docker worker or engine per cell.
Temporal is the sole hosted scheduler and Postgres is the durable hosted state
store. Staging and production branches, DNS automation, and the
staging-to-production path are planned source work. They are not established
by the Cloudflare or Modal files above.

The capabilities API route was observed returning 404 during review. The
proposed staging and Mesh DNS names did not resolve (NXDOMAIN). Treat those
capabilities and hostnames as unverified until a current route read, DNS read,
deployment identity, health result, and rollback record are available.

## What an operational claim needs

For a staging or production claim, record the exact source revision, branch,
target account and service, applied configuration, deployment result, health
response, DNS readback, logs, and rollback result. A passing build, an
infrastructure plan, a Worker configuration, or a Modal URL alone is not
deployment proof.

Do not publish account identifiers, secrets, private hostnames, or deployment
logs here. Use the private infrastructure runbooks with their required access.
See [Observability](startup-observability.md) and [Recovery](startup-recovery.md)
for the evidence to retain after a failure.
