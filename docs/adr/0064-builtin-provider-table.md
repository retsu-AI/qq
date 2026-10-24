# ADR-0064 — Built-in HTTP providers are rows in one table

**Status:** Accepted
**Date:** 2026-10-19
**Deciders:** ENG-1066 (auth-providers AP7)
**Implements:** [Auth and provider credentials](https://github.com/retsu-AI/qq/blob/130b27929e628338976d6f5f7b2d92e83302e585/docs/plans/auth-providers.md) § AP7

## Context

Five built-in HTTP providers (`openai`, `openai-codex`, `anthropic`,
`google`, `xai`) are each spelled out separately in at least six places:

- `qq-config`: a `ProviderKind` variant, a `ProviderEntryPatch` variant with
  its own field schema, an arm in `apply_provider`, `builtin()` and
  `build()`, a trust-declaration label, and either an
  `HttpCredential::ApiKey` preset or a dedicated
  `HttpCredential::{OpenAiCodex, XAi}` variant;
- the root: `LOGIN_PROVIDERS`, `built_in_endpoint`, `validate_login_mode`
  and the `auth_command` dispatch in `src/main.rs`, `CHOICES` in
  `src/init.rs`, credential checks in `src/doctor.rs`, credential
  resolution in `src/catalog.rs` and `src/runtime.rs`;
- `qq-harness`: the compiled-plan size estimate.

Adding a provider (the plan's Phase C adds five API-key presets) would
repeat all of that by hand. Three of these spellings are also contracts:

1. **The RON users write.** `OpenAi(api_key: ...)`,
   `OpenAiCodex(profile: ...)`, `XAi(api_key: ..., profile: ...)` and the
   others are documented and must keep parsing. Each variant accepts only
   its own fields under `deny_unknown_fields`.
2. **The trust digest.** `Document::sensitive_digest` hashes
   `serde_json` of the patch itself. A different serialisation would
   withhold project files users have already trusted until they re-approve
   them, and non-interactive runs would fail.
3. **Credential audiences.** Since AP3, OAuth audiences have exactly one
   home, in `qq-auth`.

## Decision

1. **One static table.** `qq-config` owns a `BUILTIN_HTTP_PROVIDERS`
   table. Each row holds the provider id, the patch variant name, the
   endpoint, endpoint mode and protocol, the usage type, the model catalog,
   the accepted credential fields (`api_key`, `profile`), and the
   credential source: an API-key preset (stored name, variables, audience)
   or a named OAuth provider. OAuth rows carry no audience; the root maps
   the name to `qq-auth`, which stays its only owner. `ProviderKind`,
   `builtin()`, `provider_credential_variables()`, the trust label and
   `apply_provider` read the table.
2. **The patch keeps its wire form.** `ProviderEntryPatch` keeps the
   externally tagged variant names and field order. Its built-in variants
   deserialise into one `Builtin { kind, api_key, profile, models }`
   representation, and serialise back to the original tagged form. Fields a
   row does not accept are rejected exactly as `deny_unknown_fields` rejects
   them today.
3. **The digest is pinned.** A table of the trust digests of every
   built-in patch shape, recorded before this change
   (`builtin_provider_patches_keep_their_trust_digests`), must stay
   byte-identical. A future change that alters them needs its own ADR and a
   trust-record migration.
4. **Credentials collapse.** `HttpCredential::{ApiKey, OpenAiCodex, XAi}`
   become one `HttpCredential::Builtin { provider, api_key, profile }`.
   Request-shape identity bytes stay as they are: the digest arm maps each
   row to the marker its old variant wrote (`built-in-api-key` plus the
   audience, `request-time-codex`, `request-time-bearer`).
5. **Login methods come from the table.** Each row lists the login methods
   it supports (API key, device code, loopback browser). `LOGIN_PROVIDERS`,
   `built_in_endpoint`, `validate_login_mode`, `auth_command` dispatch,
   `CHOICES` and the doctor's remedy text all derive from the table, so a
   new provider adds no branch in `src/main.rs`.
6. **No new dependency edges.** `qq-config` and `qq-auth` stay independent.
   The root joins them by provider name, and a root test checks that every
   OAuth row has a `qq-auth` counterpart.

## Consequences

- Adding an API-key preset is one table row plus its model catalog, docs
  and canary entry.
- The table is data, not behaviour. Per-provider wire quirks (for example
  catalog pagination in `src/catalog.rs`) stay in their protocol code,
  keyed by row, until they are generalised separately.
- The custom `ProviderEntryPatch` serde implementation is new code on a
  security boundary (trust). Pinned digests and per-row
  positive and negative parse tests cover it.

## Alternatives rejected

- **A `Builtin { kind, ... }` RON form.** Simpler code, but it changes the
  syntax users write and every trust digest.
- **Putting the table in `qq-auth`.** That gives `qq-config` a dependency
  on `qq-auth`, and it puts presets and catalogs next to credential storage.
- **Leaving the variants and generating them with a macro.** That hides
  the duplication instead of removing it, and the root's string matches
  stay.
