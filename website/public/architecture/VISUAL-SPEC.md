# Startup architecture visual specification

## Outcome and owner

One master architecture explains the current Retsu and QQ startup system. Every entry has a Greek visual alias, its actual technical name, purpose, status, state owner, trust boundary, and source reference. Aliases change no deployed resource, API, repository, or product branding.

The visual author owns only `website/public/architecture/` on `docs/master-architecture-20261013`. The documentation owner integrates this module into the existing QQ Astro and Starlight website. The existing specification owner controls architecture decisions; the startup manager alone writes root continuity. One independent architecture reviewer verifies the final artifact.

## Readers and journeys

A founder first sees eight colored zones and named connections. Selecting a service opens its evidence and state ownership. Search and a textual inventory give keyboard users the same information. Zoom preserves legibility without forcing all fine detail into the initial view.

The paid-request view follows sign-in, offer and credit qualification, payment fulfillment, admission, provider execution, and durable settlement. It distinguishes the existing extraction path from the proposed hosted-run path. A response, status endpoint, or credit balance does not establish a qualified paid journey.

The QQ improvement view follows detect errors, assess the environment, repair, verify, independent review, pull request, and versioned update with rollback. The same QQ engine serves direct clients and the assistant. It shows the unresolved provider prerequisite and preserves the existing session.

## Representation and storage

`services.json` is the authoritative visual registry. It contains source records, zones, service records, directed relationships, and ordered journey steps. Service records carry independent source/deployment/acceptance descriptions. Relationships identify observed wiring, source wiring, or required design wiring; dashed lines cannot look like deployed connections.

`generate.mjs` renders a standalone SVG and self-contained HTML from that registry. The HTML embeds its registry, so local files need no server or fetch permission. An accessible HTML inventory complements SVG buttons and a detail panel. URLs use relative module paths. No third-party scripts, fonts, tracking, or runtime dependencies are added.

This static artifact stores no user records, credentials, payments, sessions, or product state. Durable product stores remain explicitly separate: QQ sessions; Rust assistant SQLite; native client SQLite; D1 identity/application state; AccountLedger money; trusted PostgreSQL run state; Temporal history; local data catalog and objects.

## Trust and privacy

Zones identify the device boundary, local host, public Retsu edge, supplier boundary, hosted execution, data review, operations, and acquisition integrations. Tailscale provides reachability, not device/effect authorization. Identity, money, run admission, scheduling, data approval, and tool permissions do not confer each other's authority.

Public assets contain repository paths, public revision identities, and concise source summaries. They omit host filesystem paths, private account and resource IDs, raw conversations, credentials, and diagnostics. They describe source evidence without redistributing private specification contents.

## Preservation and recovery

No product database, schema, resource, route, or deployed name changes. Historical master specifications and diagrams remain intact. Git preserves the registry and renderer; regeneration reproduces the HTML and SVG. Diagram release rollback is a Git revert of this isolated static module. There is no data migration or backup operation.

## Failure and concurrent work

Generation refuses missing sources, duplicate IDs, unknown edges, missing statuses, or unknown journey references. No production endpoint is called. Unsupported or unverified integrations remain explicit. The docs peer has separate guide/sidebar ownership and imports only the reviewed module commit. Source snapshots have a date; later implementation does not silently update this artifact.

## Acceptance

1. The registry enumerates all named current startup components from the agreed source set and identifies proposed components separately. It includes acquisition and operational dependencies without inventing deployments.
2. Every service and edge has a traceable source. Greek aliases always appear beside technical names.
3. The overview has readable titles, zones, labels, and a status legend. The two journey views are understandable without following crossing arrows.
4. A real browser verifies master, both journeys, service details, search, keyboard use, zoom, and a narrow viewport. Retain screenshots and an actual interaction video outside public assets.
5. The module regenerates deterministically and passes registry/reference/privacy validation. The relevant existing website build passes on the reviewed composition.
6. Independent non-author acceptance binds the exact source commit and artifact hashes. The authorized branch push and pull request are read back; hosted checks are reported separately.

## Throughput checkpoint

- Blocking first steps: freeze current source references and exclusive asset path, then write this specification before implementation.
- Independent workstreams: one reviewer examines architectural claims while the author builds the module. The docs owner edits other paths.
- Shared mutable state: the visual registry has one writer. Root continuity and canonical specification remain with their existing writers.
- Smallest safe decomposition: one artifact author and one reused reviewer. Extra research or code authors would duplicate existing ownership.

## Tradeoffs and deliberate omissions

The initial overview groups services into zones. The complete inventory and detail panel retain individual records. This keeps connections readable while allowing full coverage. Greek names are navigation aids, not an authority hierarchy. The artifact is a dated explanation, not live telemetry, an operational dashboard, a complete system proof, or evidence of product acceptance. No publishing, merge, cloud spending, or deployment is performed by the module.
