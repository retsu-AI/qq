# Auth and provider consolidation

Five stacked PRs that remove dead auth paths, collapse duplicated provider
plumbing and harden credential handling. They build on the ENG-1066 series
(#343–#376) and ADR-0064; none of them reopens those decisions. Each PR is
reviewable and revertible on its own and lands before the next starts.

## PR 1 — Remove dead auth code

- Delete the static Codex intent (`HttpAuth::Codex`,
  `ResponsesAuth::Codex`); the runtime only builds `RequestTimeCodex`.
- Delete `HttpAuth::RequestTimeGoogleApiKey` and
  `RequestCredential::google_api_key`: no runtime or xtask caller. Google
  keys resolve once into a static `x-goog-api-key` header.
- Delete refresh-path error-mapping arms for login-only errors (Codex
  `AuthorizationDenied`/`CallbackTimedOut`, xAI
  `AuthorizationDenied`/`AuthorizationTimedOut`).
- Narrow `qq-auth` items used only inside the crate to `pub(crate)`.
- Stop registering `anthropic-version` for redaction; it is not a secret.
- Docs: ADR-0064 status, stored name `PROVIDER/PROFILE`, xAI/Codex
  resolution notes, the design note on Google request-time keys.

Proof: `qq-provider` and `qq-auth` tests; `cargo xtask check`.

## PR 2 — One static-auth path in `qq-provider`

- Replace `ResponsesAuth`, `ChatCompletionsAuth`, `AnthropicAuth` and
  `GoogleAuth` with one `StaticAuth { None, Bearer, Header(name, secret) }`
  and one `build_static_headers`. `resolve_http_auth` maps `ApiKey` to the
  protocol's header.
- Drop `ResponsesConstructionAuth`; every adapter takes `(auth, authorizer)`,
  and the headers a request-time authorizer sets are protected from static
  configuration for every protocol, not only Codex.
- One error wording for protected auth headers.
- Reuse the per-attempt credential list when merging redactions instead of
  copying the static list first.
- Mantle Anthropic keeps `x-api-key`, unchanged.

Proof: existing adapter header/redaction tests unchanged in behavior;
`benches/provider_compiler.rs` reported, not gated.

## PR 3 — The built-in table owns provider policy

- One root `builtin_auth(provider)` replacing the `match provider.audience`
  blocks in `src/runtime.rs` and `src/catalog.rs` (authenticated check,
  profile scoping, `HttpAuth` choice, secure-store probe, body shape).
- The "authenticated" check calls the same resolver as requests, so an
  invalid stored xAI OAuth record is not reported as authenticated.
- Pass xAI's variable and login remedies from the table into `qq-auth`
  (ADR-0064 §6: no new dependency edge).
- Plan descriptor reports the explicit xAI reference when one is set.
- xtask canary endpoints and ids checked against the table by a test.
- Open question: resolve API keys per request (logout/rotation without a
  restart) instead of once at build time.

Proof: a test that fails today for the invalid-xAI-OAuth status; digest pins
(`builtin_provider_patches_keep_their_trust_digests`) unchanged.

## PR 4 — Discovery reuses request auth

- `src/catalog.rs` builds headers from `qq-provider`'s credential types
  (sensitive values, one Codex header set) instead of its own
  `DiscoveryAuth` header code.
- Key the catalog cache on credential identity (name + epoch), check it
  before resolving a secret, and lock per provider instead of one global
  lock held across the blocking fetch. Confirm no caller runs on a Tokio
  worker.

Proof: discovery tests plus a test that a cache hit performs no secret
resolution.

## PR 5 — `qq-auth` efficiency and hardening

- Per-request epoch check skips the lock and re-parse when the index file is
  unchanged.
- File locks take a deadline and fail with `RefreshLockUnavailable`.
- Validate the xAI device `user_code` like Codex and print the parsed
  verification URL.
- One generic refresh helper in `oauth.rs` for Codex and xAI; shared
  request-error mapping; device polls return `oauth::DeviceStep`.
- `request.rs` cache uses a std mutex; measure the 5-minute stored-resolution
  memo and delete it if OAuth's request cache makes it redundant.
- Codex loopback redirect uses `127.0.0.1`, matching the listener.
- Open question: `zeroize` for secrets (new dependency).

Proof: lock-timeout and terminal-escape tests; per-request credential
latency reported before/after.
