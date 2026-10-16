# Auth and provider credentials

Status: active. Linear [ENG-1066](https://linear.app/retsu-ai/issue/ENG-1066).
Ledger: [progress/auth-providers.md](progress/auth-providers.md).

## Outcome

Adding a built-in provider means adding one row of static data: identity,
credential audience, environment variables, login methods and stored kind.
It should not mean adding another lock file, store field, error enum, token
client, polling loop, request-credential provider or CLI branch. This plan
first removes the per-provider duplication in `qq-auth` without changing
behaviour, then makes request-time credentials provider-neutral in
`qq-provider`. New vendors land after that, as presets.

## Current shape (2026-10-16, `origin/main` `3e34eea2`)

- `CredentialStore` holds per-provider token clients and refresh mutexes;
  `CredentialPaths` has a lock file per OAuth provider; `set` and `remove`
  take every provider lock, selected by credential-name prefix.
- `codex.rs` and `xai.rs` each define their own clock, credential name,
  bounded body decoding, stored-credential versioning, double-checked
  refresh, request-credential provider and error mapping.
- Credential audiences are repeated across `qq-auth`, `qq-provider`
  (`XAI_CREDENTIAL_ENDPOINT`), `qq-config`, `src/main.rs` and `src/runtime.rs`.
- `qq-provider` request credentials are `Bearer | Codex`. Codex auth also
  implies the Codex request body. Google rejects request-time credentials.

## Vendor constraints (fetched 2026-10-16)

Third-party use of the subscription logins of Anthropic (Claude.ai), Google
(Gemini CLI / Code Assist) and Meta (Muse Code) is prohibited by their
published terms. Supporting those logins is out of scope unless a written
vendor agreement exists. All of these vendors support API keys. Z.ai's GLM
Coding Plan and Alibaba's Coding/Token Plans are API keys on dedicated
endpoints. Z.ai restricts its plan to listed tools. Qwen's OAuth free tier
was discontinued on 2026-04-15. Kimi Code uses a device-code login whose
endpoints and terms are not yet verified.

## Slices

Phase A changes no behaviour: no CLI, config, stored-credential format, lock
file name, error text or wire change. Each slice is one PR stacked on the
previous one and follows the refactoring playbook. Proof that behaviour is
unchanged: the same tests pass, `cargo xtask smoke` passes, and
`crates/qq-protocol/tests/fixtures` is unchanged.

| ID | Goal | Owned paths |
| --- | --- | --- |
| AP0 | This plan and ledger | `docs/plans/auth-providers.md`, `docs/plans/progress/auth-providers.md`, plan/progress indexes |
| AP1 | Shared OAuth helpers: clock, profile credential names, bounded JSON decode, HTTPS URL check | `crates/qq-auth/src/{lib,oauth,codex,xai}.rs` |
| AP2 | One provider refresh lock: an `OAuthProvider` table owns name prefix and lock file; `set`/`remove`/refresh use it; existing lock file names kept | `crates/qq-auth/src/{lib,oauth,codex,xai,tests}.rs` |
| AP3 | Provider audiences defined once: `XAI_CREDENTIAL_ENDPOINT` leaves `qq-provider`; runtime, doctor and CLI read the shared constants instead of literals | `crates/qq-provider/src/lib.rs`, `crates/qq-config/src/{lib,providers}.rs`, `src/{main,runtime,doctor}.rs` |
| AP4 | Credential store split out of `lib.rs` into `store.rs` (move only) | `crates/qq-auth/src/` |

Phase B changes internal APIs and may change narrow behaviour. Each slice
records any visible change in its PR.

| ID | Goal |
| --- | --- |
| AP5 | One request-time credential provider with the epoch and expiry cache for every OAuth provider (xAI gains Codex's cache; measure TTFT before and after) |
| AP6 | `qq-provider` request credentials become a sensitive header set; the request body shape is an explicit recipe field instead of something implied by `Codex` auth; Google accepts request-time credentials |
| AP7 | Built-in credentials collapse into one `HttpCredential::Builtin` driven by a provider table; `LOGIN_PROVIDERS`, `built_in_endpoint`, `qq init` choices and `provider_credential_variables` derive from it (ADR) |

Phase C adds API-key presets: Meta Model API, DeepSeek, Z.ai, Moonshot and
Alibaba Model Studio, with region and plan endpoints, catalogs, docs and
`xtask providers` canary rows. Phase D adds OAuth grants only for vendors
whose terms permit them (Kimi Code once verified; Vertex through the user's
gcloud login).

## Acceptance (whole plan)

- Adding an API-key preset touches only the provider table, the model
  catalog, docs and the canary matrix.
- Stored OpenAI Codex and xAI credentials (version 1) keep loading and
  refreshing across the change.
- Request-time credential resolution time for xAI does not regress
  (`cargo xtask perf` first-token gate).
