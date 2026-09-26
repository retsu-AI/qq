# Provider Architecture And Validation

## Purpose

Provider support is valid only when QQ can prove all of the following:

1. Configuration resolves to the intended deployment, protocol, endpoint, and
   authentication mode.
2. The emitted request matches the provider contract without leaking secrets.
3. Streaming responses produce the expected provider-neutral events under
   arbitrary network chunk boundaries.
4. Authentication, provider errors, cancellation, and resource limits fail in
   predictable ways.
5. A real provider accepts a minimal request using the credentials and model
   expected in production.

No single test layer proves all five properties. Default tests must remain
offline and deterministic, while opt-in live canaries detect upstream API,
credential, permission, and model availability changes.

## Public Seam And Module Ownership

`qq-provider` exposes a provider-neutral model and one construction facade. A
consumer creates a typed `ProviderRecipe`, passes it to `ProviderCompiler`, and
receives `Arc<dyn Provider>`. Concrete adapters and their constructors are not
public API.

```text
lib.rs                 public table of contents and Provider trait
model.rs               requests, messages, events, usage, and errors
compiler.rs            ProviderCompiler, recipes, HttpAuth, EndpointSpec
construction.rs        protocol/auth compatibility and adapter selection
http.rs                HTTP client, header safety, retry, bounds, redaction
exchange.rs            shared HTTP-to-SSE exchange driver
aws.rs                 AWS config, credential lease, SigV4, region rules
request_auth.rs        request-time bearer/Codex credentials and authorizer
providers.rs           private adapter module declarations
providers/
  support.rs           protocol-side error and accounting kit
  openai.rs            Responses codec, including Codex request shape
  openai_chat.rs       Chat Completions codec
  anthropic.rs         Messages codec
  google.rs            GenerateContent codec
  bedrock.rs           ConverseStream adapter
  mantle.rs            lazy Mantle deployment adapter
```

The root re-exports `BedrockAuth`, the recipe/compiler types, request-credential
types, structural secret types, the neutral model, and
`XAI_CREDENTIAL_ENDPOINT`. It does not expose adapter modules. The root package
is the composition layer that translates `qq-config` and `qq-auth` values into
provider recipes.

`construction.rs` is the only protocol/authentication compatibility authority.
`HttpAuth` is the public intent vocabulary. Construction resolves that intent
once into protocol-owned headers and an optional request authorizer. Mantle's
SigV4 authorizer is an internal field on `HttpConstructionSpec`, so public
recipes cannot manufacture Mantle-only capability. `EndpointKind` beside
`EndpointSpec` is the sole Base/Exact representation.

## Shared HTTP Execution And Adapter Kit

The four direct HTTP protocols share transport before their local decoders.
`HttpExchange` owns client policy, globally controlled-header safety,
request-time authorization, pre-stream retry/backoff, transport sanitization,
bounded non-2xx bodies, response metadata, and a wire-limited success stream.
`exchange.rs` owns the invariant request → execute → content-type gate → SSE
sequence. `limits.rs` owns stream budgets and checked byte counters.

`providers/support.rs` owns only behavior proven common across protocols:

- bounded rejection-envelope interpretation and shared status classification;
- `ToolCallLedger` attribution and reuse/unknown-call errors;
- `UsageOnce` and checked cached-input subtraction;
- test-only exact-endpoint client selection.

Prompt-cache breakpoints are a codec concern. The Anthropic Messages codec
marks three blocks `cache_control: ephemeral` (the system prompt, the last
tool declaration, the last block of the last message) so consecutive turns
extend one cached prefix; the Bedrock Converse codec places the equivalent
`cachePoint` blocks, but only for the Anthropic model family, because Converse
rejects the block for families without prompt caching. OpenAI and Google
cache the prefix implicitly. `ResolvedModel.prompt_cache.control` is `Native`
exactly for the two codecs that mark.

Each adapter still owns its wire request/response schemas, protocol headers,
error names, content-type exception, and streaming state machine. This is
deliberate composition, not a Template Method. A `ProtocolCodec` super-trait was
rejected because its hooks would expose every vendor difference and turn the
shared driver into a shallow generic-provider abstraction. Shared behavior must
continue to pass the deletion test: it replaces implementation in at least two
adapters.

The provider is the single retry owner. Each compiled HTTP provider carries one
public `AttemptPolicy` (default four attempts, 500 ms base, 8 s cap, 30 s
backoff budget, full jitter) set through
`ProviderCompiler::with_attempt_policy`. Every send a logical request costs
draws from one shared ledger: a transport error or retryable status (`408` /
`429` / `500` / `502` / `503` / `504` / `529`) before the body, and an SSE body
that fails or ends before it has decoded a single event. `Retry-After` is
honoured as a floor in both delta-seconds and HTTP-date form: the server's
figure is never jittered down and is slept in full above the 8 s exponential
cap, up to 60 s. Only the exponential part of a wait is charged to the 30 s
budget; the excess is time the provider asked for, and refusing it would only
earn the same rejection sooner. The budget bounds the sum
of backoff sleeps the ledger grants, not wall time since the first send: each
attempt is already bounded by the client's connect (30 s), header (300 s), and
read (300 s) deadlines, and an attempt that stalls for its whole header
deadline is exactly the one worth resending. Charging attempt time to the
budget would make any failure slower than 30 s unretryable, which is what a
gateway that loses a request looks like. Once one `ProviderEvent`
has been yielded the request is never resent, so a retry can never duplicate
output; a body that ends after events is the adapter's protocol error. Auth
and other client errors are never retried. When the policy is exhausted the
final error message records the attempts spent. Nothing above the provider —
not the core run loop, not the session layer — retries a turn. Operational
probes use `ProviderCompiler::compile_for_canary`, which disables direct HTTP
and Mantle adapter retries through the facade. Bedrock's AWS SDK client already
has SDK retries disabled.

The header deadline is not a separate setting. reqwest arms its `read_timeout`
when the request is sent and resets it only on a body read, so `READ_TIMEOUT`
also bounds the wait for response headers; a provider that accepts a request
and never answers fails after 300 s with
`timed out waiting for response headers (none within 300s)` and is retried.

Transport failures render as `provider request failed: <phase>: <causes>`.
`http.rs` names the phase itself (connection, response headers, response body,
redirect) and walks the `reqwest::Error` source chain, because reqwest's
`Display` prints only its kind label and files every body read failure —
peer reset, truncated framing, idle timeout — under one kind that reads
"error decoding response body". A timeout names which deadline expired
(connect, headers, or the body's read/total timeout with the configured
seconds). The URL is never included; credentials can appear in it and the
caller already names the provider.

Header consolidation follows the same boundary. `http.rs` defines universal
request-controlled names and parses names and values, marks sensitive values,
rejects case-insensitive duplicates and reserved-name overrides, and produces
normalized redactions. Adapters add their small protocol-owned set. Secrets use
`SecretLiteral` rather than debug-visible strings.

## Interface Test Contract

Composition tests under `crates/qq-provider/tests/interface/` enter only through
`ProviderCompiler` and `Provider::stream`. Every direct HTTP protocol has
auth-failure and output-limit coverage; compiler tests cover successful
compile-to-stream request capture. A separate interface test proves
`compile_for_canary` surfaces the first retryable response for all four HTTP
protocols. Adapter-local tests remain appropriate for decoding tables,
fragmentation, protocol state, and owned headers.

All socket tests use `test_support::LoopbackServer`; private one-off HTTP
harnesses are not allowed. This keeps file moves and internal refactors guarded
by the same public behavior rather than by tests coupled to concrete adapters.

## Validation Matrix

Every supported deployment and authentication path must appear in one checked-in
matrix. A row is incomplete until it has deterministic contract coverage and an
assigned live-validation cadence.

| Deployment | Protocol | Authentication paths | Offline gate | Live gate |
| --- | --- | --- | --- | --- |
| OpenAI | Responses | bearer API key | every PR | nightly |
| OpenAI Codex | Responses | OAuth access token, account headers | every PR | manual and release |
| Anthropic | Messages | `x-api-key` | every PR | nightly |
| Google Gemini | GenerateContent | `x-goog-api-key` | every PR | nightly |
| xAI | Responses | request-time bearer resolved by `qq-auth` | every PR | nightly |
| xAI | Chat Completions | request-time bearer resolved by `qq-auth` | every PR | nightly |
| Amazon Bedrock | ConverseStream | Bedrock API key, default AWS chain, named profile | every PR | nightly and release |
| Bedrock Mantle | Responses | API key, SigV4 | every PR | nightly and release |
| Bedrock Mantle | Chat Completions | API key, SigV4 | every PR | nightly and release |
| Bedrock Mantle | Anthropic Messages | API key, SigV4 | every PR | nightly and release |
| LiteLLM/custom | Configured HTTP protocol | configured bearer, key, header, or no auth | every PR | deployment-owned |

The future model registry may let users select only a model ID, but validation
must continue to record the resolved deployment. Tests must fail if a registry
change silently routes a model through a different provider or authentication
path.

## Test Layers

### 1. Configuration And Compilation

These tests run without sockets or credentials. For every matrix row, assert:

- Layered configuration produces the intended typed provider recipe.
- Model selection resolves to the expected deployment and provider model ID.
- Base endpoints append only the protocol-owned path.
- Exact endpoints are not rewritten.
- Region, profile, endpoint, header, and authentication values are validated.
- Unsupported protocol/authentication combinations fail before network access.
- Provider cache identity includes every value that changes request behavior.
- Debug and error formatting never expose credentials.

Run these tests whenever configuration, the model registry, provider recipes,
credential resolution, or provider compilation changes.

### 2. Deterministic Wire Contracts

Each protocol codec uses a localhost server or an SDK replay transport. The test
captures the request and returns controlled response frames. It must verify:

- HTTP method, URL, model path, content type, and streaming negotiation.
- Protocol-specific authentication headers and absence of credentials in URLs.
- Message roles, text, system instructions, and output-token limits.
- Success events, usage, terminal completion, and legal empty deltas.
- Frames split at every meaningful byte boundary, including UTF-8 boundaries.
- Multiple events delivered in one network chunk.
- Provider-declared errors in both HTTP bodies and stream events.
- Premature EOF, malformed frames, unknown events, and non-streaming responses.
- Response, event, and accumulated-output limits.
- Cancellation while connecting, reading, decoding, and waiting for credentials.
- Error classification and redaction of request and response secrets.

Fixtures should be minimal protocol examples rather than recordings of complete
production responses. Sanitized provider fixtures may supplement generated edge
cases, but they must contain no account IDs, request IDs, credentials, prompts,
or model output copied from private traffic.

Amazon Bedrock should use the AWS SDK replay/test transport where possible so
ConverseStream request construction and event-stream decoding remain
deterministic. Test-only transport injection must not become a production
endpoint override.

### 3. Runtime Composition

Composition tests exercise the same path as `qq ask` with a local fake provider.
They prove that configuration, credential lookup, compilation, `qq-core`, and
event rendering agree. At minimum, cover:

- One successful stream for every protocol.
- A provider authentication failure.
- A provider rate-limit or availability failure.
- Cancellation and bounded output.
- Stored, environment, and OAuth credential selection without real secrets.
- Model-registry resolution to the expected provider recipe.

These tests must use isolated config, trust, credential, and data directories.
They must never depend on a developer's `.qq/config.ron`, environment, keyring,
or plaintext credential store.

### 4. Credentialed Live Canaries

Live checks are explicit, bounded, and excluded from normal `cargo test`:

```sh
cargo xtask providers check offline
QQ_LIVE_PROVIDER_TESTS=1 cargo xtask providers check live --provider google
QQ_LIVE_PROVIDER_TESTS=1 cargo xtask providers check live --all
```

The executable, nonsecret matrix lives in `xtask/src/providers.rs`. The live
runner constructs recipes directly rather than loading project configuration.
`--provider` may be repeated and accepts `openai`, `openai-codex`, `anthropic`,
`google`, `xai`, `amazon-bedrock`, and `bedrock-mantle`. `--all` and
`--provider` are mutually exclusive.

API-key rows read only their documented provider environment variables. xAI
and OpenAI Codex use `CredentialStore` request-time providers so OAuth refresh
and `qq-auth` remain inside the tested path. The executable AWS rows cover the
default chain, a named profile from `QQ_CANARY_AWS_PROFILE`, a Bedrock key from
`QQ_CANARY_BEDROCK_API_KEY`, and Mantle keys from
`QQ_CANARY_MANTLE_API_KEY`; `QQ_CANARY_AWS_REGION` supplies an explicit region
when desired. Each checked-in model has a `QQ_CANARY_*_MODEL` override for
controlled model migrations. LiteLLM/custom remains deployment-owned because
QQ has no nonsecret endpoint or model it can probe centrally.

Each live case must:

1. Use a pinned canary model known to support the tested protocol.
2. Send only `Reply only with QQ_PROVIDER_SMOKE_OK`.
3. Request no more than 32 output tokens and perform no tool calls.
4. Require at least one text event and exactly one successful terminal event.
5. Require the marker, while accepting harmless prose around it.
6. Compile with `ProviderCompiler::compile_for_canary` so pre-stream retries are
   disabled without exposing a concrete adapter.
7. Enforce provider connection/request timeouts plus a 20-second first-token and
   45-second total runner deadline, in addition to event and output limits.
8. Emit only redacted metadata.

The runner validates one pinned model per executable matrix row. QQ currently
has no global product-default model: configuration requires an explicit model
route. If a future model registry introduces a product default, add it as a
second canary whenever it differs from the pinned connectivity model.

### 5. Differential Diagnosis

Live failures are ambiguous because credentials expire, permissions change, and
providers have outages. Nightly and release automation should retain the last
green QQ binary and rerun the same canary with the same model and credential:

| Current binary | Last green binary | Interpretation |
| --- | --- | --- |
| pass | pass | healthy |
| fail | pass | probable QQ regression |
| fail | fail | credential, account, model, or provider failure |
| pass | fail | upstream recovery or baseline incompatibility |

The baseline is a diagnostic, not a release gate by itself. If a provider makes
an intentional breaking API change, both binaries may fail while current source
still needs an update.

## Live Credential Policy

- Live tests require the exact explicit opt-in `QQ_LIVE_PROVIDER_TESTS=1`.
- A selected row with no credential emits a redacted `skip` record and makes the
  command exit nonzero; this includes typed AWS default-chain load failures, so
  unavailable credentials never become a silent pass.
- Use dedicated low-quota test projects and accounts, never personal production
  credentials.
- CI should use workload identity or OIDC and short-lived credentials where the
  provider supports them.
- Bedrock short-term API keys expire with their AWS session and last at most 12
  hours. Prefer an OIDC-assumed AWS role and SigV4 for unattended checks.
- Bedrock API-key checks remain necessary because bearer authentication is a
  separate supported path. Refresh those keys immediately before their gated
  run or use a deliberately bounded long-term test key.
- OpenAI Codex OAuth uses an interactive subscription identity. Keep its live
  check manual or on an explicitly approved secure runner; do not bypass OAuth
  or place a personal refresh token in general CI.
- OpenAI Codex login keeps the loopback browser flow as the default. The
  explicit `--device-auth` path uses the provider's device endpoints, validates
  the returned PKCE proof, exchanges the authorization code once, and then
  writes the same versioned credential through the existing Codex lock, epoch,
  protected-storage, and refresh path. The device exchange does not create a
  portable credential format or transfer a credential between hosts.
- An unattended job needs a durable protected store that its service identity
  can unlock. A successful device prompt alone establishes neither that storage
  nor live provider support for a particular headless host.
- Never print, serialize into artifacts, or include credentials in command-line
  arguments. Mark authorization headers sensitive.
- Do not log full model responses. The smoke marker, event counts, byte counts,
  and timings are sufficient.

Credential metadata proves only that a credential is stored. A live request is
the validity check for expiration, revocation, endpoint scope, and permissions.

## Result Records

Each live result is one JSON line containing:

- Unix timestamp and QQ commit.
- Deployment, protocol, authentication mode, region, and model.
- Pass, fail, skip, or infrastructure-error outcome.
- Provider error category on failure.
- Time to first token, total time, event count, and output byte count.

Future differential automation may add the last-green baseline result to this
record without adding prompts, generated text, or raw provider errors.

Do not store prompts, generated text, headers, URLs containing query secrets, or
raw error bodies. Retain enough history to distinguish a one-off outage from a
regression trend.

## Required Cadence

| Trigger | Required validation |
| --- | --- |
| Every local provider change | affected package tests and affected contract matrix rows |
| Every PR | full offline workspace and provider matrix |
| Provider codec or auth change | affected live provider before merge |
| Provider SDK or HTTP dependency update | all offline contracts and affected live providers |
| Model registry update | resolver tests and live checks for changed defaults |
| Nightly | all unattended live canaries plus last-green differential on failure |
| Release candidate | every matrix row, including approved manual OAuth checks |

Live checks should report `skip` with a reason when credentials are unavailable;
they must never silently pass. Required release rows cannot remain skipped.

## Failure Triage

Classify before changing code:

1. Confirm the effective deployment, protocol, model, region, and auth mode.
2. Reproduce with the smallest live canary, not a full agent session.
3. Run the same credential and model through the last green binary.
4. Use status and provider request IDs to classify the failure using the table
   below.
5. Reproduce the failure with a sanitized local fixture before fixing code.
6. Add the fixture as a regression test, apply the fix, and rerun offline, live,
   and differential checks.

| Signal | Likely class |
| --- | --- |
| `401` or authentication `403` | expired, revoked, malformed, or wrong-scope credential |
| authorization `403` | missing provider or model permission |
| `404` | endpoint, region, protocol path, or model availability |
| `429` | quota or rate limiting |
| successful HTTP with decoder failure | protocol drift or framing regression |

Do not infer a QQ regression solely from timing. In the Bedrock API-key failure
investigated on 2026-07-22, both current source and the pre-provider-work binary
received the same authentication `403`; the stored provider credential, not the
provider changes, was the failing variable.

## Current Coverage And Gaps

### Reasoning effort controls

The provider-neutral request may carry an optional `ReasoningEffort`. The OpenAI
Responses codec serializes it as `reasoning.effort`; the OpenAI-compatible Chat
Completions codec serializes it as `reasoning_effort`. When absent, both codecs
omit the field, preserving their prior request shape. The enum values currently
supported by these API contracts are `none`, `minimal`, `low`, `medium`,
`high`, and `xhigh`; model-specific availability is not inferred by QQ and must
be established by the runtime capability/catalog layer before selection.

Reference: [OpenAI Responses API reference](https://platform.openai.com/docs/api-reference/responses)
(reasoning.effort) and [OpenAI Chat Completions API reference](https://platform.openai.com/docs/api-reference/chat)
(reasoning_effort), consulted 2026-09-18. This is request encoding only; model
selection, capability authorization, JEV routing, and durable receipts remain
runtime work.

Offline interface tests drive all six values through
`ProviderCompiler::compile` and `Provider::stream` for standard Responses,
Codex Responses, and Chat Completions. They capture both attempts of a
retryable request, pin the byte-exact request body when effort is absent, and
invoke every unsupported adapter to require a configuration error before HTTP,
request-time authorization, or lazy AWS provider initialization.

Current strengths:

- OpenAI Responses, OpenAI Chat Completions, Anthropic Messages, and Google
  GenerateContent have localhost request and stream contract tests.
- Interface tests enter through `ProviderCompiler` and prove auth failures,
  bounds, and single-attempt canary compilation for every HTTP protocol.
- Mantle tests cover protocol-specific API-key headers and SigV4 signing.
- Provider compilation and runtime construction have deterministic tests.
- Stream bounds, malformed input, terminal behavior, and secret redaction have
  focused coverage.
- `cargo xtask providers check offline|live` provides an executable matrix and
  bounded redacted live probes, including both xAI protocols through `qq-auth`
  and every centrally managed Bedrock authentication path.

Current gaps:

- There is no CI workflow or scheduled live canary.
- Live results and last-green binaries are not retained for comparison.
- QQ has no product-default model to compare against the pinned connectivity
  model; a future registry/default must add that second canary explicitly.
- Connection timing and sanitized provider request IDs are not exposed by the
  neutral provider interface, so result records begin at first token.
- Bedrock SDK request/replay coverage is less complete than the HTTP codecs.
- Codex browser and device OAuth have deterministic login tests but no approved
  live release check. The device protocol implementation is therefore separate
  from provider-support and live-headless qualification.
- Model defaults and provider resolution are not yet owned by a model registry.

## Completion Criteria

A provider feature is complete only when its matrix rows exist, offline tests
pass, the required live check passes, and failures remain diagnosable without
exposing credentials or private model traffic.

Automatic Jev effort selection additionally requires explicit model metadata
`reasoning_efforts: [low, medium, high]` (use only values the remote model accepts).
An empty or absent declaration means unknown support and preserves omission.
This declaration does not enable routing and does not claim remote validation;
it constrains the bounded candidate list used after independent trusted opt-in.
