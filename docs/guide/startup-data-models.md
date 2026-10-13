# Data and models

QQ, the assistant, and Retsu own different data. Keep those stores separate
when you document or operate them.

## QQ data

QQ stores sessions, events, runs, transcripts, trust state, and configuration
under its user-scoped data directories. The OS credential store keeps provider
secrets. The [configuration reference](configuration.md) describes layered
configuration, and [Install](install.md) describes safe session-store backup
before an upgrade.

QQ's protocol gives clients versioned commands and events. The TUI, headless
commands, and server use the same core runtime. Native clients read bounded
snapshots rather than recreating session state.

## Assistant data

The assistant candidate stores project bindings, conversations, scoped memory,
drafts, pending intent, and recovery metadata in a private SQLite store. It
does not own QQ provider credentials or copy the authoritative QQ transcript.
Memory scope and provenance stay explicit. A memory result is reference data,
not execution authority.

## Retsu data and models

Retsu's Grip design separates extraction requests, schema definitions, model
weights, usage records, and key or credit state. The model backend enforces
schema and span invariants. The gateway owns customer-facing authentication and
billing. The current local checkout is a private source snapshot, so its
model and deployment claims remain source-level until independently verified.

## Model status

A model name in configuration identifies a route. It does not prove that a
credential resolves, that a provider accepts a request, or that a deployment
serves that model. Record the route, source revision, execution environment,
and observed response separately.
