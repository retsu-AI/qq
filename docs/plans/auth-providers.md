# Auth and provider credentials

Status: active. Linear [ENG-1066](https://linear.app/retsu-ai/issue/ENG-1066).
Ledger: [progress/auth-providers.md](progress/auth-providers.md).

## Outcome

Adding a built-in API-key provider means adding one row of static data:
identity, credential audience, environment variables and stored kind.
Adding an OAuth provider adds a row (identity, login methods, stored kind)
plus its `qq-auth` endpoint specification and response mapping, which own
its audience and protocol data.
Neither should mean adding another lock file, store field, error enum, token
client, polling loop, request-credential provider or CLI branch. AP1–AP4
(shipped) shared the OAuth HTTP and bookkeeping helpers, put operation locks
behind one provider table, moved audiences into `qq-auth` constants and split
the store into modules, without changing behaviour. The per-provider token
clients, stored formats, request-credential providers and error mappings
remain. AP5–AP7 make request-time credentials and
built-in configuration provider-neutral, and AP8 makes the OAuth grants
provider-neutral. New vendors land after that, as presets.

## Shape before AP1 (2026-10-16, `origin/main` `3e34eea2`)

What AP1–AP4 changed: the clock, credential-name, HTTP-client, bounded-body
and HTTPS-URL helpers live in `qq-auth/src/oauth.rs`; one `OAuthProvider`
table there names each provider and its lock file, and `CredentialStore`
(`lib.rs`) takes one lock path for all of them; audiences are `qq-auth`
constants; and the store is split into `lib.rs`, `state.rs` and `backend.rs`.
Still as below: per-provider token clients and stored formats (AP8),
request-credential providers and error mapping (AP5), and the provider
request credentials (AP6).

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

These decide which subscription logins are in scope. Terms change, so
re-fetch the sources before starting Phase D.

| Vendor | Finding | Source |
| --- | --- | --- |
| Anthropic | "Anthropic does not permit third-party developers to offer Claude.ai login into their own applications, or to route requests through Free, Pro, or Max plan credentials"; developers "may not collect, store, or intermediate Claude.ai credentials" | <https://code.claude.com/docs/en/legal-and-compliance>, § Authentication and credential use |
| Google | "Directly accessing the services powering Gemini CLI (for example, the Gemini Code Assist service) using third-party software ... is a violation of applicable terms" | <https://github.com/google-gemini/gemini-cli/blob/main/docs/resources/tos-privacy.md>, opening section |
| Meta | The Muse Code subscription credential "is for use with Muse Code only"; the subscription "works only through the Muse Code CLI". API keys are Bearer on `https://api.meta.ai/v1` | <https://dev.meta.ai/docs/muse-code/subscriptions>, § About subscriptions; <https://dev.meta.ai/docs/authentication> |
| Z.ai | GLM Coding Plan is an API key on dedicated endpoints and "is limited to use within the following officially supported tools" | <https://docs.z.ai/devpack/tool/others>, § Supported Tools |
| Alibaba / Qwen | Coding Plan (`sk-sp-` keys) and Token Plan are API keys on dedicated endpoints. Coding Plan "is for interactive use in programming tools ... Do not use the plan's API key for automated scripts, application backends, or other non-interactive scenarios". "The Qwen OAuth free tier was discontinued on 2026-04-15" | <https://www.alibabacloud.com/help/en/model-studio/coding-plan>, § Before you subscribe; <https://qwenlm.github.io/qwen-code-docs/en/users/configuration/auth/>, Options 1–3 |
| DeepSeek | API key only; OpenAI and Anthropic formats | <https://api-docs.deepseek.com/> |
| Moonshot | Kimi Code CLI offers a device-code `/login`; endpoints, client and terms unverified | <https://moonshotai.github.io/kimi-code/en/guides/getting-started.md>, § First launch |

Supporting the Anthropic, Google or Meta subscription logins is out of
scope unless a written vendor agreement exists. All seven vendors support
API keys.

## Slices

Phase A changes no behaviour: no CLI, config, stored-credential format, lock
file name, error text or wire change. Each slice is one PR stacked on the
previous one and follows the refactoring playbook. Proof that behaviour is
unchanged: the same tests pass, `cargo xtask smoke` passes, and
`crates/qq-protocol/tests/fixtures` is unchanged. AP4b adds benchmarks and budget rows only.

| ID | Goal | Owned paths | Acceptance beyond sameness |
| --- | --- | --- | --- |
| AP0 | This plan and ledger | `docs/plans/auth-providers.md`, `docs/plans/progress/auth-providers.md`, plan/progress indexes | |
| AP1 | Shared OAuth helpers: clock, profile credential names, OAuth HTTP client, bounded JSON decode, HTTPS URL check | `crates/qq-auth/src/{lib,oauth,codex,xai}.rs` | |
| AP2 | One provider operation lock: an `OAuthProvider` table owns the name prefix and lock file; `set`, `remove` and refresh use it; existing lock file names kept | `crates/qq-auth/src/{lib,oauth,codex,xai,tests}.rs` | A test pins both lock file names (`openai-codex.lock`, `xai.lock`) and shows that, while an xAI refresh holds `xai.lock`, a second store's `set` of that credential over the same directory waits for it |
| AP3 | Provider audiences defined once: `XAI_CREDENTIAL_ENDPOINT` moves from `qq-provider` to `qq-auth`, which already owns the Codex audience. `qq-config` stops re-exporting it (it only re-exports it today). Root code (`src/runtime.rs`, `src/doctor.rs`, `src/main.rs`) uses the `qq-auth` audience constants instead of `"https://chatgpt.com"` and `"https://api.x.ai"` literals. The `"openai-codex/{}"` and `"xai/{}"` stored-name literals in root code stay until AP7 derives them from the provider table (a fallible shared helper would add error paths those sites do not have). `docs/design/providers.md` stops listing the constant among `qq-provider`'s exports | `crates/qq-auth/src/{lib,codex,xai}.rs`, `crates/qq-provider/src/lib.rs`, `crates/qq-config/src/lib.rs`, `src/{main,runtime,doctor}.rs`, `docs/design/providers.md` | No new crate dependency edge; `qq-provider` names no vendor audience |
| AP4 | Credential state and backends split out of `lib.rs` (move only): index and fallback formats plus hardened file operations into `state.rs`, OS keyring and Windows-protected backends into `backend.rs` | `crates/qq-auth/src/` | |
| AP4b | Perf fixtures for the request-credential path, landed before any behaviour change so AP5 and AP6 record their baseline and candidate with the same metric inventory and fixture version (`validate_metric_contracts` in `xtask/src/perf.rs` rejects differing inventories). Two deterministic metrics: (1) resolving request credentials for a stored, unexpired xAI OAuth credential through `CredentialStore` with fake keyring and token clients, sequentially and with several concurrent requests on one provider; (2) one HTTP attempt's request-time authorization (credential fetch, header validation and application, redaction collection) against a loopback server. Budget rows in `benchmarks/perf/budgets-v1.json` with a `fixture_version` bump | `xtask/src/perf.rs`, `benchmarks/perf/budgets-v1.json`, bench support in `crates/qq-auth`/`crates/qq-provider` | Receipts recorded on the AP4b head are the baseline for AP5 and AP6 |

Phase B changes internal APIs and may change narrow behaviour. Each slice
records any visible change in its PR.

| ID | Goal | Acceptance |
| --- | --- | --- |
| AP5 | One request-time credential provider with the epoch and expiry cache for every OAuth provider (xAI gains Codex's cache) | The AP4b credential-resolution metric is recorded on the AP4b head and on the AP5 head and must not regress beyond its budget. AP4b also records a parallel warm-cache variant (several concurrent requests on one compiled provider): warm resolutions must not serialise behind the cache lock, unlike Codex's current design, which holds its async cache mutex across the awaited epoch check; only an expired-token refresh is single-flight, and the existing single-flight refresh assertion stays. The existing first-token metrics bypass `qq-auth`, so they cannot gate this. Before the cache is accepted, xAI gets the invalidation regressions Codex already has: a `set` rotation and a `remove` in the same store and in an independent store over the same directory are each observed on the next request (epoch invalidation), and a cached token past its refresh time is rechecked and refreshed rather than served |
| AP6 | `qq-provider` request credentials become a sensitive header set; the request body shape is an explicit recipe field instead of something implied by `Codex` auth; Google accepts request-time credentials | Every *secret* dynamic header value is redacted on success, from transport errors, rejection bodies, body-read failures and retried attempts, tested per value and not only the first. The header set distinguishes secret values (tokens, API keys, account IDs) from non-secret credential metadata such as `x-openai-fedramp: true`; metadata is sent but never registered for redaction, as today (`request_auth.rs`), so diagnostics containing `true` are not corrupted. A test pins this. The header-set type is bounded (maximum header count and total bytes) and validated against the selected recipe before any header is applied. The adapters' `SafeHeaders` protocol-owned lists mix two kinds of names, so AP6 separates them. *Credential-owned* names are the authentication headers a protocol accepts (`Authorization` everywhere, `x-api-key` for Anthropic, `x-goog-api-key` for Google, and for Codex `chatgpt-account-id` and `x-openai-fedramp`). A request-time credential may set exactly those its recipe's auth intent allows; a Codex credential sends `x-openai-fedramp: true` only when its stored flag is set, and a stale value is removed when it is not, as today (`request_auth.rs`), with a request-time transport regression for both flag values. *Immutable* names are never settable by a credential: `http::is_request_controlled_header`, protocol constants such as `anthropic-version`, and names that duplicate a configured static header. Tests cover each class, including that bearer OAuth (xAI, Codex) and request-time API keys still apply. Every consumer that currently reads body shape from `HttpCredential::OpenAiCodex` switches to the explicit field: `provider_request_shape_identity` and the `output_token_control` capability derivation in `src/runtime.rs`, which `qq-core` includes in its context shape. Regressions cover both independent combinations (Codex body with non-Codex auth, standard body with Codex-style headers). The AP4b request-time authorization metric is recorded on the AP4b head and on the AP6 head and must not regress beyond its budget. `provider_request_shape_identity` (`src/runtime.rs`) digests the explicit body-shape field so recipes with different wire bodies never share an identity. If the emitted digests change, bump `ProviderRequestShapeVersion` and regenerate the affected fixtures under the protocol-bump skill |
| AP7 | Built-in providers driven by one table: `HttpCredential::{ApiKey, OpenAiCodex, XAi}` collapse into `HttpCredential::Builtin`; `ProviderKind` built-ins, `ProviderEntryPatch` built-in variants and their exhaustive matches in `qq-config` become table lookups; `LOGIN_PROVIDERS`, `built_in_endpoint`, `qq init` choices and `provider_credential_variables` derive from it. Each row lists its login methods (API key, device grant, loopback grant); `validate_login_mode` and `auth_command` dispatch on those methods, so a new provider adds no provider-name branch in `src/main.rs`. **Dependency owner:** the table lives in `qq-config`, which already owns provider presets, and carries data only (environment variables, stored kind, login methods; and the audience for API-key-only providers). An OAuth provider's row carries no audience; it names its login methods by provider name, and the root maps that name to `qq-auth`'s `OAuthProvider` (exhaustively, so an unmapped OAuth row fails to compile or fails a table test). `qq-auth` stays the only place an OAuth audience is written (AP3). The root resolves a row's audience and grant through `qq-auth` when composing. No crate gains a dependency edge, so no `Cargo.toml` changes | ADR (number reserved in `progress/root.md` at slice start). CLI tests cover login-mode validation and dispatch for every row. Config documents for every existing built-in parse to the same `ProviderConfig` as before, tested per provider. Each row declares the patch fields it accepts besides `models` (today `api_key` for OpenAI, Anthropic and Google; `profile` for OpenAI Codex; both for xAI), and unknown or disallowed fields still fail to parse as under today's `deny_unknown_fields`, with a negative test per row. The sensitive-section trust digest (`Document::sensitive_digest`, which serialises the provider patches) stays byte-identical for every existing built-in patch shape, so a project config the user already trusted is not withheld after the upgrade; digest fixtures captured before AP7 pin this per built-in. A deliberate digest change would instead need a documented trust-record migration in the ADR. Owned paths: `crates/qq-config/src/`, `src/{main,init,runtime,catalog,doctor}.rs`, `crates/qq-auth/src/`, `crates/qq-harness/src/plan.rs` (its exhaustive `HttpCredential` match in the compiled-plan size estimate, with that estimate's test) |
| AP8 | Provider-neutral OAuth grants: one token-exchange and refresh contract and one RFC 8628 device-poll loop with injectable clock and cancellation, replacing the separate `CodexTokenClient`, `CodexDeviceClient` and `XaiTokenClient` traits and the per-provider client fields on `CredentialStore`. Codex and xAI implement only their endpoint specs and response mapping. The loopback PKCE callback stays Codex-local until a second provider needs it; Codex's non-standard device flow stays a Codex grant that reuses the shared exchange | Codex and xAI login and refresh behaviour is unchanged: every existing test keeps its scenario and assertions, while its fakes migrate to the shared contract. The shared poll loop has its own tests for pending, slow-down, denial, expiry, timeout and cancellation. The operation lock file for a provider is derived as `{provider}.lock` in the data directory, which reproduces both legacy names (`openai-codex.lock`, `xai.lock`), so a new OAuth provider gets interprocess exclusion without a new lock definition; a test adds a fixture provider with no legacy file and shows cross-store serialisation through its derived lock. Required before Phase D |

Phase C adds pay-as-you-go API-key presets: Meta Model API, DeepSeek, Z.ai,
Moonshot and Alibaba Model Studio (including the Token Plan endpoints), with
region endpoints, catalogs, docs and `xtask providers` canary rows. Two
subscription plans are excluded because of their terms (sources above):
Z.ai's GLM Coding Plan, which is limited to listed tools that do not include
QQ, and Alibaba's Coding Plan, which prohibits non-interactive use that QQ's
`ask`, `run` and server surfaces would allow. Neither is offered as a preset
until the vendor authorises QQ or QQ can enforce the restriction.

Phase D (after AP8) adds OAuth grants only for vendors whose terms permit
them: Kimi Code once its endpoints and terms are verified. Vertex is not
part of Phase D. Google Application Default Credentials is a separate
credential source with its own discovery order (`GOOGLE_APPLICATION_CREDENTIALS`,
the `gcloud auth application-default login` file, the metadata server),
non-user credential types, and project/location configuration
(<https://docs.cloud.google.com/docs/authentication/application-default-credentials>).
It needs its own plan if pursued.

## Acceptance (whole plan)

- After AP7, adding an API-key preset touches only the provider table, the
  model catalog, docs and the canary matrix.
- After AP8, adding an OAuth provider that uses a shared grant adds an
  endpoint spec, response mapping and table row, not a token client, polling
  loop, lock, error enum or CLI branch.
- Stored OpenAI Codex and xAI credentials (version 1) keep loading and
  refreshing across the change.
- xAI request-credential resolution and request-time authorization do not
  regress on the AP4b metrics.
