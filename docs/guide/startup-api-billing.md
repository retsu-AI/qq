# API and billing

QQ's local runtime and Retsu's hosted extraction gateway have separate owners.
The Retsu source snapshot is private and requires repository access.

## Retsu request path

The Effect Worker in [`packages/api/src/index.ts`](https://github.com/retsu-AI/retsu/blob/b889cd9e531438e4c54b25aa5ff9dc281c5e6888/packages/api/src/index.ts)
composes the gateway services and routes. `http/public.ts` exposes
`POST /v1/extract`, `GET /v1/models`, `GET /v1/balance`, `GET /v1/health`, and
`POST /v1/waitlist`. The Worker adds a request id and calls the model backend
through the `Backend` service.

`services/Extract.ts` implements the billed path. It rate-limits, validates the
request, reserves an estimated amount for a live key, calls the backend, then
settles the exact input-token cost or releases the hold on failure. It records
usage asynchronously with request id, key id, status, model, tokens, fields,
cost, and latency.

The current Worker configuration points at a Modal Grip endpoint in
`packages/api/wrangler.jsonc` and `worker-configuration.d.ts`. That is source
configuration. It does not prove that the endpoint is reachable, healthy, or
serving the documented model.

## Accounts, sessions, and keys

`services/Auth.ts` hashes and resolves `retsu_sk_live_…` and `retsu_sk_test_…`
Bearer keys against the account and API-key tables. Revoked keys and suspended
accounts fail before extraction. `http/account.ts` exposes the dashboard
surface:

```text
POST /v1/auth/login
POST /v1/auth/verify
POST /v1/auth/invite
POST /v1/auth/logout
GET  /v1/me
POST /v1/me/keys
POST /v1/me/keys/:id/revoke
GET  /v1/me/usage?days=30
GET  /v1/me/ledger
```

`services/Sessions.ts` sends a one-time magic link with a 15-minute token,
stores only its hash, and creates a 30-day `retsu_session` cookie after
verification. The dashboard can create a key that is shown once and revoke it.
The session and key lifecycle is source-backed; account access and email
delivery still need their runtime evidence.

## Credits and Stripe

`services/Ledger.ts` calls the per-account `AccountLedger` Durable Object for
rate limits, balance, holds, releases, settlements, and idempotent credits.
D1 mirrors ledger rows for account history. The Durable Object is the hot-path
owner. D1 is the query and mirror store.

`services/Billing.ts` creates a hosted Stripe Checkout Session. The browser
receives a redirect URL, so card data does not pass through the Worker. The
Stripe webhook verifies its signature and timestamp, accepts completed payment
events, and credits the account once using the Checkout Session id and recorded
event ids. A cancelled checkout returns the user to the site without credit.
Webhook signature failure, stale events, unpaid sessions, duplicate events,
backend failure, reserve failure, release, and settlement failure remain
distinct outcomes.

Useful source routes are:

```text
GET  /v1/billing/packs
POST /v1/billing/checkout {"pack":"starter"}
POST /v1/billing/webhook
```

The source includes test coverage for duplicate webhook delivery and billing
configuration. That does not prove a live Stripe account, hosted checkout,
settled money, or customer acceptance.

## QQ usage metadata

QQ headless records can carry optional `billing.cents` metadata. That field is
not the Retsu credit ledger and does not charge an account. See [Headless](headless.md)
for QQ's local contract and [Providers](providers.md) for provider credentials.
