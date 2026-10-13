# API and billing

QQ's local runtime and Retsu's hosted extraction gateway have separate owners.
The current Retsu source pin for this page is
`8d58080829231bb74e180c6de4e8f687c4606200` (origin/main fetched 2026-10-09;
repository access required). The earlier `b889cd9e531438e4c54b25aa5ff9dc281c5e6888`
snapshot is historical context only; its cookie and account route details must
not be read as the current implementation.

## Retsu request path

The current Effect Worker in
[packages/api/src/index.ts](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/index.ts)
composes public, account, admin, billing, ledger, metrics, and monitoring
services. [http/public.ts](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/http/public.ts)
exposes `POST /v1/extract`, `GET /v1/models`, `GET /v1/balance`,
`GET /v1/health`, `GET /v1/status`, and `POST /v1/waitlist`. The Worker
adds a request id and calls the model backend through `Backend`.

[services/Extract.ts](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/services/Extract.ts)
rate-limits and validates a request, reserves an estimated amount for a live
key, calls the backend, settles exact input-token cost, or releases the hold
on failure. It records typed usage with request id, key id, status, model,
tokens, fields, cost, and latency.

The current Worker configuration points at a Modal Grip endpoint in
[packages/api/wrangler.jsonc](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/wrangler.jsonc)
and `worker-configuration.d.ts`. That is source configuration. It does not
prove that the endpoint is reachable, healthy, or serving the documented model.

## Identity, sessions, and keys

API-key authentication remains separate from browser identity:
[services/Auth.ts](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/services/Auth.ts)
hashes and resolves `retsu_sk_live_…` and `retsu_sk_test_…` bearer keys.
Revoked keys and suspended accounts fail before extraction.

Browser identity is now better-auth behind the typed Retsu routes. The
[Identity service](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/services/Identity.ts)
uses D1 auth tables, hashed magic links, Turnstile checks, and a host-only
Secure/HttpOnly/SameSite=Lax `__Host-retsu.session_token` cookie. The session
TTL is 30 days and refreshes daily while in use; magic links are one-use and
15 minutes. These are current source facts, not live authentication evidence.

[services/Sessions.ts](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/services/Sessions.ts)
maps better-auth users and sessions to billing accounts and supports current
principal checks, per-device session listing and revocation, email change,
account deletion confirmation, and bounded impersonation. Signup behavior is
selected by `signup_mode`: waitlist, invite-only, or open. The current
[account router](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/http/account.ts)
also exposes:

```text
POST /v1/auth/login
POST /v1/auth/verify
POST /v1/auth/invite
POST /v1/auth/signup
GET  /v1/auth/config
POST /v1/auth/logout
GET  /v1/me
POST /v1/me/warm
GET  /v1/me/sessions
POST /v1/me/sessions/revoke
POST /v1/me/email
POST /v1/auth/email/verify
POST /v1/me/delete
POST /v1/auth/delete/confirm
POST /v1/me/stop-impersonating
POST /v1/me/keys
POST /v1/me/keys/:id/revoke
GET  /v1/me/usage?days=30
GET  /v1/me/ledger
```

Admin bootstrap and dashboard operations are in
[http/admin.ts](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/http/admin.ts)
under `/admin` and `/v1/admin`. They cover account, invite, key, waitlist,
audit, settings, health, impersonation, and credit operations. Access and
privacy require the repository's admin controls; source routes do not prove
that an admin session or mail delivery works.

## Credits and Stripe

[services/Ledger.ts](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/services/Ledger.ts)
calls the per-account `AccountLedger` Durable Object for rate limits,
balance, holds, releases, settlements, and idempotent credits. D1 mirrors
ledger rows for account history. The Durable Object is the hot-path owner; D1
is the query and mirror store.

[services/Billing.ts](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/services/Billing.ts)
creates hosted Stripe Checkout sessions. Card data does not pass through the
Worker. The webhook verifies signature and timestamp, applies checkout credit
once, and handles asynchronous payment, cumulative refunds, and charge
disputes. A refund debits the proportional credit; an opened dispute removes
credit and suspends the account, while a won dispute restores credit pending
review. Unpaid, cancelled, stale, unknown, duplicate, signature-failed,
backend-failed, reserve-failed, release, and settlement outcomes remain
distinct.

The route surface includes:

```text
GET  /v1/billing/packs
POST /v1/billing/checkout {"pack":"starter"}
POST /v1/billing/webhook
```

When Stripe configuration is absent, the source returns the typed
`BillingNotConfigured` failure (HTTP 503) rather than claiming checkout is
ready. Source handling does not prove a live Stripe account, hosted checkout,
settled money, or customer acceptance.

## QQ usage metadata

QQ headless `outcome` records report token `usage` and
`estimated_cost_usd_nanos`. This is a local estimate, not the Retsu credit
ledger, and it charges no account. Unknown fields such as `billing` are
rejected. See [Headless](headless.md) for QQ's local contract and
[Providers](providers.md) for provider credentials.
