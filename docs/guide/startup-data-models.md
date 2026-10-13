# Data and models

QQ, Retsu Grip, and the reviewed data platform own different records and
lifecycles. Keep their source and acceptance claims separate.

## QQ runtime data

QQ owns sessions, events, runs, transcripts, configuration, trust state, and
provider credentials. The TUI and native clients consume bounded snapshots;
they do not become a second transcript authority. See [Concepts](concepts.md),
[Configuration](configuration.md), and [Install](install.md) for the released
store and backup procedures.

The assistant candidate owns project bindings, scoped memory, drafts, pending
intent, and recovery metadata in its private SQLite store. It does not own QQ
credentials or copy the authoritative QQ transcript.

## Grip model and API data

The private Retsu source pin is `8d58080829231bb74e180c6de4e8f687c4606200`, refreshed on 2026-10-13. Repository access is required. The earlier `b889cd9e531438e4c54b25aa5ff9dc281c5e6888` snapshot is historical. The current source separates request text and schema, model output,
usage, API keys, accounts, credits, and billing ledger records. `grip serve`
is a stateless model backend. The gateway owns customer identity, rate limits,
credits, and usage. [The Retsu API reference](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/docs/reference/api.md)
requires repository access and describes the request and response contract.

## Platform2 local workflow

The separately owned platform2 candidate records a local reviewed workflow in
`docs/09-local-reviewed-data-experiment.md` and maps its features in
`docs/LOCAL-MODEL-LOOP-FEATURE-MAP.md`. Its source covers dataset identity,
annotation inputs, manifest-backed release, provenance, replay, validation,
backup, and recovery receipts. The companion source includes multimodal
audio, vision, and text readiness records plus local workflow videos and
receipts. These files are private source evidence and require repository
access.

Platform2 is not the same source as `sys-0` or `platform`. Those repositories
provide reused references and comparison material. Their presence does not
prove that a hosted labeling product, model registry, API, or customer
workflow is deployed.

## Model claims

A model name identifies a route or a release record. It does not prove that a
credential resolves, a provider accepts a request, a dataset is published, or
an evaluation passed on the current source. Report the source revision,
dataset or model identity, execution environment, input, expected result, and
observed result separately.
