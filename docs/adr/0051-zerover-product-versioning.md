# ADR-0051 — 0ver product versioning; compatibility is carried by contract versions

**Status:** Accepted
**Date:** 2026-09-28
**Deciders:** Release lead (v0.1.5 release review, ENG-968 / ENG-969)
**Implements:** [`runbooks/release.md` § Choosing the product number](../runbooks/release.md#choosing-the-product-number-0ver)

## Context

Until v0.1.4 the release runbook said "any contract bump → `MINOR` while
`0.x`". It was not followed: v0.1.3 bumped `PROTOCOL_VERSION` 25 → 27 as a
PATCH release, and the v0.1.5 candidate carries protocol 27 → 30, descriptor
9 → 11, and store schema 35 → 39. Nearly every release bumps a contract, so
the rule would make every release a MINOR and the MINOR digit would stop
meaning anything. The product version is not what keeps mixed builds safe:

- **Client ↔ server:** `PROTOCOL_VERSION` is the only value checked across a
  connection. Local discovery (`qq-protocol::local`) and the server
  (`qq-server`) refuse a peer whose `ServerInfo.protocol_version` differs.
- **Capabilities:** `CAPABILITIES_VERSION` is advertised in
  `/v1/capabilities` for a client to read; nothing refuses on it.
- **Plan descriptor:** `DESCRIPTOR_VERSION` is local. It is part of the plan
  digest and cache key, so a changed encoding never shares identity with an
  old one; it is never compared across a connection.
- **Store schema:** `STORE_SCHEMA_VERSION` is local and checked when a
  session store is opened; migrations only go forward, and an older build
  refuses a newer store.

## Decision

QQ uses [0ver](https://0ver.org). The product version is `0.MINOR.PATCH`
and `MAJOR` stays `0`.

1. `PATCH` is the normal release step whatever the release contains,
   including contract bumps and `!` commits.
2. `MINOR` is a deliberate milestone chosen by the release lead, never an
   automatic consequence of a contract bump.
3. Leaving `0` is a separate decision with its own ADR.
4. Because the product number does not signal breakage, every release PR
   lists each contract constant that changed since the previous tag (old →
   new) and the steps a user upgrading from that tag must take. The
   changelog's `**breaking:**` bullets, from `!` commits, remain the
   per-change record.

## Consequences

- Operators cannot infer compatibility from the product number. They read
  `qq version` (which prints all four contracts) and the release notes.
- Only the protocol is enforced between processes. Capabilities are
  advisory. Descriptor and store changes are safe because each is checked
  where it is used (plan identity, store open), not at a connection. A
  future contract that must be refused across a connection needs its own
  check; this ADR does not add one.
- Rolling back across a store schema bump still needs a store backup; the
  release notes must say so whenever the schema changed.
- The runbook rule this replaces is withdrawn. Its only content was the
  MINOR-on-contract-bump rule, so no earlier ADR is superseded.
