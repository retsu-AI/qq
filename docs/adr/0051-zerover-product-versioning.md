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

- **Client ↔ server:** `PROTOCOL_VERSION` is the only value compared
  across a connection, and the **client** does the comparing: before it
  attaches, local discovery (`qq-protocol::local`) and the health probe
  (`qq-server::probe_health`) reject a server whose advertised
  `ServerInfo.protocol_version` differs. The server does not check its
  callers; requests carry no protocol version and are authenticated by
  bearer token only, so a stale or custom client that skips the probe is
  not refused.
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
4. Because the product number does not signal breakage, every release whose
   contracts or configuration changed lists each changed contract constant
   (old → new) and the upgrade steps in an Upgrading block of its
   `CHANGELOG.md` section, which the release workflow publishes as the
   GitHub release body. The changelog's `**breaking:**` bullets, from `!`
   commits, remain the per-change record.

## Consequences

- Operators cannot infer compatibility from the product number. They read
  `qq version` (which prints all four contracts) and the release notes.
- Only the protocol is compared between processes, and only by the client
  before it attaches. Capabilities are advisory. Descriptor and store
  changes are safe because each is checked where it is used (plan identity,
  store open), not at a connection. A future contract that must be refused
  across a connection, or a server that must refuse stale clients, needs its
  own check; this ADR does not add one.
- Rolling back across a store schema bump still needs a store backup; the
  release notes must say so whenever the schema changed.
- Upgrade steps reach users through the GitHub release body, which the
  release workflow builds from the tagged `CHANGELOG.md` section (a
  hand-written Upgrading block plus generated entries), not only through the
  bump PR.
- The runbook rule this replaces is withdrawn. Its only content was the
  MINOR-on-contract-bump rule, so no earlier ADR is superseded.

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| Keep MINOR on every contract bump (the old rule) | Nearly every release bumps a contract, so every release would be a MINOR and the digit would carry no signal; v0.1.3 already broke the rule, so it described nothing real |
| Bump MINOR only for a protocol bump (the one cross-process check) | Still most releases (27 → 30 in one cycle), and descriptor or store bumps can break users just as much (store migrations are one-way); a partial rule invites re-litigating which contract "counts" |
| Leave 0.x for 1.0 and use SemVer MAJOR for breaks | The wire, store and config surfaces are still changing every cycle; committing to SemVer stability now would force either constant MAJORs or slower change. Leaving `0` stays a separate, later decision |
| CalVer (`YYYY.MM.N`) | Orders releases like 0ver but breaks the existing `0.1.x` ordering, crates.io and `cargo binstall` expectations, and the installer's version pins, for no compatibility signal gained |

## Evidence / references

- `crates/qq-protocol/src/local.rs` and `crates/qq-server/src/lib.rs`
  (`probe_health`, `into_connection`): the only `PROTOCOL_VERSION` comparisons;
  `authenticate` checks the bearer token only.
- `git diff v0.1.2..v0.1.3`: `PROTOCOL_VERSION` 25 → 27 shipped as PATCH.
- `git diff v0.1.4..HEAD` at the v0.1.5 review: protocol 27 → 30, descriptor
  9 → 11, store schema 35 → 39.
- [0ver](https://0ver.org); [`runbooks/release.md`](../runbooks/release.md).
