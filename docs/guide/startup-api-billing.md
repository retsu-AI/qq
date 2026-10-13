# API and billing

QQ's local runtime and the Retsu extraction API have different ownership and
billing boundaries.

## QQ usage

QQ's headless protocol can attach optional usage and billing metadata to a
record. The current source includes a `billing.cents` field in the headless
record shape. That field is metadata for a run. It does not create a customer
account, charge a card, settle credits, or prove a paid journey.

QQ provider credentials remain under QQ's credential boundary. Review
[Providers and credentials](providers.md) for supported local provider setup.

## Retsu extraction API

The Retsu source design describes a public gateway and a private `grip serve`
backend. The gateway handles customer API keys, rate limits, credits, and usage
records. The backend performs stateless extraction and receives a backend
secret. The documented request is `POST /v1/extract`, with `GET /v1/models` and
`GET /v1/health` for model and health information.

That API description is source documentation in a separate private repository.
The local Retsu checkout is not the current remote main revision. It is not
evidence of a deployed endpoint, valid account, live key, charged request, or
customer acceptance. Repository access is required for the implementation.

## Billing acceptance

Treat these as separate checks: an API request, an authenticated request, a
credit reservation, a model response, a usage record, settlement, and a
customer-visible receipt. A source schema or test fixture cannot replace a
useful authorized paid-provider workflow.

Do not put credentials, account identifiers, private payment records, or raw
provider responses in this site. For QQ's local paid-provider limits, see
[Troubleshooting](troubleshooting.md) and [Headless](headless.md).
